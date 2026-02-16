pub mod daemon_proxy;
pub mod server_entry;

use std::time::Instant;

use tauri::Manager;
use tauri::ipc::Channel;
use tracing_subscriber::EnvFilter;
use zenui_daemon_client::{ClientConfig, TransportPreference, connect_or_spawn};
use zenui_provider_api::{ClientMessage, ServerMessage};

use crate::daemon_proxy::DaemonProxy;

/// Initialize tracing to stderr + a log file so the user can see what the
/// Tauri backend is doing. The log file lives alongside the daemon log.
fn init_tracing() {
    let log_dir = std::env::temp_dir().join("flowzen").join("logs");
    let _ = std::fs::create_dir_all(&log_dir);
    let log_path = log_dir.join("flowzen-ui.log");

    // File writer (append).
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .ok();

    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("flowzen=info,zenui=info,warn"));

    if let Some(file) = file {
        let subscriber = tracing_subscriber::fmt()
            .with_env_filter(env_filter)
            .with_writer(std::sync::Mutex::new(file))
            .with_ansi(false);
        let _ = subscriber.try_init();
        eprintln!("flowzen: logging to {}", log_path.display());
    } else {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(env_filter)
            .try_init();
    }
}

/// Proxy a client message to the daemon over the shared WebSocket.
#[tauri::command]
async fn handle_message(
    state: tauri::State<'_, DaemonProxy>,
    message: ClientMessage,
) -> Result<Option<ServerMessage>, String> {
    Ok(state.send(message).await)
}

/// Subscribe to the daemon's event stream. First replays every message
/// the proxy has buffered since it opened the daemon connection (so the
/// frontend catches up on startup events like ProviderHealthUpdated),
/// then forwards live broadcasts until the channel closes.
#[tauri::command]
async fn connect(
    state: tauri::State<'_, DaemonProxy>,
    on_event: Channel<ServerMessage>,
) -> Result<(), String> {
    let (replay, mut rx) = state.subscribe().await;

    tracing::info!(replay_count = replay.len(), "connect: replaying buffered messages");
    for msg in replay {
        if on_event.send(msg).is_err() {
            return Ok(());
        }
    }

    loop {
        match rx.recv().await {
            Ok(message) => {
                if on_event.send(message).is_err() {
                    break;
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                tracing::warn!("connect subscriber lagged by {n} messages");
                continue;
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
        }
    }

    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    init_tracing();

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            let total_start = Instant::now();
            tracing::info!("flowzen: starting up");

            let flowzen_root = dirs::home_dir()
                .expect("no home directory")
                .join(".flowzen");
            std::fs::create_dir_all(&flowzen_root)
                .expect("failed to create ~/.flowzen");

            let canonical = std::fs::canonicalize(&flowzen_root)
                .expect("failed to canonicalize ~/.flowzen");

            let config = ClientConfig {
                project_root: canonical.clone(),
                server_binary: None,
                spawn_timeout: std::time::Duration::from_secs(15),
                health_timeout: std::time::Duration::from_secs(2),
                preferred_transport: TransportPreference::Http,
            };

            tracing::info!(project_root = %canonical.display(), "calling connect_or_spawn");
            let spawn_start = Instant::now();
            let handle = connect_or_spawn(&config)
                .expect("failed to connect to or spawn flowzen daemon");
            tracing::info!(
                elapsed_ms = spawn_start.elapsed().as_millis() as u64,
                pid = handle.pid,
                "connect_or_spawn returned"
            );

            let http = handle
                .as_http()
                .expect("flowzen daemon has no HTTP transport");
            let ws_url = http.ws_url.to_string();

            tracing::info!(http_base = %http.http_base, ws_url = %ws_url, "daemon address");

            // Open the proxy WebSocket. This blocks the setup closure
            // until the connection is established, which is fine — it
            // happens once at startup and is fast on loopback.
            tracing::info!("opening daemon proxy WebSocket");
            let ws_start = Instant::now();
            let proxy = tauri::async_runtime::block_on(DaemonProxy::connect(&ws_url))
                .expect("failed to open daemon proxy WebSocket");
            tracing::info!(
                elapsed_ms = ws_start.elapsed().as_millis() as u64,
                "daemon proxy WebSocket opened"
            );

            app.manage(proxy);

            tracing::info!(
                total_ms = total_start.elapsed().as_millis() as u64,
                "flowzen: startup complete"
            );

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![handle_message, connect])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
