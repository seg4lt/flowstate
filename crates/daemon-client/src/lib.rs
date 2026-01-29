//! ZenUI daemon client.
//!
//! A small, dependency-lean crate every frontend (tao-web-shell, a future
//! Tauri shell, a future CLI, a future GPUI shell) uses to locate — or
//! auto-spawn — a running `zenui-server` and attach to its HTTP + WS
//! endpoints. Deliberately does *not* depend on `daemon-core`, so building
//! a shell does not drag in the entire runtime + provider + SQLite stack.
//!
//! The coordination primitives — ready file format, ready file path
//! resolution — are duplicated from `daemon-core` (see `ready_file.rs`).

mod ready_file;
mod spawn;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

pub use ready_file::{ReadyFile, ReadyFileContent};

/// What a connected client needs to talk to the daemon.
#[derive(Debug, Clone)]
pub struct DaemonHandle {
    pub http_base: String,
    pub ws_url: String,
    pub pid: u32,
}

#[derive(Debug, Clone)]
pub struct ClientConfig {
    /// Canonical project root. The ready file and spawn lock are keyed by
    /// a hash of this path, so different projects run independent daemons.
    pub project_root: PathBuf,
    /// Override for `zenui-server` binary discovery (tests and dev).
    pub server_binary: Option<PathBuf>,
    /// Total time budget for waiting on the ready file after spawning.
    pub spawn_timeout: Duration,
    /// Per-call timeout for `/api/health` probes.
    pub health_timeout: Duration,
}

impl ClientConfig {
    /// Build a config rooted at the current working directory.
    pub fn for_current_project() -> Result<Self> {
        let cwd = std::env::current_dir().context("resolve current working directory")?;
        let project_root = std::fs::canonicalize(&cwd)
            .with_context(|| format!("canonicalize {}", cwd.display()))?;
        Ok(Self {
            project_root,
            server_binary: None,
            spawn_timeout: Duration::from_secs(10),
            health_timeout: Duration::from_millis(500),
        })
    }
}

/// Connect to a running daemon or spawn one and wait for it to become
/// ready. Race-safe against concurrent callers: a `fs4` advisory lock
/// serializes the spawn attempt, and the ready file is re-read after the
/// lock is acquired so a caller that lost the race returns cleanly.
pub fn connect_or_spawn(config: &ClientConfig) -> Result<DaemonHandle> {
    let ready = ReadyFile::for_project(&config.project_root)?;

    // Happy path: existing ready file + health check OK.
    if let Some(content) = ready.read()? {
        if health_check(&content.http_base, config.health_timeout).is_ok() {
            return Ok(DaemonHandle {
                http_base: content.http_base,
                ws_url: content.ws_url,
                pid: content.pid,
            });
        }
        // Stale file — daemon died without cleaning up. Remove so the
        // spawn branch below doesn't trip over it.
        let _ = ready.delete();
    }

    // Take the advisory lock so only one client spawns at a time.
    let lock = spawn::acquire_spawn_lock(&config.project_root)
        .context("acquire zenui-server spawn lock")?;

    // Re-read under the lock — another client may have won the race while
    // we were blocked waiting on it.
    if let Some(content) = ready.read()? {
        if health_check(&content.http_base, config.health_timeout).is_ok() {
            drop(lock);
            return Ok(DaemonHandle {
                http_base: content.http_base,
                ws_url: content.ws_url,
                pid: content.pid,
            });
        }
        let _ = ready.delete();
    }

    // We own the spawn. Kick off zenui-server; its own `start` subcommand
    // handles detachment and polls its ready file before returning.
    spawn::spawn_daemon(config)?;
    drop(lock);

    // Poll until the ready file appears AND the health check passes. The
    // zenui-server binary should already have done this waiting for us,
    // but we re-check here in case of unexpected timing or a stale file.
    let deadline = Instant::now() + config.spawn_timeout;
    loop {
        if let Some(content) = ready.read()? {
            if health_check(&content.http_base, config.health_timeout).is_ok() {
                return Ok(DaemonHandle {
                    http_base: content.http_base,
                    ws_url: content.ws_url,
                    pid: content.pid,
                });
            }
        }
        if Instant::now() >= deadline {
            bail!(
                "timed out waiting for zenui-server to become ready for project {}",
                config.project_root.display()
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn health_check(http_base: &str, timeout: Duration) -> Result<()> {
    let url = format!("{}/api/health", http_base);
    ureq::get(&url)
        .timeout(timeout)
        .call()
        .with_context(|| format!("GET {} failed", url))?;
    Ok(())
}
