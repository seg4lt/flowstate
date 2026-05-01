//! Child-process lifecycle for `opencode serve`.
//!
//! The opencode binary ships a headless HTTP server that multiplexes
//! many sessions behind one process. This module owns one such
//! server per flowstate daemon — allocating a local port, generating
//! a throwaway password for the server's basic-auth guard, waiting
//! for its readiness line on stderr, and tearing the child down
//! cleanly when the adapter is dropped.

use std::net::TcpListener;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use rand::RngCore;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tokio::time::timeout;
use tracing::{debug, info, warn};

use crate::http::OpenCodeClient;

/// Substring we scan for on opencode's stdout to confirm the HTTP
/// listener is up and accepting requests. Opencode prints a line
/// matching `opencode server listening on http://127.0.0.1:PORT` on
/// **stdout** once the server is ready. We match a stable prefix so
/// minor log-format tweaks upstream don't hang the adapter's
/// bootstrap (e.g. a future "server listening on ..." without the
/// "opencode " prefix would still match).
const READINESS_MARKER: &str = "listening";

/// Long-lived handle to an `opencode serve` child process plus the
/// pre-built REST client that talks to it.
pub struct OpenCodeServer {
    url: String,
    #[allow(dead_code)] // retained so the basic-auth value is inspectable via logs if needed
    password: String,
    client: Arc<OpenCodeClient>,
    /// Guards the child so only one tear-down path can fire. Wrapped
    /// in an `Option` so `shutdown()` can take ownership of the handle
    /// and join it without leaving the mutex in a moved-out state.
    child: Mutex<Option<Child>>,
    /// Cross-platform process-group / Job-Object handle owning the
    /// `opencode serve` subtree. Stored separately from `child` so
    /// Drop / shutdown can signal the whole group even if the child
    /// handle was already moved out of the mutex by `shutdown()`.
    /// On Unix it carries a pgid (= child pid); on Windows it owns
    /// a Job Object handle with `KILL_ON_JOB_CLOSE`. See
    /// `zenui_provider_api::ProcessGroup`.
    process_group: zenui_provider_api::ProcessGroup,
}

impl OpenCodeServer {
    /// Pick a free localhost port, spawn `opencode serve` bound to it,
    /// wait for the readiness line, return a live handle.
    pub async fn spawn(
        binary: &str,
        working_directory: &Path,
        startup_timeout: Duration,
    ) -> Result<Self, String> {
        let port = pick_free_port()?;
        let password = random_password();
        let hostname = "127.0.0.1";

        info!(
            %port,
            binary,
            cwd = %working_directory.display(),
            "spawning opencode serve"
        );

        let mut cmd = Command::new(binary);
        cmd.args(["serve", "--hostname", hostname, "--port", &port.to_string()])
            .current_dir(working_directory)
            // Augment PATH so opencode and anything it forks (git,
            // its own MCP subprocesses) sees the user's configured
            // extra search dirs.
            .env("PATH", zenui_provider_api::path_with_extras(&[]))
            // Opencode's server reads this to gate requests. We
            // regenerate it on every spawn so a lingering stale
            // process from a previous crash can't answer for us.
            .env("OPENCODE_SERVER_PASSWORD", &password)
            // Keep the default username — opencode's docs default to
            // `"opencode"` when `OPENCODE_SERVER_USERNAME` is unset,
            // and we match that in the HTTP client.
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_ASKPASS", "")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);

        // Put opencode in its own process group / Job Object so
        // shutdown can kill the whole subtree atomically. Unix
        // `setpgid(0, 0)` runs in pre_exec; Windows creates a Job
        // Object now and the spawned child is assigned to it
        // post-spawn via `process_group.attach(&child)` below. Either
        // way, grandchildren opencode forks (per-session agent
        // workers, its own mcp-server subprocesses) inherit the
        // group, and one `kill_best_effort` call reaps the whole
        // tree.
        //
        // Without this, a SIGKILL of flowstate would trigger
        // `kill_on_drop` only on the immediate `opencode serve` PID;
        // children opencode had spawned would reparent to PID 1 (or
        // become orphans on Windows) and leak.
        let mut process_group = zenui_provider_api::ProcessGroup::before_spawn(&mut cmd);

        let mut child = cmd
            .spawn()
            .map_err(|e| format!("failed to spawn `{binary} serve`: {e}"))?;

        // Bind the spawned child to the group. On Unix the pgid
        // equals the child pid (we set it in pre_exec). On Windows
        // this calls `AssignProcessToJobObject` against the Job
        // Object created above. Both are best-effort; if attach
        // fails the struct degrades to tokio's `kill_on_drop`
        // behaviour for the direct child only.
        process_group.attach(&child);

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "opencode serve stdout unavailable".to_string())?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| "opencode serve stderr unavailable".to_string())?;

        // Bridge both streams into the daemon's tracing pipeline so
        // crashes and warnings from opencode show up next to our own
        // logs. Readiness detection watches **stdout** — opencode
        // emits its `opencode server listening on http://…` banner
        // there, not on stderr (stderr carries misconfiguration and
        // runtime error noise).
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(drain_stream("opencode.stdout", stdout, Some(ready_tx)));
        tokio::spawn(drain_stream("opencode.stderr", stderr, None));

        match timeout(startup_timeout, ready_rx).await {
            Ok(Ok(())) => {
                debug!("opencode serve readiness line received");
            }
            Ok(Err(_)) => {
                // Sender dropped before signalling. Usually means the
                // child exited before it ever printed a readiness
                // line — bubble up something actionable instead of a
                // cryptic "oneshot cancelled".
                let _ = child.start_kill();
                return Err(
                    "opencode serve exited before signalling readiness — check the \
                     daemon log for its stderr"
                        .to_string(),
                );
            }
            Err(_) => {
                let _ = child.start_kill();
                return Err(format!(
                    "opencode serve did not become ready within {}s",
                    startup_timeout.as_secs()
                ));
            }
        }

        let url = format!("http://{hostname}:{port}");
        let client = Arc::new(OpenCodeClient::new(url.clone(), password.clone()));

        Ok(Self {
            url,
            password,
            client,
            child: Mutex::new(Some(child)),
            process_group,
        })
    }

    /// Base URL of the running server, e.g. `http://127.0.0.1:49192`.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Cloneable HTTP client wired to this server's URL + password.
    pub fn client(&self) -> Arc<OpenCodeClient> {
        self.client.clone()
    }

    /// Async, cooperative teardown. Preferred over relying on `Drop`
    /// for the Phase-B idle-kill path because:
    ///
    /// - `Drop` must be sync, so it uses `try_lock` on the child
    ///   mutex and skips tear-down if something else is holding it.
    ///   An async `shutdown()` can `.lock().await` and is guaranteed
    ///   to consume the child handle.
    /// - Idle-kill wants to `await` the child's actual exit so the
    ///   next spawn doesn't race against the previous port being
    ///   released. `Drop` can't await.
    ///
    /// Order of operations mirrors `Drop` for consistency:
    ///   1. `killpg(pgid, SIGTERM)` — polite signal to the whole
    ///      opencode process group (server + its MCP subprocess +
    ///      any per-session workers).
    ///   2. Take the child handle under the mutex and await its
    ///      exit, bounded by a short timeout.
    ///   3. If the child is still alive past the timeout, escalate
    ///      via tokio's `start_kill` (maps to SIGKILL on Unix).
    ///
    /// Live in Phase B: the idle watcher
    /// (`crates/core/provider-opencode/src/lib.rs` `IdleWatcher`) and
    /// the daemon-wide graceful shutdown path
    /// (`OpenCodeAdapter::shutdown` →
    /// `daemon-core/src/shutdown.rs::graceful_shutdown`) both call
    /// this. The sync `Drop` impl below remains as a belt-and-braces
    /// fallback for paths that bypass graceful shutdown entirely
    /// (hard aborts, SIGKILL).
    pub async fn shutdown(&self) {
        // Polite signal to the whole opencode subtree. Unix sends
        // SIGTERM to the process group; Windows TerminateJobObject's
        // every member of the Job Object. ESRCH / "already reaped"
        // are silently ignored either way.
        self.process_group.kill_best_effort();
        let mut guard = self.child.lock().await;
        if let Some(mut child) = guard.take() {
            // Bounded wait. 3s matches what opencode's own
            // signal-handler path takes to flush state on a SIGTERM —
            // generous but not so long it lets a wedged server block
            // the idle-kill path.
            let wait_result = tokio::time::timeout(Duration::from_secs(3), child.wait()).await;
            if wait_result.is_err() {
                warn!("opencode serve did not exit within 3s of SIGTERM; escalating to SIGKILL");
                let _ = child.start_kill();
                // One more bounded wait so the caller doesn't return
                // while the child is still mid-reap.
                let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
            }
        }
    }
}

impl Drop for OpenCodeServer {
    fn drop(&mut self) {
        // Tear down the *whole* opencode subtree, not just the direct
        // child. Opencode spawns its own grandchildren (per-session
        // agent workers, the flowstate mcp-server subprocess we
        // registered in opencode.json). Killing only the parent leaks
        // grandchildren — we've observed ~22 orphan `flowstate
        // mcp-server` processes accumulating during `tauri dev`
        // restart cycles.
        //
        // Order of operations:
        //   1. `killpg(pgid, SIGTERM)` — polite signal to the whole
        //      group, gives opencode a chance to flush writes.
        //   2. `start_kill()` on the direct child — redundant on the
        //      leader but ensures tokio's internal state reflects that
        //      the child is on its way out.
        // We deliberately do NOT `killpg(SIGKILL)`: SIGTERM followed
        // by the kernel reaping on actual exit is the clean path.
        // Hard SIGKILL escalation lives in the startup orphan scan
        // (runs before the next flowstate binds its port, so any
        // stubborn survivors get reaped there).
        // Polite signal to the whole subtree (SIGTERM on Unix,
        // TerminateJobObject on Windows). ESRCH / "already reaped"
        // are silently ignored. The Drop impl on `process_group`
        // itself runs after this and is a belt-and-braces no-op on
        // Unix / a CloseHandle on Windows (which kills any survivors
        // again via JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE).
        self.process_group.kill_best_effort();
        if let Ok(mut guard) = self.child.try_lock() {
            if let Some(mut child) = guard.take() {
                let _ = child.start_kill();
            }
        }
    }
}

/// Ask the kernel for a free TCP port by binding `127.0.0.1:0` and
/// immediately releasing it. There is a small race window between
/// the release and opencode's bind, but it's the same technique used
/// by just about every test harness that needs a free port and has
/// been stable in practice.
fn pick_free_port() -> Result<u16, String> {
    let listener = TcpListener::bind("127.0.0.1:0")
        .map_err(|e| format!("could not allocate a local port for opencode: {e}"))?;
    let port = listener
        .local_addr()
        .map_err(|e| format!("could not read allocated port: {e}"))?
        .port();
    drop(listener);
    Ok(port)
}

/// Generate a 32-byte hex password. Scoped only to the running
/// opencode child, never persisted, regenerated on every spawn.
fn random_password() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Forward a child stream line-by-line into the daemon log. Optionally
/// fires a oneshot the first time a line containing `READINESS_MARKER`
/// appears, which is how [`OpenCodeServer::spawn`] knows the HTTP
/// listener is accepting requests.
async fn drain_stream<R>(
    target: &'static str,
    reader: R,
    mut ready_tx: Option<tokio::sync::oneshot::Sender<()>>,
) where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    let mut lines = BufReader::new(reader).lines();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }

                // Readiness probe runs once — the line that flips the
                // server to ready is interesting enough to log at info,
                // both as a confirmation signal and so a stuck startup
                // (line never appears) is obvious without re-running
                // with `RUST_LOG=debug`.
                if let Some(tx) = ready_tx.take() {
                    if trimmed.to_ascii_lowercase().contains(READINESS_MARKER) {
                        info!(
                            target: "provider-opencode",
                            stream = target,
                            "{trimmed}"
                        );
                        let _ = tx.send(());
                        continue;
                    } else {
                        // Pre-readiness chatter. Surface at info too
                        // so `did not become ready` failures leave a
                        // paper trail of what opencode *did* print
                        // before giving up.
                        info!(
                            target: "provider-opencode",
                            stream = target,
                            "{trimmed}"
                        );
                        ready_tx = Some(tx);
                        continue;
                    }
                }

                // Post-readiness or non-readiness stream: debug-level
                // so a running opencode server doesn't spam the log.
                debug!(target: "provider-opencode", stream = target, "{trimmed}");
            }
            Ok(None) => break,
            Err(err) => {
                warn!(stream = target, "error reading opencode output: {err}");
                break;
            }
        }
    }
}
