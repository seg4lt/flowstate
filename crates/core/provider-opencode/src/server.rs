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

        let mut child = Command::new(binary)
            .args([
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
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("failed to spawn `{binary} serve`: {e}"))?;

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
}

impl Drop for OpenCodeServer {
    fn drop(&mut self) {
        // Best-effort tear-down. `kill_on_drop(true)` on the Command
        // handles the common case automatically, but we do an explicit
        // `start_kill()` through the mutex too so the child gets
        // signalled even if the tokio reactor is on its way out.
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
