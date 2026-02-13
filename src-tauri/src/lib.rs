use std::sync::Arc;
use std::time::Duration;

use tauri::Manager;
use zenui_daemon_core::{
    DaemonConfig, DaemonLifecycle, Transport, bootstrap_core, graceful_shutdown,
};
use zenui_runtime_core::ConnectionObserver;
use zenui_transport_tauri::TauriTransport;

struct AppLifecycle {
    lifecycle: Arc<DaemonLifecycle>,
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            let app_handle = app.handle().clone();
            let flowzen_root = dirs::home_dir()
                .expect("no home directory")
                .join(".flowzen");
            std::fs::create_dir_all(&flowzen_root)
                .expect("failed to create ~/.flowzen");
            std::fs::create_dir_all(flowzen_root.join("threads")).ok();

            let transport = Box::new(TauriTransport::new(app_handle));

            let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);

            std::thread::spawn(move || {
                let mut config = DaemonConfig::with_project_root(flowzen_root);
                config.idle_timeout = Duration::MAX;

                let core = bootstrap_core(&config).expect("daemon bootstrap failed");

                core.tokio_runtime.block_on(async {
                    let bound = transport.bind().expect("transport bind failed");
                    let observer: Arc<dyn ConnectionObserver> = core.lifecycle.clone();
                    let handle = bound
                        .serve(core.runtime_core.clone(), observer)
                        .expect("transport serve failed");

                    // Signal main thread AFTER serve() has managed TauriDaemonState.
                    // This guarantees the connect command can access it.
                    ready_tx
                        .send(core.lifecycle.clone())
                        .expect("failed to signal ready");

                    core.lifecycle.wait_for_shutdown().await;

                    let _ = graceful_shutdown(
                        core.runtime_core.clone(),
                        core.lifecycle.clone(),
                        config.shutdown_grace,
                    )
                    .await;

                    handle.shutdown().await;
                });
            });

            // Block until serve() is done and TauriDaemonState is managed.
            let lifecycle = ready_rx.recv().expect("daemon failed to start");
            app.manage(AppLifecycle { lifecycle });

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            zenui_transport_tauri::commands::connect,
            zenui_transport_tauri::commands::handle_message,
        ])
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::Destroyed = event {
                if let Some(state) = window.try_state::<AppLifecycle>() {
                    state.lifecycle.request_shutdown();
                }
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
