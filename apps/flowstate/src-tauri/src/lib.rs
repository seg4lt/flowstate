use std::sync::Arc;
use std::time::Duration;

use tauri::Manager;
use zenui_daemon_core::{
    DaemonConfig, DaemonLifecycle, Transport, bootstrap_core_async, graceful_shutdown,
    transport_tauri,
};
use zenui_provider_api::ProviderAdapter;
use zenui_provider_claude_cli::ClaudeCliAdapter;
use zenui_provider_claude_sdk::ClaudeSdkAdapter;
use zenui_provider_codex::CodexAdapter;
use zenui_provider_github_copilot::GitHubCopilotAdapter;
use zenui_provider_github_copilot_cli::GitHubCopilotCliAdapter;
use zenui_runtime_core::ConnectionObserver;
use zenui_usage_store::UsageStore;
use transport_tauri::TauriTransport;

mod fatal;
use fatal::FatalExpect;

mod lock;

mod pty;
use pty::PtyManager;

mod shell_env;

mod user_config;
use user_config::UserConfigStore;

mod usage;

mod git;
use git::DiffTasks;

mod code;
mod editor;
mod tracing_setup;

struct AppLifecycle {
    lifecycle: Arc<DaemonLifecycle>,
}

/// Resolved cross-platform app data dir for Flowstate — the same
/// directory the daemon and user_config sqlite live under. Surfaced
/// to the Settings UI as a read-only row so users can copy the
/// path and open it in Finder / Explorer / their terminal.
#[tauri::command]
fn get_app_data_dir(app: tauri::AppHandle) -> Result<String, String> {
    app.path()
        .app_data_dir()
        .map_err(|e| format!("resolve app data dir: {e}"))
        .map(|p| p.to_string_lossy().to_string())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tracing_setup::init_tracing();

    // Enrich the process env with the user's login shell's PATH
    // (and friends) before anything else boots. Must happen before
    // any thread spawns — tauri, tokio workers, pty readers — so
    // every downstream `Command::spawn` (integrated terminal,
    // open_in_editor, git subcommands) inherits a PATH that
    // contains Homebrew, mise, nvm, cargo, bun, etc. See the module
    // doc on `shell_env` for the rationale.
    shell_env::hydrate_from_login_shell();

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        // Auto-updater + process plugins. The frontend calls
        // `check()` on startup (see src/main.tsx) and from a
        // manual button in Settings; on accept it
        // `downloadAndInstall()`s and then `relaunch()`s the
        // app. Manifests are served from the public release
        // repo and verified against the embedded pubkey.
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .manage(PtyManager::new())
        .manage(DiffTasks::default())
        .setup(|app| {
            let app_handle = app.handle().clone();
            let fatal_handle = app_handle.clone();

            // Cross-platform per-user data directory. Tauri resolves
            // this to:
            //   - macOS:   ~/Library/Application Support/<bundle.id>/
            //   - Linux:   ~/.local/share/<bundle.id>/
            //   - Windows: %APPDATA%/<bundle.id>/
            // Everything flowstate owns — daemon SQLite + threads dir +
            // the app's own user_config sqlite — lives under here.
            let flowstate_root = app
                .path()
                .app_data_dir()
                .fatal(&fatal_handle, "resolve app data dir");
            std::fs::create_dir_all(&flowstate_root)
                .fatal(&fatal_handle, "create app data dir");
            std::fs::create_dir_all(flowstate_root.join("threads")).ok();

            // Open the flowstate-app-owned user config store. Lives in
            // its own file at <app_data_dir>/user_config.sqlite — a
            // separate database from the daemon's. SDK and app each
            // own their own SQLite; nothing about app-level UI config
            // belongs in the daemon's schema.
            let user_config_store =
                UserConfigStore::open(&flowstate_root).fatal(&fatal_handle, "open user_config store");
            app.manage(user_config_store);

            // Open the usage analytics store — a *third* sqlite file
            // at <app_data_dir>/usage.sqlite that backs the in-app
            // Usage dashboard. Kept separate from user_config so
            // write-heavy per-turn recording never contends with the
            // tiny hot-path config reads, and deleting one file
            // (reset stats) doesn't destroy the other. Opened twice:
            // once for Tauri-managed state so `#[tauri::command]`
            // extractors can borrow it, and once for the subscriber
            // task that writes per-turn rows on
            // `RuntimeEvent::TurnCompleted`. SQLite is happy with
            // multiple handles to the same file — the Mutex around
            // each handle's Connection keeps writes serialized within
            // that handle, and sqlite's own file locking handles
            // cross-handle concurrency. Failure is non-fatal: we log
            // and register a no-op sentinel store so command
            // invocations return an empty dashboard instead of
            // panicking the setup chain.
            let usage_writer: Option<UsageStore> = match UsageStore::open(&flowstate_root) {
                Ok(store) => Some(store),
                Err(e) => {
                    tracing::error!(
                        "failed to open usage store (writer), disabling analytics recording: {e}"
                    );
                    None
                }
            };
            match UsageStore::open(&flowstate_root) {
                Ok(reader) => {
                    app.manage(reader);
                }
                Err(e) => {
                    tracing::error!(
                        "failed to open usage store (reader): {e}; dashboard will error"
                    );
                }
            }

            let transport = Box::new(TauriTransport::new(app_handle));

            let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);

            // Run the daemon on Tauri's existing tokio runtime so the
            // process has exactly one thread pool. The previous shape
            // (std::thread::spawn + bootstrap_core's own runtime) was
            // a workaround for "cannot start a runtime from within a
            // runtime"; bootstrap_core_async removes that need by
            // letting us share the host runtime.
            // Clone once for the bootstrap task so we can surface
            // async startup failures through the same native error
            // dialog that the sync path uses.
            let async_fatal = fatal_handle.clone();

            tauri::async_runtime::spawn(async move {
                let mut config = DaemonConfig::with_project_root(flowstate_root.clone());
                config.idle_timeout = Duration::MAX;
                // Advertised to every connected client via the
                // Bootstrap wire payload. Keeping it here means
                // `runtime-core` never knows its host app's name.
                config.app_name = "Flowstate".to_string();

                // Construct the provider adapters the app wants to
                // expose. Adding or removing providers now lives in a
                // single call site here — `daemon-core` stays
                // provider-agnostic. Per-provider `default_enabled()`
                // decides which are on out of the box.
                config.adapters = vec![
                    Arc::new(ClaudeSdkAdapter::new(flowstate_root.clone()))
                        as Arc<dyn ProviderAdapter>,
                    Arc::new(ClaudeCliAdapter::new(flowstate_root.clone())),
                    Arc::new(CodexAdapter::new(flowstate_root.clone())),
                    Arc::new(GitHubCopilotAdapter::new(flowstate_root.clone())),
                    Arc::new(GitHubCopilotCliAdapter::new(flowstate_root.clone())),
                ];

                let core = match bootstrap_core_async(&config).await {
                    Ok(c) => c,
                    Err(e) => fatal::show_and_exit(&async_fatal, "daemon bootstrap", e),
                };

                // Usage analytics subscriber. Runs for the life of
                // the daemon, filtering the RuntimeEvent broadcast
                // for TurnCompleted events and writing one row per
                // turn to the usage sqlite. Missing this task is
                // never fatal — a broadcast lag skips some telemetry
                // but never corrupts runtime state. We subscribe
                // BEFORE the transport's serve() so no event is lost
                // between bootstrap and the first client connect.
                if let Some(writer) = usage_writer {
                    usage::spawn_turn_completed_subscriber(&core.runtime_core, writer);
                }

                let bound = match transport.bind() {
                    Ok(b) => b,
                    Err(e) => fatal::show_and_exit(&async_fatal, "transport bind", e),
                };
                let observer: Arc<dyn ConnectionObserver> = core.lifecycle.clone();
                let handle = match bound.serve(core.runtime_core.clone(), observer) {
                    Ok(h) => h,
                    Err(e) => fatal::show_and_exit(&async_fatal, "transport serve", e),
                };

                // Signal main thread AFTER serve() has managed TauriDaemonState.
                // This guarantees the connect command can access it.
                if let Err(e) = ready_tx.send(core.lifecycle.clone()) {
                    fatal::show_and_exit(&async_fatal, "signal daemon ready", e);
                }

                core.lifecycle.wait_for_shutdown().await;

                let _ = graceful_shutdown(
                    core.runtime_core.clone(),
                    core.lifecycle.clone(),
                    config.shutdown_grace,
                )
                .await;

                handle.shutdown().await;
            });

            // Block until serve() is done and TauriDaemonState is managed.
            let lifecycle = ready_rx.recv().fatal(&fatal_handle, "daemon start");
            app.manage(AppLifecycle { lifecycle });

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            transport_tauri::commands::connect,
            transport_tauri::commands::handle_message,
            git::branch::get_git_branch,
            git::branch::list_git_branches,
            git::worktree::list_git_worktrees,
            git::worktree::git_checkout,
            git::branch::git_create_branch,
            git::branch::git_delete_branch,
            git::worktree::create_git_worktree,
            git::worktree::remove_git_worktree,
            git::branch::resolve_git_root,
            git::branch::path_exists,
            git::diff::get_git_diff_summary,
            git::diff::get_git_diff_file,
            git::diff_stream::watch_git_diff_summary,
            git::diff_stream::stop_git_diff_summary,
            code::list_project_files,
            code::read_project_file,
            code::search_file_contents,
            editor::open_in_editor,
            pty::pty_open,
            pty::pty_write,
            pty::pty_resize,
            pty::pty_pause,
            pty::pty_resume,
            pty::pty_kill,
            user_config::get_user_config,
            user_config::set_user_config,
            user_config::set_session_display,
            user_config::get_session_display,
            user_config::list_session_display,
            user_config::delete_session_display,
            user_config::set_project_display,
            user_config::get_project_display,
            user_config::list_project_display,
            user_config::delete_project_display,
            user_config::set_project_worktree,
            user_config::get_project_worktree,
            user_config::list_project_worktree,
            user_config::delete_project_worktree,
            usage::get_usage_summary,
            usage::get_usage_timeseries,
            usage::get_top_sessions,
            get_app_data_dir,
        ])
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::Destroyed = event {
                if let Some(pty) = window.try_state::<PtyManager>() {
                    pty.kill_all();
                }
                if let Some(state) = window.try_state::<AppLifecycle>() {
                    state.lifecycle.request_shutdown();
                }
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
