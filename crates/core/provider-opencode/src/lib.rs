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

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::{Mutex, OnceCell};
use tracing::{debug, warn};
use zenui_provider_api::{
    OrchestrationIpcHandle, OrchestrationIpcInfo, PermissionMode, ProviderAdapter, ProviderKind,
    ProviderModel, ProviderSessionState, ProviderStatus, ProviderStatusLevel, ProviderTurnOutput,
    ReasoningEffort, SessionDetail, ThinkingMode, TurnEventSink, UserInput, find_cli_binary,
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
    /// Working-directory fallback handed to the probe server when a
    /// session doesn't carry its own `cwd`. Per-flowstate-session
    /// servers are spawned in their own tmp cwds (see
    /// `session_cwd_dir`) so each one can pick up its own
    /// `opencode.json` with a session-scoped flowstate MCP entry.
    working_directory: PathBuf,
    /// Probe-only shared server. Lazy singleton used by `health()` and
    /// `fetch_models()` only — neither of those flows needs the
    /// flowstate MCP server registered, and spawning a fresh server
    /// for every probe would be wasteful. Session work goes through
    /// `session_servers` instead.
    server: Arc<OnceCell<Arc<OpenCodeServer>>>,
    /// Guards the one-time probe-server spawn.
    server_init_lock: Arc<Mutex<()>>,
    /// One `opencode serve` per flowstate session. Keyed by flowstate
    /// session id (not opencode's native id — we need this map *before*
    /// opencode has minted a session, because `start_session` is what
    /// creates the opencode side). The per-session server's cwd is
    /// `session_cwd_dir(flowstate_session_id)` and contains the
    /// session-specific `opencode.json` with `mcp.flowstate` pointing
    /// at `flowstate mcp-server --session-id <flowstate_session_id>`.
    /// Torn down in `end_session` so the child exits immediately when
    /// the user deletes a session; the process-count overhead is
    /// approximately one `opencode serve` per active flowstate
    /// session.
    session_servers: Arc<Mutex<HashMap<String, Arc<OpenCodeServer>>>>,
    /// Shared handle over the runtime's loopback HTTP transport. When
    /// populated, `ensure_session_server` writes a session-scoped
    /// `opencode.json` registering the flowstate MCP server before
    /// spawning. Absent → sessions run without cross-provider
    /// orchestration tools (pre-refactor behaviour).
    orchestration: Option<OrchestrationIpcHandle>,
    /// Routes SSE events to the right session's sink. Each per-session
    /// server kicks its own reader into this shared router; opencode
    /// mints unique session ids across servers so the map never
    /// collides.
    event_router: Arc<EventRouter>,
}

impl OpenCodeAdapter {
    /// Construct without cross-provider orchestration wiring. Sessions
    /// behave exactly like pre-refactor opencode — `opencode.json` is
    /// not written, the flowstate MCP server isn't registered, and
    /// agents can't call `flowstate_spawn` / etc.
    pub fn new(working_directory: PathBuf) -> Self {
        Self::new_with_orchestration(working_directory, None)
    }

    /// Construct with an optional [`OrchestrationIpcHandle`]. When
    /// populated, every per-session `opencode serve` spawn writes an
    /// `opencode.json` into its cwd that registers the `flowstate`
    /// MCP server for that session. Agents on those sessions see
    /// flowstate's spawn/send/read tools alongside opencode's
    /// built-ins.
    pub fn new_with_orchestration(
        working_directory: PathBuf,
        orchestration: Option<OrchestrationIpcHandle>,
    ) -> Self {
        Self {
            working_directory,
            server: Arc::new(OnceCell::new()),
            server_init_lock: Arc::new(Mutex::new(())),
            session_servers: Arc::new(Mutex::new(HashMap::new())),
            orchestration,
            event_router: Arc::new(EventRouter::new()),
        }
    }

    /// Filesystem path for a flowstate session's dedicated `opencode
    /// serve` cwd. Relative to the adapter's working directory so
    /// tear-down naturally clusters under one prefix and a
    /// `rm -rf opencode-sessions/` purges everything. Not created
    /// here — `ensure_session_server` handles directory creation +
    /// `opencode.json` emission atomically.
    fn session_cwd_dir(&self, flowstate_session_id: &str) -> PathBuf {
        self.working_directory
            .join("opencode-sessions")
            .join(flowstate_session_id)
    }

    /// Render the `opencode.json` payload registering the flowstate
    /// MCP server for this session. Returns `None` when orchestration
    /// isn't wired — callers fall through to starting the server
    /// without the config file (opencode accepts a missing
    /// `opencode.json` as "no project overrides").
    ///
    /// Shape (matches opencode's `McpLocalConfig`):
    /// ```json
    /// { "mcp": { "flowstate": {
    ///     "type": "local",
    ///     "command": [<flowstate>, "mcp-server",
    ///                 "--http-base", <URL>,
    ///                 "--session-id", <SID>],
    ///     "environment": {
    ///         "FLOWSTATE_AUTH_TOKEN": <token>,
    ///         "FLOWSTATE_SESSION_ID": <SID>,
    ///         "FLOWSTATE_HTTP_BASE":  <URL>
    ///     }
    /// } } }
    /// ```
    fn render_opencode_json(
        info: &OrchestrationIpcInfo,
        flowstate_session_id: &str,
    ) -> serde_json::Value {
        serde_json::json!({
            "mcp": {
                "flowstate": {
                    "type": "local",
                    "command": [
                        info.executable_path.to_string_lossy(),
                        "mcp-server",
                        "--http-base",
                        &info.base_url,
                        "--session-id",
                        flowstate_session_id,
                    ],
                    "environment": {
                        "FLOWSTATE_AUTH_TOKEN": &info.auth_token,
                        "FLOWSTATE_SESSION_ID": flowstate_session_id,
                        "FLOWSTATE_HTTP_BASE": &info.base_url,
                    }
                }
            }
        })
    }

    /// Ensure a per-flowstate-session `opencode serve` is up and
    /// return its handle. Writes `opencode.json` in the session's tmp
    /// cwd on first spawn, starts the SSE reader, and caches the
    /// handle in `session_servers`. Subsequent calls return the same
    /// handle.
    ///
    /// The double-checked pattern around `session_servers` avoids
    /// holding the mutex across the (slow) `OpenCodeServer::spawn`
    /// call. Two concurrent callers for the same session race, but
    /// the loser drops its freshly-spawned server and waits for the
    /// winner — no leaked children.
    async fn ensure_session_server(
        &self,
        flowstate_session_id: &str,
    ) -> Result<Arc<OpenCodeServer>, String> {
        // Fast path: already spawned.
        {
            let guard = self.session_servers.lock().await;
            if let Some(existing) = guard.get(flowstate_session_id) {
                return Ok(existing.clone());
            }
        }

        // Slow path: spawn outside the lock.
        let binary = Self::find_opencode_binary().ok_or_else(|| {
            format!(
                "opencode binary not found on PATH. {}",
                OPENCODE_INSTALL_HINT
            )
        })?;
        let cwd = self.session_cwd_dir(flowstate_session_id);
        tokio::fs::create_dir_all(&cwd)
            .await
            .map_err(|e| format!("failed to create opencode session cwd: {e}"))?;

        // Write `opencode.json` if orchestration wiring is active.
        // Doing this BEFORE the spawn means opencode reads it during
        // its own startup — no restart required.
        if let Some(info) = self.orchestration.as_ref().and_then(|h| h.get()) {
            let body = serde_json::to_string_pretty(&Self::render_opencode_json(
                info,
                flowstate_session_id,
            ))
            .map_err(|e| format!("failed to render opencode.json: {e}"))?;
            tokio::fs::write(cwd.join("opencode.json"), body)
                .await
                .map_err(|e| format!("failed to write opencode.json: {e}"))?;
            debug!(
                flowstate_session_id,
                cwd = %cwd.display(),
                "wrote per-session opencode.json with flowstate MCP entry"
            );
        }

        let server = OpenCodeServer::spawn(
            &binary,
            &cwd,
            std::time::Duration::from_secs(SERVER_STARTUP_TIMEOUT_SECS),
        )
        .await?;
        let server = Arc::new(server);
        self.event_router
            .spawn_reader(server.clone(), server.client());

        // Insert under the lock; if a concurrent caller beat us to it
        // the kill_on_drop on our child will tear it down when the
        // Arc we're about to drop falls out of scope.
        let mut guard = self.session_servers.lock().await;
        let stored = guard
            .entry(flowstate_session_id.to_string())
            .or_insert_with(|| server.clone())
            .clone();
        Ok(stored)
    }

    /// HTTP client for a given flowstate session. Thin wrapper around
    /// [`Self::ensure_session_server`].
    async fn session_client(
        &self,
        flowstate_session_id: &str,
    ) -> Result<Arc<OpenCodeClient>, String> {
        let server = self.ensure_session_server(flowstate_session_id).await?;
        Ok(server.client())
    }

    /// Tear down a flowstate session's dedicated opencode server.
    /// Called from `end_session` so that the child process exits
    /// promptly — the `kill_on_drop` on the Command handle would
    /// eventually reap it, but explicit cleanup is polite and
    /// deterministic. Also removes the tmp cwd so stale
    /// `opencode.json` files don't accumulate.
    async fn shutdown_session_server(&self, flowstate_session_id: &str) {
        let server = {
            let mut guard = self.session_servers.lock().await;
            guard.remove(flowstate_session_id)
        };
        if let Some(server) = server {
            // Dropping the Arc fires `Drop for OpenCodeServer` which
            // start_kill()s the child. The `kill_on_drop(true)` on
            // the `Command` handle is our backstop.
            drop(server);
            let cwd = self.session_cwd_dir(flowstate_session_id);
            if let Err(err) = tokio::fs::remove_dir_all(&cwd).await {
                // Non-fatal; leaving the dir around only costs a few
                // KB on disk.
                debug!(
                    flowstate_session_id,
                    cwd = %cwd.display(),
                    %err,
                    "failed to remove opencode session cwd"
                );
            }
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
        // IMPORTANT: every ProviderStatus we return below must carry
        // `features: features_for_kind(ProviderKind::OpenCode)`, NOT
        // `ProviderFeatures::default()`. The broadcast
        // `provider_health_updated` event sends this status verbatim
        // to the frontend, and the reducer *replaces* the whole
        // provider entry. Emitting a default (all-false) features
        // payload here causes the UI to lose any capability flag
        // (effort selector, context breakdown, etc.) the moment a
        // post-bootstrap health check lands — bootstrap alone reads
        // from `persistence::get_cached_health`, which re-hydrates
        // features, but subsequent broadcasts do not go through that
        // re-hydration path.
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
                    features: zenui_provider_api::features_for_kind(ProviderKind::OpenCode),
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
                    features: zenui_provider_api::features_for_kind(ProviderKind::OpenCode),
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
                    features: zenui_provider_api::features_for_kind(ProviderKind::OpenCode),
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
                    features: zenui_provider_api::features_for_kind(ProviderKind::OpenCode),
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
                features: zenui_provider_api::features_for_kind(ProviderKind::OpenCode),
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

        // Per-session server — spawns a dedicated `opencode serve`
        // for this flowstate session and writes its `opencode.json`
        // with the flowstate MCP entry before startup so the server
        // picks up cross-provider orchestration tools from turn one.
        let client = self
            .session_client(&session.summary.session_id)
            .await?;
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

        let client = self
            .session_client(&session.summary.session_id)
            .await?;

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

        let client = self
            .session_client(&session.summary.session_id)
            .await?;
        client.abort_session(native_id).await?;
        Ok("Interrupt sent to opencode.".to_string())
    }

    async fn end_session(&self, session: &SessionDetail) -> Result<(), String> {
        // Unsubscribe from SSE routing *and* shut down this session's
        // dedicated opencode server. With per-session servers, there
        // are no other flowstate sessions sharing this process — it
        // belongs to exactly one flowstate session id and leaks if we
        // leave it running after session delete.
        if let Some(native_id) = session
            .provider_state
            .as_ref()
            .and_then(|s| s.native_thread_id.as_deref())
        {
            self.event_router.unsubscribe(native_id).await;
        }
        self.shutdown_session_server(&session.summary.session_id)
            .await;
        Ok(())
    }
}
