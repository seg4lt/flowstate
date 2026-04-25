//! Tauri commands that drive the streaming transport.
//!
//! These commands are registered in the app binary's `invoke_handler`
//! and access [`TauriDaemonState`] from Tauri managed state.

use tauri::ipc::Channel;
use tauri::WebviewWindow;
use tokio::sync::{broadcast, oneshot};
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
    webview: WebviewWindow,
    state: tauri::State<'_, TauriDaemonState>,
    on_event: Channel<ServerMessage>,
) -> Result<(), String> {
    state.observer.on_client_connected();

    // Subscribe BEFORE bootstrap — any event published between the
    // snapshot read and subscribe would otherwise be lost.
    let mut rx = state.runtime.subscribe();

    // Per-webview destruction signal. Without this, when a popout
    // window is destroyed the connect loop sits in `rx.recv().await`
    // until the next runtime broadcast — and because the `on_event`
    // Channel<T> internally holds a strong reference to the
    // WebviewWindow, the WKWebView's WebContent subprocess can't be
    // released until this task exits. On an idle app that means
    // never. Wiring a per-webview WindowEvent::Destroyed listener
    // here lets us drop the Channel promptly and let macOS reap the
    // webview process.
    let (close_tx, mut close_rx) = oneshot::channel::<()>();
    let close_tx = std::sync::Mutex::new(Some(close_tx));
    let webview_label = webview.label().to_string();
    // Only fire on Destroyed, never CloseRequested. The main window
    // hits CloseRequested on every red-X click (the lib.rs handler
    // intercepts it and hides instead) — exiting connect there would
    // strand the rehydrated main window with no event stream after a
    // dock-icon reopen. Popouts go through destroy() in lib.rs, which
    // reliably emits Destroyed.
    webview.on_window_event(move |event| {
        if matches!(event, tauri::WindowEvent::Destroyed) {
            if let Ok(mut guard) = close_tx.lock() {
                if let Some(sender) = guard.take() {
                    let _ = sender.send(());
                }
            }
        }
    });

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
        tokio::select! {
            biased;
            _ = &mut close_rx => {
                tracing::debug!(label = %webview_label, "connect loop exiting: webview destroyed");
                break;
            }
            recv = rx.recv() => match recv {
                Ok(event) => {
                    if on_event.send(ServerMessage::Event { event }).is_err() {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(
                        "streaming channel lagged by {n} events; sending snapshot + active session reseed"
                    );
                    let snapshot = state.runtime.snapshot().await;
                    if on_event.send(ServerMessage::Snapshot { snapshot }).is_err() {
                        break;
                    }
                    // Snapshot only carries summaries — chat-view needs
                    // the full live turn list (including any in-flight
                    // tool calls that were dropped from the broadcast
                    // queue) to reconcile, so push a SessionLoaded for
                    // every session that's currently mid-turn.
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
            },
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
