//! WebSocket proxy to the flowzen daemon.
//!
//! The Tauri backend opens a single WebSocket connection to the daemon and
//! multiplexes it for the frontend's Tauri IPC calls:
//!
//! - `handle_message` commands push a ClientMessage onto the WS and await
//!   a response via a FIFO pending-response queue.
//! - `connect` commands subscribe to a buffered broadcast of all incoming
//!   messages. The buffer replays every message the proxy has seen since
//!   it opened the daemon connection, so late subscribers don't miss
//!   provider-health or other startup events that arrived before the
//!   frontend had a chance to subscribe.

use std::collections::VecDeque;
use std::sync::Arc;

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::{Mutex, broadcast, mpsc, oneshot};
use tokio_tungstenite::tungstenite::protocol::Message as WsMessage;
use zenui_provider_api::{ClientMessage, ServerMessage};

/// Max number of buffered messages to replay to a new subscriber. Bounded
/// so a long-running daemon can't grow the buffer without limit.
const REPLAY_BUFFER_CAP: usize = 512;

/// Shared state for the proxy. Cloneable so it can be stashed in Tauri
/// managed state and accessed from multiple async commands concurrently.
#[derive(Clone)]
pub struct DaemonProxy {
    /// Outbound queue to the writer task.
    outbound_tx: mpsc::UnboundedSender<WsMessage>,
    /// Shared state: buffered messages + broadcast channel.
    shared: Arc<Mutex<SharedState>>,
    /// FIFO queue of pending `handle_message` responders.
    pending: Arc<Mutex<VecDeque<oneshot::Sender<ServerMessage>>>>,
}

struct SharedState {
    /// Ring buffer of every message received from the daemon since the
    /// proxy opened. Replayed to every new subscriber so they catch up on
    /// events that arrived before they connected.
    replay_buffer: VecDeque<ServerMessage>,
    /// Broadcast of every ServerMessage received from the daemon.
    broadcast_tx: broadcast::Sender<ServerMessage>,
}

impl DaemonProxy {
    /// Open a WebSocket connection to the daemon and spawn the reader /
    /// writer tasks. Returns once the connection is established; the
    /// first `Welcome` message arrives shortly after via the reader.
    pub async fn connect(ws_url: &str) -> Result<Self> {
        let (ws_stream, _) = tokio_tungstenite::connect_async(ws_url)
            .await
            .with_context(|| format!("connect WebSocket {}", ws_url))?;

        let (mut sink, mut stream) = ws_stream.split();

        let (outbound_tx, mut outbound_rx) = mpsc::unbounded_channel::<WsMessage>();
        let (broadcast_tx, _) = broadcast::channel::<ServerMessage>(1024);
        let shared = Arc::new(Mutex::new(SharedState {
            replay_buffer: VecDeque::with_capacity(REPLAY_BUFFER_CAP),
            broadcast_tx,
        }));
        let pending: Arc<Mutex<VecDeque<oneshot::Sender<ServerMessage>>>> =
            Arc::new(Mutex::new(VecDeque::new()));

        // Writer task: pulls from outbound_rx and pushes to the WebSocket.
        tokio::spawn(async move {
            while let Some(msg) = outbound_rx.recv().await {
                if let Err(err) = sink.send(msg).await {
                    tracing::warn!("daemon proxy: write failed: {err}");
                    break;
                }
            }
        });

        // Reader task: parses incoming WS messages, buffers + broadcasts
        // them, and resolves pending responses.
        let shared_reader = shared.clone();
        let pending_reader = pending.clone();
        tokio::spawn(async move {
            while let Some(frame) = stream.next().await {
                let text = match frame {
                    Ok(WsMessage::Text(text)) => text,
                    Ok(WsMessage::Close(_)) | Err(_) => break,
                    Ok(_) => continue,
                };
                let message: ServerMessage = match serde_json::from_str(&text) {
                    Ok(m) => m,
                    Err(err) => {
                        tracing::warn!("daemon proxy: parse error: {err}");
                        continue;
                    }
                };

                // Atomically: push into replay buffer + broadcast to live
                // subscribers. Holding the lock across both ensures the
                // subscribe() snapshot can never observe a half-applied
                // state (we'd either miss or duplicate events otherwise).
                {
                    let mut state = shared_reader.lock().await;
                    if state.replay_buffer.len() >= REPLAY_BUFFER_CAP {
                        state.replay_buffer.pop_front();
                    }
                    state.replay_buffer.push_back(message.clone());
                    let _ = state.broadcast_tx.send(message.clone());
                }

                // If this is a response (not an unsolicited broadcast),
                // resolve the oldest pending request.
                if !is_broadcast_only(&message) {
                    let mut queue = pending_reader.lock().await;
                    if let Some(tx) = queue.pop_front() {
                        let _ = tx.send(message);
                    }
                }
            }

            tracing::warn!("daemon proxy: reader loop exited");
        });

        Ok(Self {
            outbound_tx,
            shared,
            pending,
        })
    }

    /// Send a `ClientMessage` to the daemon and await the matching
    /// response.
    pub async fn send(&self, message: ClientMessage) -> Option<ServerMessage> {
        let payload = match serde_json::to_string(&message) {
            Ok(p) => p,
            Err(err) => {
                tracing::warn!("daemon proxy: serialize error: {err}");
                return None;
            }
        };

        let (resp_tx, resp_rx) = oneshot::channel::<ServerMessage>();
        {
            let mut queue = self.pending.lock().await;
            queue.push_back(resp_tx);
        }

        if self
            .outbound_tx
            .send(WsMessage::Text(payload.into()))
            .is_err()
        {
            return None;
        }

        match tokio::time::timeout(std::time::Duration::from_secs(60), resp_rx).await {
            Ok(Ok(msg)) => Some(msg),
            Ok(Err(_)) => None,
            Err(_) => {
                tracing::warn!("daemon proxy: response timeout");
                None
            }
        }
    }

    /// Subscribe to all incoming messages. Returns the buffered replay
    /// (every message seen since the proxy opened) AND a live broadcast
    /// receiver for future messages, both snapshotted atomically so no
    /// events slip through the gap.
    pub async fn subscribe(
        &self,
    ) -> (Vec<ServerMessage>, broadcast::Receiver<ServerMessage>) {
        let state = self.shared.lock().await;
        let replay: Vec<ServerMessage> = state.replay_buffer.iter().cloned().collect();
        let receiver = state.broadcast_tx.subscribe();
        (replay, receiver)
    }
}

/// `Welcome` and `Event` are always broadcast to every subscriber and
/// are never responses to a ClientMessage — we must never pop a pending
/// resolver for them. Other types (Snapshot, Pong, Ack, SessionLoaded,
/// SessionCreated, Error, ArchivedSessionsList) flow both as responses
/// and as broadcasts.
fn is_broadcast_only(message: &ServerMessage) -> bool {
    matches!(
        message,
        ServerMessage::Welcome { .. } | ServerMessage::Event { .. }
    )
}
