pub mod server_entry;

use tauri::Manager;
use zenui_daemon_client::{ClientConfig, TransportPreference, connect_or_spawn};

#[derive(Clone)]
struct DaemonUrl(String);

#[tauri::command]
fn get_daemon_url(state: tauri::State<'_, DaemonUrl>) -> String {
    state.0.clone()
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            let flowzen_root = dirs::home_dir()
                .expect("no home directory")
                .join(".flowzen");
            std::fs::create_dir_all(&flowzen_root)
                .expect("failed to create ~/.flowzen");

            let canonical = std::fs::canonicalize(&flowzen_root)
                .expect("failed to canonicalize ~/.flowzen");

            let config = ClientConfig {
                project_root: canonical,
                server_binary: None,
                spawn_timeout: std::time::Duration::from_secs(10),
                health_timeout: std::time::Duration::from_millis(500),
                preferred_transport: TransportPreference::Http,
            };

            let handle = connect_or_spawn(&config)
                .expect("failed to connect to or spawn flowzen daemon");

            let http = handle.as_http().expect(
                "flowzen daemon has no HTTP transport",
            );
            let ws_url = http.ws_url.to_string();

            eprintln!("flowzen: connected to daemon at {} (pid={})", http.http_base, handle.pid);

            app.manage(DaemonUrl(ws_url));

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![get_daemon_url])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
