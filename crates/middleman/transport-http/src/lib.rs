//! HTTP + WebSocket transport for the ZenUI daemon.
//!
//! Implements `zenui_daemon_core::Transport` via a two-stage
//! `HttpTransport` → `HttpBound` → `HttpHandle` lifecycle. `bind()` runs on
//! the host thread and claims the TCP listener synchronously (so
//! port-in-use errors surface at daemon startup, not later). `serve()`
//! runs inside the daemon's tokio runtime and spawns the axum accept
//! loop. `shutdown()` is async — it sends the oneshot, awaits the accept
//! task, and drops the listener.
//!
//! The frontend is embedded into the binary at compile time via
//! `rust-embed` (see `build.rs` + `FrontendAssets`), so the daemon has no
//! runtime dependency on `apps/zenui/frontend/dist` existing on disk.

use std::net::{SocketAddr, TcpListener as StdTcpListener};
use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, Path, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::Utc;
use futures::{SinkExt, StreamExt};
use rust_embed::Embed;
use tokio::net::TcpListener;
use tokio::sync::{broadcast, oneshot};
use tokio::task::JoinHandle;
use tracing::{error, warn};
use zenui_daemon_core::{Bound, Transport, TransportAddressInfo, TransportHandle};
use zenui_provider_api::{
    AppSnapshot, BootstrapPayload, ClientMessage, HealthPayload, ServerMessage,
};
use zenui_runtime_core::{ConnectionObserver, RuntimeCore};

/// Frontend bundle embedded into the binary at build time. The path is
/// relative to this crate's `Cargo.toml`; `build.rs` runs
/// `bun install && bun run build` before the embed happens, so the
/// `dist/` directory is guaranteed to exist whenever this compiles.
#[derive(Embed)]
#[folder = "../../../apps/zenui/frontend/dist/"]
struct FrontendAssets;

/// HTTP + WebSocket transport. Construct this in your app's `main()`,
/// add it to `run_blocking`'s transport list, and daemon-core handles
/// the rest of the lifecycle.
pub struct HttpTransport {
    bind_addr: SocketAddr,
}

impl HttpTransport {
    pub fn new(bind_addr: SocketAddr) -> Self {
        Self { bind_addr }
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

        // Verify the embedded frontend is present. This is a compile-time
        // guarantee once rust-embed runs, but we check defensively so a
        // mis-wired build.rs surfaces at daemon startup instead of at
        // first request.
        if FrontendAssets::get("index.html").is_none() {
            anyhow::bail!(
                "embedded frontend bundle is missing index.html; check transport-http build.rs"
            );
        }

        Ok(Box::new(HttpBound {
            std_listener: Some(std_listener),
            address,
        }))
    }
}

/// HTTP transport that has bound its TCP socket and is waiting for
/// `serve()` to be called inside the tokio runtime context.
pub struct HttpBound {
    std_listener: Option<StdTcpListener>,
    address: SocketAddr,
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
        let listener = TcpListener::from_std(std_listener)
            .context("failed to move listener into tokio")?;

        let address = self.address;
        let ws_url = format!("ws://{address}/ws");

        let state = ApiState {
            runtime,
            ws_url,
            observer,
        };
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

        let task = tokio::spawn(async move {
            let router = Router::new()
                .route("/", get(index_handler))
                .route("/index.html", get(index_handler))
                .route("/api/health", get(health_handler))
                .route("/api/bootstrap", get(bootstrap_handler))
                .route("/api/snapshot", get(snapshot_handler))
                .route("/api/status", get(status_handler))
                .route("/api/shutdown", post(shutdown_handler))
                .route("/ws", get(ws_handler))
                .route("/{*path}", get(static_handler))
                .with_state(state);

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

async fn index_handler() -> Response {
    serve_embedded("index.html")
}

/// Fallback route handler for everything that isn't an API/WS route.
/// Serves the requested file from the embedded bundle; for paths that
/// don't resolve to an embedded file, falls back to `index.html` so the
/// React SPA router can take over.
async fn static_handler(Path(path): Path<String>) -> Response {
    // API and websocket routes have their own handlers and never reach
    // here. The SPA fallback below covers client-side routes like
    // `/projects/123` that don't correspond to embedded files.
    if FrontendAssets::get(&path).is_some() {
        serve_embedded(&path)
    } else {
        serve_embedded("index.html")
    }
}

fn serve_embedded(path: &str) -> Response {
    match FrontendAssets::get(path) {
        Some(file) => {
            let mime = mime_guess::from_path(path)
                .first_or_octet_stream()
                .as_ref()
                .to_string();
            let cache_control = if path == "index.html" {
                "no-store, must-revalidate"
            } else {
                "public, max-age=3600"
            };
            (
                [
                    (header::CONTENT_TYPE, mime),
                    (header::CACHE_CONTROL, cache_control.to_string()),
                ],
                file.data.into_owned(),
            )
                .into_response()
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn health_handler() -> Json<HealthPayload> {
    Json(HealthPayload {
        status: "ok".to_string(),
        generated_at: Utc::now().to_rfc3339(),
    })
}

async fn bootstrap_handler(State(state): State<ApiState>) -> Json<BootstrapPayload> {
    Json(state.runtime.bootstrap(state.ws_url.clone()).await)
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
        bootstrap: state.runtime.bootstrap(state.ws_url.clone()).await,
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
                        if sub_tx.send(ServerMessage::SessionLoaded { session }).is_err() {
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
                        if let Some(response) = runtime.handle_client_message(client_message).await {
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
