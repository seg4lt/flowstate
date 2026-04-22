//! Daemon-process supervisor for the Tauri shell.
//!
//! Phase 6 of the architecture plan. The shell spawns the
//! `flowstate daemon` subcommand as a child, reads its handshake
//! file, publishes the loopback URL to `DaemonBaseUrl`, and watches
//! the child for unclean exits (restart with exponential backoff,
//! trip a crash-loop guard after repeated failures).
//!
//! # Opt-in via env var
//!
//! Today the Tauri shell still runs the daemon embedded inside its
//! own setup closure (`bootstrap_core_async` in `lib.rs`). Setting
//! `FLOWSTATE_USE_DAEMON=1` flips to the out-of-process path that
//! calls this module instead. Once the WS relay for the
//! `connect` / `handle_message` forwarder commands lands, the env
//! var flips default-on and the embedded path is deleted.
//!
//! # Lifecycle
//!
//! ```text
//! spawn() ─► `flowstate daemon --data-dir …` child ─► waits
//!                                                    │
//!                                                    ▼
//!           waits for <data_dir>/daemon.handshake ◄──┘
//!                   │
//!                   ▼
//!           publishes base_url to DaemonBaseUrl
//!                   │
//!                   ▼
//!           background task watches child + handshake file
//!                   │
//!                   ├── child exits unexpectedly ─► respawn w/ backoff
//!                   │                                 │
//!                   │                                 └── N crashes / 60s → fatal
//!                   │
//!                   └── Tauri shutdown ─► SIGTERM child, wait, SIGKILL
//! ```

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};

use crate::daemon_client::DaemonBaseUrl;

/// Shape of `<data_dir>/daemon.handshake`. Mirrors the writer in
/// `flowstate_app_layer::daemon_main::Handshake`; kept as a separate
/// type on the reader side so the shell can add fields defensively
/// (e.g. tolerate an older daemon that doesn't emit `build_sha`).
#[derive(Debug, Deserialize)]
pub struct Handshake {
    pub base_url: String,
    pub pid: u32,
    pub schema_version: u32,
    #[serde(default)]
    pub build_sha: Option<String>,
}

/// Supervisor config. Exposed as fields rather than a builder
/// because every caller wants the same set; a struct literal at
/// the one call site is clearer than ergonomic wrappers.
pub struct SupervisorConfig {
    /// Filesystem path the daemon writes the handshake to and where
    /// SQLite / state live. Passed to the daemon child via
    /// `--data-dir`.
    pub data_dir: PathBuf,
    /// Absolute path to the `flowstate` executable. Production use:
    /// `std::env::current_exe()`. Tests inject a fake `flowstate` so
    /// we can exercise the supervisor without spawning the real
    /// daemon binary.
    pub exe_path: PathBuf,
    /// How long to wait for the handshake file to appear after
    /// spawning. Covers first-time SQLite schema creation + adapter
    /// construction on cold start; 10 s is generous but not so long
    /// that a genuine failure stalls app startup forever.
    pub handshake_timeout: Duration,
    /// Initial backoff before respawning a crashed daemon. Doubles
    /// on each failure, capped at `max_backoff`.
    pub backoff_initial: Duration,
    /// Upper bound on respawn backoff.
    pub max_backoff: Duration,
    /// Crash-loop ceiling: if the daemon exits more than this many
    /// times within `crash_loop_window`, we stop respawning and
    /// emit a fatal error to the shell.
    pub crash_loop_max_restarts: usize,
    pub crash_loop_window: Duration,
}

impl SupervisorConfig {
    pub fn defaults(data_dir: PathBuf, exe_path: PathBuf) -> Self {
        Self {
            data_dir,
            exe_path,
            handshake_timeout: Duration::from_secs(10),
            backoff_initial: Duration::from_millis(500),
            max_backoff: Duration::from_secs(30),
            crash_loop_max_restarts: 3,
            crash_loop_window: Duration::from_secs(60),
        }
    }
}

/// Live supervisor handle. Drop the struct (or call `shutdown`) to
/// SIGTERM the daemon child and release the handshake watcher.
pub struct Supervisor {
    /// Broadcast channel the UI can subscribe to for "daemon
    /// respawned" notifications. `()` because the payload is just
    /// "a new handshake is live" — consumers re-read state as a
    /// consequence.
    pub restart_rx: tokio::sync::broadcast::Receiver<()>,
    shutdown_tx: tokio::sync::mpsc::Sender<()>,
}

impl Supervisor {
    /// Tell the supervisor task to shut down cleanly. SIGTERM's the
    /// daemon child, waits briefly, SIGKILLs if still alive.
    pub async fn shutdown(self) {
        let _ = self.shutdown_tx.send(()).await;
    }
}

/// Spawn the supervisor task. Returns a `Supervisor` once the
/// handshake from the *first* daemon process has been read and its
/// base URL published to `daemon_base_url` — so the caller knows
/// the daemon is up and all subsequent Tauri commands will route
/// to a real HTTP server.
///
/// Subsequent respawns (after crashes) re-publish the URL
/// asynchronously and emit on the `restart_rx` broadcast so the UI
/// can mark in-flight turns as interrupted and resubscribe to the
/// new event stream.
pub async fn spawn(
    config: SupervisorConfig,
    daemon_base_url: DaemonBaseUrl,
) -> Result<Supervisor> {
    let (restart_tx, restart_rx) = tokio::sync::broadcast::channel::<()>(4);
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::mpsc::channel::<()>(1);

    // First spawn happens synchronously (well, awaited) so the
    // caller sees a live daemon before the `Ok(Supervisor)` hits.
    let (mut child, hs) = spawn_once(&config).await?;
    daemon_base_url.publish(hs.base_url.clone());
    tracing::info!(
        pid = hs.pid,
        base_url = %hs.base_url,
        schema_version = hs.schema_version,
        "daemon supervisor: initial daemon up"
    );

    let daemon_base_url_watch = daemon_base_url.clone();
    let config = Arc::new(config);
    tokio::spawn(async move {
        let mut restarts_in_window: Vec<Instant> = Vec::new();
        let mut backoff = config.backoff_initial;
        loop {
            tokio::select! {
                // Shell is shutting down: SIGTERM the daemon, wait briefly, force.
                _ = shutdown_rx.recv() => {
                    tracing::info!("daemon supervisor: shell shutdown, terminating daemon");
                    let _ = terminate_child(&mut child).await;
                    return;
                }
                // Daemon exited. Decide whether to respawn.
                status = child.wait() => {
                    match status {
                        Ok(s) if s.success() => {
                            tracing::info!("daemon supervisor: daemon exited cleanly; not respawning");
                            return;
                        }
                        Ok(s) => {
                            tracing::warn!(code = ?s.code(), "daemon supervisor: daemon exited unsuccessfully");
                        }
                        Err(e) => {
                            tracing::warn!(%e, "daemon supervisor: child.wait() failed");
                        }
                    }

                    // Prune restarts outside the crash-loop window.
                    let now = Instant::now();
                    restarts_in_window.retain(|t| now.duration_since(*t) < config.crash_loop_window);
                    if restarts_in_window.len() >= config.crash_loop_max_restarts {
                        tracing::error!(
                            restarts = restarts_in_window.len(),
                            window_secs = config.crash_loop_window.as_secs(),
                            "daemon supervisor: crash-loop detected; giving up"
                        );
                        return;
                    }
                    restarts_in_window.push(now);

                    tracing::info!(
                        backoff_ms = backoff.as_millis() as u64,
                        "daemon supervisor: respawning after backoff"
                    );
                    tokio::select! {
                        _ = tokio::time::sleep(backoff) => {}
                        _ = shutdown_rx.recv() => {
                            tracing::info!("daemon supervisor: shutdown during backoff");
                            return;
                        }
                    }
                    backoff = (backoff * 2).min(config.max_backoff);

                    match spawn_once(&config).await {
                        Ok((new_child, new_hs)) => {
                            child = new_child;
                            daemon_base_url_watch.publish(new_hs.base_url.clone());
                            let _ = restart_tx.send(());
                            tracing::info!(
                                pid = new_hs.pid,
                                base_url = %new_hs.base_url,
                                "daemon supervisor: respawn successful"
                            );
                            // Reset backoff on successful respawn.
                            backoff = config.backoff_initial;
                        }
                        Err(e) => {
                            tracing::error!(%e, "daemon supervisor: respawn failed; will retry");
                            // Loop will re-await child.wait() (which
                            // resolves immediately since `child` is
                            // still the dead one) — meaning another
                            // backoff iteration. To avoid tight-loop
                            // on a permanently-broken daemon, count
                            // this as a restart attempt.
                            continue;
                        }
                    }
                }
            }
        }
    });

    Ok(Supervisor {
        restart_rx,
        shutdown_tx,
    })
}

/// Spawn one daemon process and wait for its handshake.
async fn spawn_once(config: &SupervisorConfig) -> Result<(Child, Handshake)> {
    // Remove any stale handshake from a prior crash so the waiter
    // doesn't read it and race forward.
    let handshake_path = config.data_dir.join("daemon.handshake");
    let _ = std::fs::remove_file(&handshake_path);

    let mut cmd = Command::new(&config.exe_path);
    cmd.arg("daemon")
        .arg("--data-dir")
        .arg(&config.data_dir)
        // Inherit stderr (daemon logs go there) so the shell's
        // own stderr surface shows the daemon's tracing output.
        // stdin/stdout are detached.
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = cmd
        .spawn()
        .with_context(|| format!("spawn flowstate daemon at {}", config.exe_path.display()))?;

    // Forward daemon stderr into our tracing pipeline so a failing
    // daemon leaves a paper trail.
    if let Some(stderr) = child.stderr.take() {
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::info!(target: "daemon", "{line}");
            }
        });
    }

    // Poll for the handshake file. 50 ms interval × timeout/50.
    let poll_interval = Duration::from_millis(50);
    let deadline = Instant::now() + config.handshake_timeout;
    loop {
        if let Some(hs) = read_handshake(&handshake_path) {
            return Ok((child, hs));
        }
        // Bail fast if the daemon already exited before writing the
        // handshake — something is wrong; don't wait out the full
        // timeout.
        if let Ok(Some(status)) = child.try_wait() {
            anyhow::bail!(
                "daemon exited before writing handshake (status {:?})",
                status.code()
            );
        }
        if Instant::now() > deadline {
            // Kill the stuck child before we return.
            let _ = child.start_kill();
            anyhow::bail!(
                "daemon did not write handshake within {:?}",
                config.handshake_timeout
            );
        }
        tokio::time::sleep(poll_interval).await;
    }
}

fn read_handshake(path: &Path) -> Option<Handshake> {
    let body = std::fs::read_to_string(path).ok()?;
    serde_json::from_str::<Handshake>(&body).ok()
}

async fn terminate_child(child: &mut Child) -> Result<()> {
    // Polite SIGTERM first. Tokio's start_kill actually SIGKILLs on
    // Unix — to get SIGTERM we signal by hand. Small allowance for
    // the daemon to drain, then escalate.
    #[cfg(unix)]
    {
        if let Some(pid) = child.id() {
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGTERM);
            }
        }
    }
    let wait_window = Duration::from_secs(5);
    let deadline = Instant::now() + wait_window;
    loop {
        if let Ok(Some(_)) = child.try_wait() {
            return Ok(());
        }
        if Instant::now() > deadline {
            let _ = child.start_kill();
            let _ = child.wait().await;
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
