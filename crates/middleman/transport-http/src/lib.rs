//! HTTP + WebSocket transport for the agent daemon.
//!
//! Implements `zenui_runtime_core::transport::Transport` via a two-stage
//! `HttpTransport` → `HttpBound` → `HttpHandle` lifecycle. `bind()` runs on
//! the host thread and claims the TCP listener synchronously (so
//! port-in-use errors surface at daemon startup, not later). `serve()`
//! runs inside the daemon's tokio runtime and spawns the axum accept
//! loop. `shutdown()` is async — it sends the oneshot, awaits the accept
//! task, and drops the listener.
//!
//! This is a pure transport — it exposes the JSON API and WebSocket
//! stream, and nothing else. Serving a UI bundle is the application's
//! responsibility, not the transport's.

use std::net::{SocketAddr, TcpListener as StdTcpListener};
use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::Utc;
use futures::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio::sync::{broadcast, oneshot};
use tokio::task::JoinHandle;
use tracing::{error, warn};
use serde::{Deserialize, Serialize};
use zenui_provider_api::{
    AppSnapshot, BootstrapPayload, ClientMessage, HealthPayload, RuntimeCall, RuntimeCallError,
    RuntimeCallOrigin, RuntimeCallResult, ServerMessage, ToolCatalogEntry, capability_tools_wire,
};
use zenui_runtime_core::transport::{Bound, Transport, TransportAddressInfo, TransportHandle};
use zenui_runtime_core::{ConnectionObserver, RuntimeCore};

/// HTTP + WebSocket transport. Construct this in your app's `main()`,
/// add it to `run_blocking`'s transport list, and daemon-core handles
/// the rest of the lifecycle.
///
/// # No authentication
///
/// The transport has no bearer-token or cookie auth. It relies on
/// the loopback bind (`127.0.0.1`) being the only access path — on
/// a single-user desktop every local process that can reach the
/// port already runs with the user's credentials and has unrestricted
/// access to the flowstate SQLite store and attachments directory
/// regardless. If flowstate is ever deployed on a multi-user host,
/// reintroduce a bearer-token middleware here (see the git log for
/// the prior shape — a `require_bearer_token` layer gated off an
/// `auth_token: Option<String>` field on this struct).
pub struct HttpTransport {
    bind_addr: SocketAddr,
    /// Extra axum Router merged into the main router just before
    /// `serve()` returns. Used by Phase 4 to attach the flowstate-
    /// app-layer HTTP handlers (user_config / usage / …) under the
    /// same loopback port that serves `/api/orchestration/*`. Kept
    /// `Router<()>` so it doesn't leak app-layer state types into
    /// the transport crate — callers `.with_state(...)` on their
    /// inner routes before handing the Router here.
    extra_router: Option<Router<()>>,
}

impl HttpTransport {
    pub fn new(bind_addr: SocketAddr) -> Self {
        Self {
            bind_addr,
            extra_router: None,
        }
    }

    /// Builder-style chain: merge an additional axum Router into the
    /// main transport router. Added Phase 4; used by the flowstate
    /// Tauri app to expose app-layer REST handlers alongside the
    /// orchestration surface without adding those app-specific
    /// routes into this transport crate.
    pub fn with_extra_router(mut self, router: Router<()>) -> Self {
        self.extra_router = Some(router);
        self
    }
}

impl Transport for HttpTransport {
    fn kind(&self) -> &'static str {
        "http"
    }

    fn bind(self: Box<Self>) -> Result<Box<dyn Bound>> {
        let std_listener = StdTcpListener::bind(self.bind_addr)
            .with_context(|| format!("failed to bind HTTP server at {}", self.bind_addr))?;
        std_listener
            .set_nonblocking(true)
            .context("failed to mark listener non-blocking")?;
        let address = std_listener
            .local_addr()
            .context("failed to read local listener address")?;

        Ok(Box::new(HttpBound {
            std_listener: Some(std_listener),
            address,
            extra_router: self.extra_router,
        }))
    }
}

/// HTTP transport that has bound its TCP socket and is waiting for
/// `serve()` to be called inside the tokio runtime context.
pub struct HttpBound {
    std_listener: Option<StdTcpListener>,
    address: SocketAddr,
    /// Carries the `with_extra_router`-supplied Router through bind
    /// → serve so it can be merged in just before the server boots.
    extra_router: Option<Router<()>>,
}

impl Bound for HttpBound {
    fn kind(&self) -> &'static str {
        "http"
    }

    fn address_info(&self) -> TransportAddressInfo {
        TransportAddressInfo::Http {
            http_base: format!("http://{}", self.address),
            ws_url: format!("ws://{}/ws", self.address),
        }
    }

    fn serve(
        mut self: Box<Self>,
        runtime: Arc<RuntimeCore>,
        observer: Arc<dyn ConnectionObserver>,
    ) -> Result<Box<dyn TransportHandle>> {
        let std_listener = self
            .std_listener
            .take()
            .expect("HttpBound::serve called twice");
        let listener =
            TcpListener::from_std(std_listener).context("failed to move listener into tokio")?;

        let address = self.address;
        let ws_url = format!("ws://{address}/ws");

        let state = ApiState {
            runtime,
            ws_url,
            observer,
        };
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

        let extra_router = self.extra_router.take();
        let task = tokio::spawn(async move {
            let router = Router::new()
                .route("/api/health", get(health_handler))
                .route("/api/version", get(version_handler))
                .route("/api/bootstrap", get(bootstrap_handler))
                .route("/api/snapshot", get(snapshot_handler))
                .route("/api/status", get(status_handler))
                .route("/api/shutdown", post(shutdown_handler))
                // Cross-provider orchestration entry point consumed by
                // the `flowstate mcp-server` subprocess. Agents running
                // under opencode, Copilot SDK, or any CLI that accepts
                // an MCP stdio subprocess invoke flowstate's
                // spawn/send/read tools through a local MCP server that
                // POSTs `RuntimeCall`s here and returns the runtime's
                // result verbatim. Single source of truth for the tool
                // catalog lives in
                // `zenui_provider_api::capabilities::capability_tools_wire`.
                .route(
                    "/api/orchestration/catalog",
                    get(orchestration_catalog_handler),
                )
                .route(
                    "/api/orchestration/dispatch",
                    post(orchestration_dispatch_handler),
                )
                // Phase 5.5.7 — webview replay route. A just-reconnected
                // UI passes its last-seen seq as `?since=N`; we return
                // every buffered event with seq > N for that session.
                // Combined with subscribing to the live WS, this gives
                // a gapless reconnect.
                .route(
                    "/api/sessions/{session_id}/events",
                    get(session_events_replay_handler),
                )
                .route("/ws", get(ws_handler))
                .with_state(state);
            // Merge caller-supplied routes (Phase 4 app-layer
            // handlers) AFTER `.with_state(state)` so our own
            // handlers remain keyed on `ApiState` while the extra
            // router carries its own `State` already baked in by
            // the caller.
            let router = if let Some(extra) = extra_router {
                router.merge(extra)
            } else {
                router
            };

            let server = axum::serve(
                listener,
                router.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .with_graceful_shutdown(async {
                let _ = shutdown_rx.await;
            });

            if let Err(server_error) = server.await {
                error!("HTTP transport exited with error: {server_error}");
            }
        });

        Ok(Box::new(HttpHandle {
            address,
            shutdown_tx: Some(shutdown_tx),
            task: Some(task),
        }))
    }
}

/// Handle for a running HTTP transport. Dropping it without calling
/// `shutdown()` aborts the accept task — prefer the explicit async path
/// during graceful shutdown.
pub struct HttpHandle {
    address: SocketAddr,
    shutdown_tx: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<()>>,
}

#[async_trait]
impl TransportHandle for HttpHandle {
    fn kind(&self) -> &'static str {
        "http"
    }

    fn address_info(&self) -> TransportAddressInfo {
        TransportAddressInfo::Http {
            http_base: format!("http://{}", self.address),
            ws_url: format!("ws://{}/ws", self.address),
        }
    }

    async fn shutdown(self: Box<Self>) {
        // HttpHandle implements Drop, so we can't destructure the Box.
        // Rebind as mut and take() the Option fields. The Drop impl still
        // runs when `me` goes out of scope, but it's a no-op by then
        // because both Options are already None.
        let mut me = self;
        let shutdown_tx = me.shutdown_tx.take();
        let task = me.task.take();
        if let Some(tx) = shutdown_tx {
            let _ = tx.send(());
        }
        if let Some(task) = task {
            let _ = task.await;
        }
    }
}

impl Drop for HttpHandle {
    fn drop(&mut self) {
        // Fallback for callers that let the handle drop without calling
        // shutdown(). Sends the graceful oneshot (best effort) and aborts
        // the accept task. Equivalent to the old LocalServer::drop.
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

// ---------------------------------------------------------------------------
// Route handlers — unchanged from the pre-transport-trait code except that
// `observer` is now non-optional (so /api/status + /api/shutdown always
// have a real observer to consult, and NoopObserver fills the no-op case).
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct ApiState {
    runtime: Arc<RuntimeCore>,
    ws_url: String,
    observer: Arc<dyn ConnectionObserver>,
}

/// GET /api/version — stable handshake payload consumed by daemon
/// clients (the flowstate Tauri proxy, the MCP subprocess) to detect
/// mismatched shell ↔ daemon versions after an auto-update. Always
/// reachable; no auth on the loopback transport.
///
/// `schema_version` bumps only on breaking changes to the HTTP/WS
/// wire shape; `build_sha` is a commit-sha stamp for debugging / bug
/// reports. Keep the schema_version increment policy documented on
/// the matching consumer in the Tauri shell.
#[derive(serde::Serialize)]
struct VersionPayload {
    schema_version: u32,
    build_sha: &'static str,
    transport: &'static str,
}

async fn version_handler() -> Json<VersionPayload> {
    Json(VersionPayload {
        // Pulled from the single source of truth in `provider-api`
        // so shell + daemon + mcp-server can't drift. Bump the
        // constant there, not here.
        schema_version: zenui_provider_api::SCHEMA_VERSION,
        // `option_env!` returns `Option<&'static str>` — so default
        // to "dev" for local builds that don't set the env var.
        build_sha: option_env!("FLOWSTATE_BUILD_SHA").unwrap_or("dev"),
        transport: "http",
    })
}

async fn health_handler() -> Json<HealthPayload> {
    Json(HealthPayload {
        status: "ok".to_string(),
        generated_at: Utc::now().to_rfc3339(),
    })
}

async fn bootstrap_handler(State(state): State<ApiState>) -> Json<BootstrapPayload> {
    // Phase 4.10 made `bootstrap`'s ws_url optional (None for in-proc
    // transports like Tauri). HTTP always provides a real URL.
    Json(state.runtime.bootstrap(Some(state.ws_url.clone())).await)
}

async fn snapshot_handler(State(state): State<ApiState>) -> Json<AppSnapshot> {
    Json(state.runtime.snapshot().await)
}

async fn ws_handler(ws: WebSocketUpgrade, State(state): State<ApiState>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

/// GET /api/status — returns a `DaemonStatus` snapshot from the observer.
/// Returns 501 when the observer is a no-op (e.g. `NoopObserver`) whose
/// default `status()` returns None.
async fn status_handler(State(state): State<ApiState>) -> Response {
    match state.observer.status() {
        Some(status) => Json(status).into_response(),
        None => StatusCode::NOT_IMPLEMENTED.into_response(),
    }
}

/// Request body for `POST /api/orchestration/dispatch`. The caller —
/// currently only the `flowstate-mcp-server` binary — supplies the
/// originating session id (passed in via env var when the provider
/// adapter spawned the MCP server subprocess) plus the `RuntimeCall`
/// shape the agent wants to execute. `origin_turn_id` is optional:
/// external callers without a real turn context should omit it so
/// the server synthesizes a per-call id, which means orchestration
/// budget is scoped per call rather than per turn.
///
/// **Do not** rename these fields to `session_id` / `turn_id`.
/// `RuntimeCall` variants (`Send`, `Poll`, `SendAndAwait`, etc.) have
/// their own `session_id` that targets a *different* session — the
/// peer to message or poll. With `#[serde(flatten)]`, a naming
/// collision made the outer field swallow the JSON `session_id`
/// before the inner variant could see it, producing a cryptic
/// `missing field "session_id"` deserialization error on every poll
/// / send / send_and_await call. The `origin_*` prefix keeps the two
/// axes distinct on the wire.
#[derive(Debug, Deserialize)]
pub struct OrchestrationDispatchRequest {
    pub origin_session_id: String,
    #[serde(default)]
    pub origin_turn_id: Option<String>,
    #[serde(flatten)]
    pub call: RuntimeCall,
}

/// Response body for `POST /api/orchestration/dispatch`. Exactly one
/// of `result` / `error` is populated — same contract as the bridge
/// RPC's `runtime_call_response` so the MCP server can relay both
/// shapes to its MCP client without reshaping.
#[derive(Debug, Serialize)]
pub struct OrchestrationDispatchResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<RuntimeCallResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RuntimeCallError>,
}

/// GET /api/orchestration/catalog — serves the cross-provider tool
/// catalog. The `flowstate-mcp-server` binary fetches this once at
/// startup and replays it to its MCP client on `tools/list`. Returning
/// the same wire shape the Claude SDK bridge consumes (see
/// `BridgeRequest::LoadToolCatalog` in `provider-claude-sdk/src/wire.rs`)
/// means every provider's flowstate tools stay in lock-step — adding a
/// variant to `ProviderKind` or a new orchestration tool in
/// `capabilities.rs` shows up on every client after a rebuild, with no
/// per-provider edits.
async fn orchestration_catalog_handler() -> Json<Vec<ToolCatalogEntry>> {
    Json(capability_tools_wire())
}

/// POST /api/orchestration/dispatch — executes a `RuntimeCall` on
/// behalf of an out-of-process MCP server. Body:
/// `{ "session_id": "...", "turn_id": "...", "kind": "spawn", ... }`
/// (the `kind` + call-specific fields flatten from `RuntimeCall`).
/// 200 + `{ result }` on success, 200 + `{ error }` on a dispatcher
/// error (cycle, budget, timeout, …). Non-200 is reserved for
/// transport-level failures.
async fn orchestration_dispatch_handler(
    State(state): State<ApiState>,
    Json(body): Json<OrchestrationDispatchRequest>,
) -> Json<OrchestrationDispatchResponse> {
    let origin = RuntimeCallOrigin {
        session_id: body.origin_session_id,
        // Synthesize a per-call turn id when the external caller
        // doesn't have one. Budgets key off `turn_id`, so this gives
        // each external call its own fresh budget — equivalent to the
        // in-process "one tool call per turn" bound when the call
        // isn't riding on a real agent turn.
        turn_id: body
            .origin_turn_id
            .unwrap_or_else(|| format!("ext-{}", uuid::Uuid::new_v4())),
    };
    match state
        .runtime
        .clone()
        .dispatch_runtime_call_external(origin, body.call)
        .await
    {
        Ok(result) => Json(OrchestrationDispatchResponse {
            result: Some(result),
            error: None,
        }),
        Err(error) => Json(OrchestrationDispatchResponse {
            result: None,
            error: Some(error),
        }),
    }
}

/// Query params for the replay route.
#[derive(Debug, Deserialize, Default)]
struct EventsReplayQuery {
    /// Last seq the client has already seen. `0` (or omitted) asks
    /// for the full ring from the earliest retained event.
    #[serde(default)]
    since: u64,
}

/// JSON body of the replay response.
#[derive(Debug, Serialize)]
struct EventsReplayResponse {
    /// Events with `seq > since`, in seq order.
    events: Vec<EventsReplayEntry>,
    /// Oldest seq still retained in the ring. `null` if the ring is
    /// empty for this session.
    oldest_retained_seq: Option<u64>,
    /// Next seq the ring will assign on the next `publish`. Lets the
    /// client know "if you subscribe to the live stream now, the
    /// first event you'll see will have seq ≥ this number."
    next_seq: u64,
    /// True iff the caller's `since` was older than what the ring
    /// still has. Client MUST fall back to a full
    /// `LoadSession` + fresh live subscription; the events field
    /// alone is not enough to reconstruct state.
    gap_detected: bool,
}

#[derive(Debug, Serialize)]
struct EventsReplayEntry {
    seq: u64,
    event: zenui_provider_api::RuntimeEvent,
}

/// GET /api/sessions/:session_id/events?since=N
///
/// Phase 5.5.7 replay route. Returns every buffered event for
/// `session_id` with seq > since plus metadata the client uses to
/// detect gaps. Never streams — it's a snapshot. For live events,
/// use the WS.
async fn session_events_replay_handler(
    State(state): State<ApiState>,
    axum::extract::Path(session_id): axum::extract::Path<String>,
    axum::extract::Query(query): axum::extract::Query<EventsReplayQuery>,
) -> Json<EventsReplayResponse> {
    let result = state.runtime.replay_session_events(&session_id, query.since);
    let gap = result.gap_detected(query.since);
    let entries = result
        .events
        .into_iter()
        .map(|e| EventsReplayEntry {
            seq: e.seq,
            event: e.event,
        })
        .collect();
    Json(EventsReplayResponse {
        events: entries,
        oldest_retained_seq: result.oldest_retained_seq,
        next_seq: result.next_seq,
        gap_detected: gap,
    })
}

/// POST /api/shutdown — loopback-only endpoint that asks the daemon to
/// begin graceful shutdown. 204 on success, 403 for non-loopback peers.
async fn shutdown_handler(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    State(state): State<ApiState>,
) -> StatusCode {
    if !peer.ip().is_loopback() {
        warn!(%peer, "rejecting /api/shutdown from non-loopback peer");
        return StatusCode::FORBIDDEN;
    }
    tracing::info!(%peer, "loopback shutdown request received");
    state.observer.on_shutdown_requested();
    StatusCode::NO_CONTENT
}

async fn handle_socket(socket: WebSocket, state: ApiState) {
    state.observer.on_client_connected();
    // RAII guard: fires the disconnect hook unconditionally on function
    // exit, including panic paths.
    struct DisconnectGuard(Arc<dyn ConnectionObserver>);
    impl Drop for DisconnectGuard {
        fn drop(&mut self) {
            self.0.on_client_disconnected();
        }
    }
    let _disconnect_guard = DisconnectGuard(state.observer.clone());

    // Three concurrent halves share a single outbound mpsc that feeds the
    // writer task. Otherwise, awaiting `handle_client_message` (which
    // streams runtime events through the broadcast channel) would block
    // subscription draining, and the UI would only see events after each
    // turn finishes.
    let (mut sender, mut receiver) = socket.split();
    // INVARIANT: subscribe() must precede bootstrap(). Any event published
    // between bootstrap's database read and subscribe() would otherwise be
    // lost on reconnect, creating a silent gap in the client's view. Do
    // not reorder these two lines for "efficiency" — the subscription's
    // ring buffer is what closes the gap.
    let mut subscription = state.runtime.subscribe();
    let (out_tx, mut out_rx) = tokio::sync::mpsc::unbounded_channel::<ServerMessage>();

    let welcome = ServerMessage::Welcome {
        bootstrap: state.runtime.bootstrap(Some(state.ws_url.clone())).await,
    };
    if out_tx.send(welcome).is_err() {
        return;
    }

    let writer = tokio::spawn(async move {
        while let Some(message) = out_rx.recv().await {
            if let Err(send_error) = send_server_message(&mut sender, message).await {
                warn!("failed to write websocket payload: {send_error}");
                break;
            }
        }
    });

    let sub_tx = out_tx.clone();
    let sub_runtime = state.runtime.clone();
    let subscriber = tokio::spawn(async move {
        loop {
            match subscription.recv().await {
                Ok(event) => {
                    if sub_tx.send(ServerMessage::Event { event }).is_err() {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    warn!(
                        "websocket subscriber lagged behind by {skipped} messages; sending snapshot + active session reseed"
                    );
                    // On Lagged we've dropped events; push a fresh
                    // snapshot for the sidebar plus a SessionLoaded for
                    // every session that's currently mid-turn so chat
                    // views can reconcile in-flight tool calls that
                    // were dropped from the broadcast queue.
                    let snapshot = sub_runtime.snapshot().await;
                    if sub_tx.send(ServerMessage::Snapshot { snapshot }).is_err() {
                        break;
                    }
                    let mut send_failed = false;
                    for session in sub_runtime.active_session_details().await {
                        if sub_tx
                            .send(ServerMessage::SessionLoaded { session })
                            .is_err()
                        {
                            send_failed = true;
                            break;
                        }
                    }
                    if send_failed {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    while let Some(inbound) = receiver.next().await {
        match inbound {
            Ok(Message::Text(text)) => match serde_json::from_str::<ClientMessage>(text.as_str()) {
                Ok(client_message) => {
                    let runtime = state.runtime.clone();
                    let tx = out_tx.clone();
                    tokio::spawn(async move {
                        if let Some(response) = runtime.handle_client_message(client_message).await
                        {
                            let _ = tx.send(response);
                        }
                    });
                }
                Err(parse_error) => {
                    let _ = out_tx.send(ServerMessage::Error {
                        message: format!("Invalid websocket payload: {parse_error}"),
                    });
                }
            },
            Ok(Message::Ping(_)) | Ok(Message::Pong(_)) => {}
            Ok(Message::Close(_)) | Err(_) => break,
            Ok(_) => {}
        }
    }

    drop(out_tx);
    subscriber.abort();
    let _ = writer.await;
}

async fn send_server_message(
    sender: &mut futures::stream::SplitSink<WebSocket, Message>,
    message: ServerMessage,
) -> Result<()> {
    let payload =
        serde_json::to_string(&message).context("failed to serialize websocket payload")?;
    sender
        .send(Message::Text(payload.into()))
        .await
        .context("failed to send websocket payload")
}
