//! Tauri commands that drive the streaming transport.
//!
//! These commands are registered in the app binary's `invoke_handler`
//! and access [`TauriDaemonState`] from Tauri managed state.

use tauri::ipc::Channel;
use tokio::sync::broadcast;
use zenui_provider_api::{ClientMessage, ServerMessage};

use crate::TauriDaemonState;

/// Streaming connection. The frontend creates a `Channel` and passes
/// it here. We send `Welcome` immediately, then stream
/// `ServerMessage::Event` for every `RuntimeEvent` until the channel
/// closes or the daemon shuts down.
///
/// This mirrors the WebSocket handler in `transport-http`: subscribe
/// before bootstrap (no event gap), wrap events in `ServerMessage`,
/// and recover from lag with a fresh snapshot.
#[tauri::command]
pub async fn connect(
    state: tauri::State<'_, TauriDaemonState>,
    on_event: Channel<ServerMessage>,
) -> Result<(), String> {
    state.observer.on_client_connected();

    // Subscribe BEFORE bootstrap — any event published between the
    // snapshot read and subscribe would otherwise be lost.
    let mut rx = state.runtime.subscribe();

    let welcome = ServerMessage::Welcome {
        bootstrap: state.runtime.bootstrap(String::new()).await,
    };
    if on_event.send(welcome).is_err() {
        state.observer.on_client_disconnected();
        return Ok(());
    }

    loop {
        match rx.recv().await {
            Ok(event) => {
                if on_event
                    .send(ServerMessage::Event { event })
                    .is_err()
                {
                    break;
                }
            }
            Err(broadcast::error::RecvError::Lagged(n)) => {
                tracing::warn!("streaming channel lagged by {n} events; sending snapshot");
                let snapshot = state.runtime.snapshot().await;
                if on_event
                    .send(ServerMessage::Snapshot { snapshot })
                    .is_err()
                {
                    break;
                }
            }
            Err(broadcast::error::RecvError::Closed) => break,
        }
    }

    state.observer.on_client_disconnected();
    Ok(())
}

/// Dispatch a `ClientMessage` to the runtime. Returns the
/// `ServerMessage` response (if any).
#[tauri::command]
pub async fn handle_message(
    state: tauri::State<'_, TauriDaemonState>,
    message: ClientMessage,
) -> Result<Option<ServerMessage>, String> {
    Ok(state.runtime.handle_client_message(message).await)
}
