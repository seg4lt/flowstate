use std::net::{SocketAddr, TcpListener as StdTcpListener};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::IntoResponse;
use axum::routing::get;
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
use zenui_runtime_core::RuntimeCore;

#[derive(Clone)]
struct ApiState {
    runtime: Arc<RuntimeCore>,
    ws_url: String,
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
    let state = ApiState { runtime, ws_url };
    let (shutdown_tx, shutdown_rx) = oneshot::channel();

    let task = runtime_handle.spawn(async move {
        let static_assets =
            ServeDir::new(frontend_dist).not_found_service(ServeFile::new(index_file));
        let router = Router::new()
            .route("/api/health", get(health_handler))
            .route("/api/bootstrap", get(bootstrap_handler))
            .route("/api/snapshot", get(snapshot_handler))
            .route("/ws", get(ws_handler))
            .fallback_service(static_assets)
            .with_state(state);

        let server = axum::serve(listener, router).with_graceful_shutdown(async {
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

async fn handle_socket(socket: WebSocket, state: ApiState) {
    let (mut sender, mut receiver) = socket.split();
    let mut subscription = state.runtime.subscribe();

    if let Err(send_error) = send_server_message(
        &mut sender,
        ServerMessage::Welcome {
            bootstrap: state.runtime.bootstrap(state.ws_url.clone()).await,
        },
    )
    .await
    {
        warn!("failed to send welcome message: {send_error}");
        return;
    }

    loop {
        tokio::select! {
            inbound = receiver.next() => {
                match inbound {
                    Some(Ok(Message::Text(text))) => {
                        match serde_json::from_str::<ClientMessage>(text.as_str()) {
                            Ok(client_message) => {
                                if let Some(response) = state.runtime.handle_client_message(client_message).await
                                    && let Err(send_error) = send_server_message(&mut sender, response).await {
                                    warn!("failed to send response over websocket: {send_error}");
                                    break;
                                }
                            }
                            Err(parse_error) => {
                                if let Err(send_error) = send_server_message(
                                    &mut sender,
                                    ServerMessage::Error {
                                        message: format!("Invalid websocket payload: {parse_error}"),
                                    },
                                ).await {
                                    warn!("failed to send websocket error: {send_error}");
                                    break;
                                }
                            }
                        }
                    }
                    Some(Ok(Message::Ping(payload))) => {
                        if sender.send(Message::Pong(payload)).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(_)) => {}
                    Some(Err(socket_error)) => {
                        warn!("websocket receive error: {socket_error}");
                        break;
                    }
                }
            }
            runtime_event = subscription.recv() => {
                match runtime_event {
                    Ok(event) => {
                        if let Err(send_error) = send_server_message(&mut sender, ServerMessage::Event { event }).await {
                            warn!("failed to forward runtime event: {send_error}");
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        warn!("websocket subscriber lagged behind by {skipped} messages");
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }
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
