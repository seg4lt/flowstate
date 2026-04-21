//! opencode provider adapter.
//!
//! Drives the opencode coding agent via its headless HTTP server
//! (`opencode serve`) with Server-Sent Events for streaming. The
//! server is spawned once per daemon on first use and shared across
//! every flowstate session that picks the opencode provider —
//! opencode's server is natively multi-session, so one process is
//! enough no matter how many chats the user has open.
//!
//! Module layout:
//! - [`server`] — child-process lifecycle: pick a free localhost
//!   port, generate a random password, spawn `opencode serve`, wait
//!   for its readiness line, tear down cleanly on drop.
//! - [`http`]   — thin `reqwest` wrappers over the REST endpoints we
//!   actually consume (session create, prompt, abort, health, model
//!   catalog, permission answers).
//! - [`events`] — SSE reader against `/event`; parses the JSON
//!   payloads and forwards per-session events to whichever
//!   `TurnEventSink` is currently active for that session id.
//!
//! The public entry point is [`OpenCodeAdapter`], constructed in the
//! host app's adapter vector alongside the other providers.

mod events;
mod http;
mod server;

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::{Mutex, OnceCell};
use tracing::{debug, warn};
use zenui_provider_api::{
    PermissionMode, ProviderAdapter, ProviderKind, ProviderModel, ProviderSessionState,
    ProviderStatus, ProviderStatusLevel, ProviderTurnOutput, ReasoningEffort, SessionDetail,
    ThinkingMode, TurnEventSink, UserInput, find_cli_binary,
};

use crate::events::EventRouter;
use crate::http::OpenCodeClient;
use crate::server::OpenCodeServer;

/// Install hint surfaced in `health()` when the `opencode` binary is
/// missing. Kept as a constant so the diagnostic message stays
/// consistent between the pre-flight probe and any runtime errors.
const OPENCODE_INSTALL_HINT: &str =
    "Install opencode. macOS: `brew install sst/tap/opencode`  \u{2022}  \
     Linux/macOS: `curl -fsSL https://opencode.ai/install | bash`  \u{2022}  \
     any platform: `npm i -g opencode-ai`";

/// The binary name we look for on PATH. opencode ships as `opencode`
/// on every platform (no `.cmd` shim on Windows today — the installer
/// drops `opencode.exe`).
const OPENCODE_BINARY: &str = "opencode";

/// Bound an initial server startup + health probe. Opencode's own
/// readiness log usually lands in <1s; we give generous headroom for
/// cold starts (npm-installed shims on Windows can take a few
/// seconds) but bail before a stuck probe blocks the daemon's
/// bootstrap flow.
const SERVER_STARTUP_TIMEOUT_SECS: u64 = 10;

/// Upper bound for a single turn. Opencode itself doesn't stop — the
/// model could loop on tools indefinitely — so the adapter enforces a
/// wall clock. Matches the `provider-claude-cli` ceiling so the UI's
/// "stuck?" banner kicks in at the same scale across providers.
const TURN_TIMEOUT_SECS: u64 = 600;

#[derive(Clone)]
pub struct OpenCodeAdapter {
    /// Working-directory fallback handed to the server process when a
    /// session doesn't carry its own `cwd`. Sessions attached to a
    /// project override this per-request via the REST API's
    /// `directory` parameter; the server's own cwd is only used as a
    /// last-resort default.
    working_directory: PathBuf,
    /// Lazy singleton: the first `execute_turn` / `health` that needs
    /// the server spawns it; subsequent calls reuse the same child.
    /// Wrapped in a `Mutex<OnceCell<…>>` so concurrent callers during
    /// a cold start block on the first spawn instead of racing it.
    server: Arc<OnceCell<Arc<OpenCodeServer>>>,
    /// Guards the one-time server spawn. `OnceCell::get_or_try_init`
    /// would do this too, but holding the lock lets us emit a single
    /// consolidated "opencode server starting…" log line from the
    /// winning task without interleaving from concurrent losers.
    server_init_lock: Arc<Mutex<()>>,
    /// Routes SSE events to the right session's sink. Lives at the
    /// adapter level because one SSE connection feeds every in-flight
    /// session — multiplexing happens here, not per-session.
    event_router: Arc<EventRouter>,
}

impl OpenCodeAdapter {
    pub fn new(working_directory: PathBuf) -> Self {
        Self {
            working_directory,
            server: Arc::new(OnceCell::new()),
            server_init_lock: Arc::new(Mutex::new(())),
            event_router: Arc::new(EventRouter::new()),
        }
    }

    /// Resolve the `opencode` binary. Returns `None` if nothing
    /// resembling opencode is on PATH or in the usual install
    /// locations; `health()` turns that into a user-visible
    /// "install opencode" hint rather than a cryptic spawn failure
    /// down the line.
    fn find_opencode_binary() -> Option<String> {
        find_cli_binary(OPENCODE_BINARY).map(|p| p.to_string_lossy().into_owned())
    }

    /// Ensure the server is running and return a handle. Callers that
    /// need the HTTP client should prefer [`Self::client`] which
    /// composes this with the auth handshake.
    async fn ensure_server(&self) -> Result<Arc<OpenCodeServer>, String> {
        // Fast path — server already initialised.
        if let Some(existing) = self.server.get() {
            return Ok(existing.clone());
        }

        // Slow path — serialise the first spawn so concurrent callers
        // don't race the child process startup. `OnceCell::get_or_init`
        // handles the actual once-ness; the extra mutex just keeps the
        // log noise sensible.
        let _guard = self.server_init_lock.lock().await;
        let server = self
            .server
            .get_or_try_init(|| async {
                let binary = Self::find_opencode_binary().ok_or_else(|| {
                    format!(
                        "opencode binary not found on PATH. {}",
                        OPENCODE_INSTALL_HINT
                    )
                })?;
                let server = OpenCodeServer::spawn(
                    &binary,
                    &self.working_directory,
                    std::time::Duration::from_secs(SERVER_STARTUP_TIMEOUT_SECS),
                )
                .await?;
                let server = Arc::new(server);
                // Kick the SSE reader once the server is live so the
                // first turn's events land in the router the moment
                // opencode starts streaming. `spawn_reader` is
                // idempotent — subsequent calls are cheap no-ops.
                self.event_router
                    .spawn_reader(server.clone(), server.client());
                Ok::<Arc<OpenCodeServer>, String>(server)
            })
            .await?;
        Ok(server.clone())
    }

    /// Convenience: get a ready-to-use HTTP client. Side effect: spins
    /// up the server if it isn't already running.
    async fn client(&self) -> Result<Arc<OpenCodeClient>, String> {
        let server = self.ensure_server().await?;
        Ok(server.client())
    }
}

#[async_trait]
impl ProviderAdapter for OpenCodeAdapter {
    fn kind(&self) -> ProviderKind {
        ProviderKind::OpenCode
    }

    /// Off by default. opencode is opt-in — users who don't have the
    /// binary installed shouldn't see a warning badge in Settings the
    /// first time they open the app. Mirrors how `provider-claude-cli`
    /// and the Codex CLI provider behave.
    fn default_enabled(&self) -> bool {
        false
    }

    async fn health(&self) -> ProviderStatus {
        let label = ProviderKind::OpenCode.label().to_string();
        let binary = match Self::find_opencode_binary() {
            Some(b) => b,
            None => {
                return ProviderStatus {
                    kind: ProviderKind::OpenCode,
                    label,
                    installed: false,
                    authenticated: false,
                    version: None,
                    status: ProviderStatusLevel::Error,
                    message: Some(format!(
                        "opencode CLI is not installed. {}",
                        OPENCODE_INSTALL_HINT
                    )),
                    models: Vec::new(),
                    enabled: true,
                    features: zenui_provider_api::ProviderFeatures::default(),
                };
            }
        };

        // Best-effort version probe. We run `opencode --version` ourselves
        // instead of going through the shared `probe_cli` helper because
        // `probe_cli` also wants an `auth` subcommand, and opencode's
        // auth story lives inside the running server (providers are
        // configured per-workspace via `opencode auth login`, not
        // globally). The server itself is the real authentication
        // proof, so we try to start it next.
        let version = match tokio::process::Command::new(&binary)
            .arg("--version")
            .output()
            .await
        {
            Ok(out) => zenui_provider_api::helpers::first_non_empty_line(&out.stdout)
                .or_else(|| zenui_provider_api::helpers::first_non_empty_line(&out.stderr)),
            Err(err) => {
                return ProviderStatus {
                    kind: ProviderKind::OpenCode,
                    label,
                    installed: false,
                    authenticated: false,
                    version: None,
                    status: ProviderStatusLevel::Error,
                    message: Some(format!(
                        "opencode CLI was found at `{binary}` but `--version` failed: {err}. {}",
                        OPENCODE_INSTALL_HINT
                    )),
                    models: Vec::new(),
                    enabled: true,
                    features: zenui_provider_api::ProviderFeatures::default(),
                };
            }
        };

        // Second probe: start the HTTP server (or reuse an already-
        // running one) and hit `/health`. Server startup failure
        // surfaces as a warning — the binary is present, but the
        // user's environment isn't letting it bind a local port or
        // finish initialising. The daemon still lets them try a turn
        // so the error lands with full diagnostics instead of being
        // swallowed here.
        let status = match self.ensure_server().await {
            Ok(server) => match server.client().health().await {
                Ok(()) => ProviderStatus {
                    kind: ProviderKind::OpenCode,
                    label: label.clone(),
                    installed: true,
                    authenticated: true,
                    version,
                    status: ProviderStatusLevel::Ready,
                    message: Some(format!("{label} server is running on {}.", server.url())),
                    models: Vec::new(),
                    enabled: true,
                    features: zenui_provider_api::ProviderFeatures::default(),
                },
                Err(err) => ProviderStatus {
                    kind: ProviderKind::OpenCode,
                    label,
                    installed: true,
                    authenticated: false,
                    version,
                    status: ProviderStatusLevel::Warning,
                    message: Some(format!(
                        "opencode server started but health probe failed: {err}"
                    )),
                    models: Vec::new(),
                    enabled: true,
                    features: zenui_provider_api::ProviderFeatures::default(),
                },
            },
            Err(err) => ProviderStatus {
                kind: ProviderKind::OpenCode,
                label,
                installed: true,
                authenticated: false,
                version,
                status: ProviderStatusLevel::Warning,
                message: Some(format!("failed to start opencode server: {err}")),
                models: Vec::new(),
                enabled: true,
                features: zenui_provider_api::ProviderFeatures::default(),
            },
        };

        status
    }

    async fn fetch_models(&self) -> Result<Vec<ProviderModel>, String> {
        let client = self.client().await?;
        client.list_models().await
    }

    async fn start_session(
        &self,
        session: &SessionDetail,
    ) -> Result<Option<ProviderSessionState>, String> {
        // If a native session id is already persisted (adapter reload,
        // session restore), reuse it — opencode's server keeps the
        // conversation alive across daemon restarts as long as the
        // session row hasn't been deleted.
        if let Some(existing) = session
            .provider_state
            .as_ref()
            .and_then(|s| s.native_thread_id.as_deref())
        {
            debug!(
                session_id = %session.summary.session_id,
                native = %existing,
                "opencode: reusing existing native session id"
            );
            return Ok(None);
        }

        let client = self.client().await?;
        let cwd = zenui_provider_api::helpers::session_cwd(session, &self.working_directory);
        // On `start_session` we don't yet know which permission mode
        // the user will run with — the runtime only hands it through
        // on `execute_turn`. Default here to the conservative "ask"
        // ruleset; `execute_turn` recreates the session with the
        // caller's chosen mode if it's different.
        let native_id = client
            .create_session(
                cwd.to_string_lossy().as_ref(),
                session.summary.model.as_deref(),
                PermissionMode::Default,
            )
            .await?;

        Ok(Some(ProviderSessionState {
            native_thread_id: Some(native_id),
            metadata: None,
        }))
    }

    async fn execute_turn(
        &self,
        session: &SessionDetail,
        input: &UserInput,
        permission_mode: PermissionMode,
        reasoning_effort: Option<ReasoningEffort>,
        _thinking_mode: Option<ThinkingMode>,
        events: TurnEventSink,
    ) -> Result<ProviderTurnOutput, String> {
        if !input.images.is_empty() {
            warn!(
                provider = ?ProviderKind::OpenCode,
                count = input.images.len(),
                "opencode adapter dropping image attachments; multimodal input not yet wired"
            );
        }

        let client = self.client().await?;

        // Resolve / create the native opencode session id. Sessions
        // created by a prior `start_session` carry it through
        // `provider_state`; sessions created elsewhere (e.g. REPL
        // flows that skip `start_session`) get one lazily here with
        // the caller's permission mode baked in.
        let native_id = match session
            .provider_state
            .as_ref()
            .and_then(|s| s.native_thread_id.clone())
        {
            Some(id) => id,
            None => {
                let cwd = zenui_provider_api::helpers::session_cwd(session, &self.working_directory);
                client
                    .create_session(
                        cwd.to_string_lossy().as_ref(),
                        session.summary.model.as_deref(),
                        permission_mode,
                    )
                    .await?
            }
        };

        // Register this session's sink with the router *before*
        // firing the prompt so we can't miss the first SSE event.
        let subscription = self
            .event_router
            .subscribe(native_id.clone(), events.clone())
            .await;

        let prompt_result = client
            .send_prompt(
                &native_id,
                &input.text,
                session.summary.model.as_deref(),
                reasoning_effort,
                permission_mode,
            )
            .await;
        if let Err(err) = prompt_result {
            subscription.cancel().await;
            return Err(err);
        }

        // Wait for the turn-complete signal emitted by the SSE
        // reader. Returns the accumulated output text on success or
        // a diagnostic error on timeout / server-side failure.
        let output = subscription
            .wait_for_completion(std::time::Duration::from_secs(TURN_TIMEOUT_SECS))
            .await?;

        // `drain_pending` is the contract every adapter has with the
        // sink — orphaned permission / question oneshots must be
        // released when the turn ends, otherwise they leak forever.
        events.drain_pending().await;

        Ok(ProviderTurnOutput {
            output,
            provider_state: Some(ProviderSessionState {
                native_thread_id: Some(native_id),
                metadata: None,
            }),
        })
    }

    async fn interrupt_turn(&self, session: &SessionDetail) -> Result<String, String> {
        let Some(native_id) = session
            .provider_state
            .as_ref()
            .and_then(|s| s.native_thread_id.as_deref())
        else {
            // No native id = no turn in flight from opencode's point of
            // view. Treat as a no-op so the UI's Stop button doesn't
            // surface a scary error.
            return Ok("No opencode session to interrupt.".to_string());
        };

        let client = self.client().await?;
        client.abort_session(native_id).await?;
        Ok("Interrupt sent to opencode.".to_string())
    }

    async fn end_session(&self, session: &SessionDetail) -> Result<(), String> {
        // Tell the router to drop any sink registered for this
        // session, but leave the opencode server alone — other
        // flowstate sessions may still be using it. Server shutdown
        // happens when the `OpenCodeServer` arc is dropped (daemon
        // tear-down) or the child exits on its own.
        if let Some(native_id) = session
            .provider_state
            .as_ref()
            .and_then(|s| s.native_thread_id.as_deref())
        {
            self.event_router.unsubscribe(native_id).await;
        }
        Ok(())
    }
}
