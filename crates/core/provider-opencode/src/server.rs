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
    /// Process-group id that `opencode serve` leads. Captured right
    /// after spawn — we pass `.process_group(0)` so the child becomes
    /// its own group leader (pgid == child pid). Stored separately so
    /// Drop / shutdown can `killpg(pgid, SIGTERM)` even if the child
    /// handle was already moved out of the mutex.
    ///
    /// `None` on non-Unix builds (no process-group concept) or if the
    /// child died before we could read its pid. Both cases degrade to
    /// the tokio `kill_on_drop` behaviour.
    child_pgid: Option<i32>,
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
        cmd.args([
            "serve",
            "--hostname",
            hostname,
            "--port",
            &port.to_string(),
        ])
        .current_dir(working_directory)
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

        // Put opencode in its own process group so shutdown can kill
        // the whole subtree atomically. `process_group(0)` tells the
        // kernel to `setpgid(child_pid, child_pid)` after fork — the
        // child becomes group leader and any grandchildren it spawns
        // (per-session agent workers, its own mcp-server subprocesses)
        // inherit the group. One `killpg(pgid, SIGTERM)` later
        // reaps everything; the `Drop` path below uses exactly that.
        //
        // Without this, a SIGKILL of flowstate would trigger
        // `kill_on_drop` only on the immediate `opencode serve` PID;
        // children opencode had spawned would reparent to PID 1 and
        // leak. (In practice opencode is good about tearing down its
        // own children, but relying on that is fragile — the belt is
        // cheap.) See Phase 2.5.c in the plan.
        // tokio's `Command::pre_exec` is Unix-only; the `#[cfg(unix)]`
        // gate keeps non-Unix builds compiling by simply skipping the
        // pre-exec hook.
        #[cfg(unix)]
        // Safety: `setpgid(0, 0)` is an async-signal-safe syscall
        // and is the documented pre-exec use case.
        unsafe {
            cmd.pre_exec(|| {
                if libc::setpgid(0, 0) == 0 {
                    Ok(())
                } else {
                    Err(std::io::Error::last_os_error())
                }
            });
        }

        let mut child = cmd
            .spawn()
            .map_err(|e| format!("failed to spawn `{binary} serve`: {e}"))?;

        // Capture pgid now — after `setpgid(0,0)` in pre_exec the
        // child's pid equals its pgid, and `child.id()` is Some until
        // the child is awaited. `Option<u32> → Option<i32>` with an
        // explicit `try_into` fall-back so stub builds on unsupported
        // platforms compile cleanly.
        #[cfg(unix)]
        let child_pgid: Option<i32> = child.id().and_then(|p| i32::try_from(p).ok());
        #[cfg(not(unix))]
        let child_pgid: Option<i32> = None;

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
        tokio::spawn(drain_stream(
            "opencode.stdout",
            stdout,
            Some(ready_tx),
        ));
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
            child_pgid,
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
        #[cfg(unix)]
        if let Some(pgid) = self.child_pgid {
            // Safety: `killpg` with a valid pgid is a standard POSIX
            // syscall. ESRCH (already reaped) is the expected outcome
            // on a clean shutdown and silently ignored.
            unsafe {
                libc::killpg(pgid, libc::SIGTERM);
            }
        }
        let mut guard = self.child.lock().await;
        if let Some(mut child) = guard.take() {
            // Bounded wait. 3s matches what opencode's own
            // signal-handler path takes to flush state on a SIGTERM —
            // generous but not so long it lets a wedged server block
            // the idle-kill path.
            let wait_result = tokio::time::timeout(
                Duration::from_secs(3),
                child.wait(),
            )
            .await;
            if wait_result.is_err() {
                warn!("opencode serve did not exit within 3s of SIGTERM; escalating to SIGKILL");
                let _ = child.start_kill();
                // One more bounded wait so the caller doesn't return
                // while the child is still mid-reap.
                let _ = tokio::time::timeout(
                    Duration::from_secs(2),
                    child.wait(),
                )
                .await;
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
        #[cfg(unix)]
        if let Some(pgid) = self.child_pgid {
            // Safety: `killpg` with a valid pgid is a standard POSIX
            // syscall. ESRCH (group already empty) is expected on
            // clean shutdown and silently ignored.
            unsafe {
                libc::killpg(pgid, libc::SIGTERM);
            }
        }
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
