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
        // Tauri's transport streams events through a host `Channel<T>`
        // — there is no separate WS endpoint to dial, so advertise
        // `None` rather than an empty string the frontend would have
        // to special-case.
        bootstrap: state.runtime.bootstrap(None).await,
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
                tracing::warn!(
                    "streaming channel lagged by {n} events; sending snapshot + active session reseed"
                );
                let snapshot = state.runtime.snapshot().await;
                if on_event
                    .send(ServerMessage::Snapshot { snapshot })
                    .is_err()
                {
                    break;
                }
                // Snapshot only carries summaries — chat-view needs the
                // full live turn list (including any in-flight tool
                // calls that were dropped from the broadcast queue) to
                // reconcile, so push a SessionLoaded for every session
                // that's currently mid-turn.
                let mut send_failed = false;
                for session in state.runtime.active_session_details().await {
                    if on_event
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
