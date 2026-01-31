use std::net::{SocketAddr, TcpListener as StdTcpListener};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::extract::{ConnectInfo, State};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::Utc;
use futures::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio::sync::{broadcast, oneshot};
use tokio::task::JoinHandle;
use tower_http::services::{ServeDir, ServeFile};
use tracing::{error, warn};
use zenui_provider_api::{
    AppSnapshot, BootstrapPayload, ClientMessage, HealthPayload, ServerMessage,
};
use zenui_runtime_core::{ConnectionObserver, RuntimeCore};

#[derive(Clone)]
struct ApiState {
    runtime: Arc<RuntimeCore>,
    ws_url: String,
    index_file: PathBuf,
    lifecycle: Option<Arc<dyn ConnectionObserver>>,
}

pub struct LocalServer {
    address: SocketAddr,
    shutdown_tx: Option<oneshot::Sender<()>>,
    task: JoinHandle<()>,
}

impl LocalServer {
    pub fn frontend_url(&self) -> String {
        format!("http://{}", self.address)
    }
}

impl Drop for LocalServer {
    fn drop(&mut self) {
        if let Some(shutdown_tx) = self.shutdown_tx.take() {
            let _ = shutdown_tx.send(());
        }
        self.task.abort();
    }
}

pub fn spawn_local_server(
    runtime_handle: &tokio::runtime::Runtime,
    runtime: Arc<RuntimeCore>,
    frontend_dist: PathBuf,
    bind_addr: SocketAddr,
    lifecycle: Option<Arc<dyn ConnectionObserver>>,
) -> Result<LocalServer> {
    let std_listener =
        StdTcpListener::bind(bind_addr).context("failed to bind local HTTP server")?;
    std_listener
        .set_nonblocking(true)
        .context("failed to mark listener non-blocking")?;
    let address = std_listener
        .local_addr()
        .context("failed to read local listener address")?;
    let ws_url = format!("ws://{address}/ws");
    let listener = {
        let _runtime_guard = runtime_handle.enter();
        TcpListener::from_std(std_listener).context("failed to move listener into tokio")?
    };
    let index_file = frontend_dist.join("index.html");
    if !index_file.exists() {
        anyhow::bail!(
            "frontend build output is missing at {}",
            index_file.display()
        );
    }
    let state = ApiState {
        runtime,
        ws_url,
        index_file: index_file.clone(),
        lifecycle,
    };
    let (shutdown_tx, shutdown_rx) = oneshot::channel();

    let task = runtime_handle.spawn(async move {
        let static_assets =
            ServeDir::new(frontend_dist).not_found_service(ServeFile::new(index_file));
        let router = Router::new()
            .route("/", get(index_handler))
            .route("/index.html", get(index_handler))
            .route("/api/health", get(health_handler))
            .route("/api/bootstrap", get(bootstrap_handler))
            .route("/api/snapshot", get(snapshot_handler))
            .route("/api/status", get(status_handler))
            .route("/api/shutdown", post(shutdown_handler))
            .route("/ws", get(ws_handler))
            .fallback_service(static_assets)
            .with_state(state);

        let server = axum::serve(
            listener,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async {
            let _ = shutdown_rx.await;
        });

        if let Err(server_error) = server.await {
            error!("local server exited with error: {server_error}");
        }
    });

    Ok(LocalServer {
        address,
        shutdown_tx: Some(shutdown_tx),
        task,
    })
}

async fn index_handler(State(state): State<ApiState>) -> Response {
    match tokio::fs::read(&state.index_file).await {
        Ok(bytes) => (
            [
                (header::CONTENT_TYPE, "text/html; charset=utf-8"),
                (header::CACHE_CONTROL, "no-store, must-revalidate"),
            ],
            bytes,
        )
            .into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
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

/// GET /api/status — returns a `DaemonStatus` snapshot. Returns 501 when
/// http-api is running without a daemon observer attached.
async fn status_handler(State(state): State<ApiState>) -> Response {
    match state.lifecycle.as_ref().and_then(|o| o.status()) {
        Some(status) => Json(status).into_response(),
        None => StatusCode::NOT_IMPLEMENTED.into_response(),
    }
}

/// POST /api/shutdown — loopback-only endpoint that asks the daemon to
/// begin graceful shutdown. Returns:
///   204 if the lifecycle observer accepted the request
///   501 if no lifecycle is wired (e.g. when http-api is running inside the
///       in-process tao-web-shell with no daemon around it)
///   403 if the requester is not on loopback (defence in depth — we only
///       bind 127.0.0.1 anyway)
async fn shutdown_handler(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    State(state): State<ApiState>,
) -> StatusCode {
    if !peer.ip().is_loopback() {
        warn!(%peer, "rejecting /api/shutdown from non-loopback peer");
        return StatusCode::FORBIDDEN;
    }
    match state.lifecycle.as_ref() {
        Some(observer) => {
            tracing::info!(%peer, "loopback shutdown request received");
            observer.on_shutdown_requested();
            StatusCode::NO_CONTENT
        }
        None => StatusCode::NOT_IMPLEMENTED,
    }
}

async fn handle_socket(socket: WebSocket, state: ApiState) {
    if let Some(observer) = state.lifecycle.as_ref() {
        observer.on_client_connected();
    }
    let lifecycle_for_drop = state.lifecycle.clone();
    // RAII guard: fires the disconnect hook unconditionally on function exit,
    // including panic paths. Counter underflow is prevented because we only
    // decrement from this guard and we set _incremented by construction.
    struct DisconnectGuard(Option<Arc<dyn ConnectionObserver>>);
    impl Drop for DisconnectGuard {
        fn drop(&mut self) {
            if let Some(observer) = self.0.as_ref() {
                observer.on_client_disconnected();
            }
        }
    }
    let _disconnect_guard = DisconnectGuard(lifecycle_for_drop);

    // Three concurrent halves share a single outbound mpsc that feeds the writer
    // task. Otherwise, awaiting `handle_client_message` (which streams runtime
    // events through the broadcast channel) would block subscription draining,
    // and the UI would only see events after each turn finishes.
    let (mut sender, mut receiver) = socket.split();
    // INVARIANT: subscribe() must precede bootstrap(). Any event published
    // between bootstrap's database read and subscribe() would otherwise be
    // lost on reconnect, creating a silent gap in the client's view. Do not
    // reorder these two lines for "efficiency" — the subscription's ring
    // buffer is what closes the gap.
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
                    warn!("websocket subscriber lagged behind by {skipped} messages; sending fresh snapshot");
                    // On Lagged we've dropped events; push a fresh snapshot so
                    // the client re-reconciles from authoritative state.
                    let snapshot = sub_runtime.snapshot().await;
                    if sub_tx
                        .send(ServerMessage::Snapshot { snapshot })
                        .is_err()
                    {
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
