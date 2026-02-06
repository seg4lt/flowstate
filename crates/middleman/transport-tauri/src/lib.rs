//! Tauri IPC transport for the ZenUI daemon.
//!
//! Implements `zenui_daemon_core::Transport` using Tauri's in-process
//! IPC. The transport manages the daemon lifecycle hooks
//! (connected/disconnected) and exposes the `RuntimeCore` + observer
//! via [`TauriDaemonState`] for Tauri commands to drive streaming.
//!
//! ## Streaming architecture
//!
//! Streaming uses Tauri's [`Channel`] API in a `connect` command:
//!
//! 1. Frontend calls `invoke("connect", { onEvent: channel })`.
//! 2. The command sends `ServerMessage::Welcome`, then loops on
//!    `RuntimeCore::subscribe()`, forwarding each `RuntimeEvent` as
//!    `ServerMessage::Event { event }` through the channel.
//! 3. On broadcast lag, a fresh `ServerMessage::Snapshot` is sent so the
//!    client can re-reconcile.
//! 4. The loop exits when the channel is dropped (frontend disconnect)
//!    or the daemon shuts down.
//!
//! Request messages (frontend → backend) go through a separate
//! `handle_message` Tauri command.
//!
//! ## Registering commands
//!
//! The app binary must register the commands from this crate:
//!
//! ```ignore
//! .invoke_handler(tauri::generate_handler![
//!     zenui_transport_tauri::commands::connect,
//!     zenui_transport_tauri::commands::handle_message,
//! ])
//! ```

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::oneshot;
use zenui_daemon_core::transport::{Bound, Transport, TransportAddressInfo, TransportHandle};
use zenui_runtime_core::{ConnectionObserver, RuntimeCore};

pub mod commands;

/// Shared state stored in Tauri managed state by [`TauriBound::serve`].
/// The streaming commands in [`commands`] access this to reach the
/// runtime and observer.
pub struct TauriDaemonState {
    pub runtime: Arc<RuntimeCore>,
    pub observer: Arc<dyn ConnectionObserver>,
}

/// Tauri IPC transport. Construct with the app handle and pass to
/// `daemon-core`'s transport composition.
pub struct TauriTransport {
    app_handle: tauri::AppHandle,
}

impl TauriTransport {
    pub fn new(app_handle: tauri::AppHandle) -> Self {
        Self { app_handle }
    }
}

impl Transport for TauriTransport {
    fn kind(&self) -> &'static str {
        "tauri-ipc"
    }

    fn bind(self: Box<Self>) -> Result<Box<dyn Bound>> {
        Ok(Box::new(TauriBound {
            app_handle: self.app_handle,
        }))
    }
}

struct TauriBound {
    app_handle: tauri::AppHandle,
}

impl Bound for TauriBound {
    fn kind(&self) -> &'static str {
        "tauri-ipc"
    }

    fn address_info(&self) -> TransportAddressInfo {
        TransportAddressInfo::InProcess
    }

    fn serve(
        self: Box<Self>,
        runtime: Arc<RuntimeCore>,
        observer: Arc<dyn ConnectionObserver>,
    ) -> Result<Box<dyn TransportHandle>> {
        // Store the runtime and observer so Tauri commands can access them.
        use tauri::Manager;
        self.app_handle.manage(TauriDaemonState {
            runtime: runtime.clone(),
            observer: observer.clone(),
        });

        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

        Ok(Box::new(TauriHandle {
            shutdown_tx: Some(shutdown_tx),
            _shutdown_rx: shutdown_rx,
            observer,
        }))
    }
}

struct TauriHandle {
    shutdown_tx: Option<oneshot::Sender<()>>,
    _shutdown_rx: oneshot::Receiver<()>,
    observer: Arc<dyn ConnectionObserver>,
}

#[async_trait]
impl TransportHandle for TauriHandle {
    fn kind(&self) -> &'static str {
        "tauri-ipc"
    }

    fn address_info(&self) -> TransportAddressInfo {
        TransportAddressInfo::InProcess
    }

    async fn shutdown(self: Box<Self>) {
        let mut me = self;
        me.observer.on_client_disconnected();
        if let Some(tx) = me.shutdown_tx.take() {
            let _ = tx.send(());
        }
    }
}

impl Drop for TauriHandle {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
    }
}
