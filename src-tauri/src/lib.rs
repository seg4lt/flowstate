use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use tauri::Manager;
use tracing_subscriber::EnvFilter;
use zenui_daemon_core::{
    DaemonConfig, DaemonLifecycle, Transport, bootstrap_core, graceful_shutdown,
};
use zenui_runtime_core::ConnectionObserver;
use zenui_transport_tauri::TauriTransport;

/// Return the current git branch for `path`, or `None` if `path` is not
/// inside a git repo (or git itself fails). Used by the chat header to
/// surface the active branch under the thread title.
#[tauri::command]
fn get_git_branch(path: String) -> Option<String> {
    let output = Command::new("git")
        .args(["-C", &path, "rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let branch = String::from_utf8(output.stdout).ok()?.trim().to_string();
    if branch.is_empty() { None } else { Some(branch) }
}

/// Lightweight per-file entry returned by `get_git_diff_summary`.
/// Just path + line stats — no file contents. Designed so the diff
/// panel can show the full file list immediately without paying the
/// IPC + render cost of every file's before/after content. The
/// expensive content fetch happens lazily, one file at a time,
/// through `get_git_diff_file` when the user expands a row.
#[derive(Serialize)]
struct GitFileSummary {
    path: String,
    status: String,
    additions: u32,
    deletions: u32,
}

/// Full before/after for a single file, returned by
/// `get_git_diff_file`. `before` is HEAD content (empty for newly
/// added or untracked files); `after` is on-disk content (empty
/// for deleted files). Capped at GIT_DIFF_MAX_FILE_BYTES.
#[derive(Serialize)]
struct GitFileContents {
    before: String,
    after: String,
}

/// Maximum file size we'll inline into a diff payload. Keeps the
/// Tauri-bridge JSON message bounded so a session that touches a
/// 50 MB generated artifact doesn't lock up the frontend.
const GIT_DIFF_MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;

fn read_file_capped(abs: &Path) -> String {
    if let Ok(meta) = std::fs::metadata(abs) {
        if meta.len() > GIT_DIFF_MAX_FILE_BYTES {
            return format!(
                "<file too large to inline: {} bytes>",
                meta.len()
            );
        }
    }
    std::fs::read_to_string(abs).unwrap_or_default()
}

fn git_show_head(repo: &str, file: &str) -> String {
    let Ok(output) = Command::new("git")
        .args(["-C", repo, "show", &format!("HEAD:{file}")])
        .output()
    else {
        return String::new();
    };
    if !output.status.success() {
        return String::new();
    }
    if output.stdout.len() as u64 > GIT_DIFF_MAX_FILE_BYTES {
        return format!(
            "<file too large to inline: {} bytes>",
            output.stdout.len()
        );
    }
    String::from_utf8(output.stdout).unwrap_or_default()
}

/// Cheap one-shot summary: every file that differs between the
/// working tree and HEAD (plus untracked files), returned with line
/// stats only — no contents. Drives the diff panel's file list and
/// the Show Diff button's +X / −Y badge. We use `git diff --numstat`
/// for tracked changes (git already counts the lines for us) and
/// `git ls-files --others` for untracked files (we count their
/// lines on the rust side since git doesn't compute stats for files
/// that aren't tracked yet).
#[tauri::command]
fn get_git_diff_summary(path: String) -> Vec<GitFileSummary> {
    let mut entries: Vec<GitFileSummary> = Vec::new();
    let project_path = Path::new(&path);
    if !project_path.is_dir() {
        return entries;
    }

    // Tracked changes via `git diff HEAD --numstat -z`.
    // Format with `-z`:
    //   For non-renames:  "<adds>\t<dels>\t<path>\0"
    //   For renames:      "<adds>\t<dels>\t\0<old>\0<new>\0"
    // Binary files report `-` for both counts; we treat as 0/0.
    if let Ok(output) = Command::new("git")
        .args(["-C", &path, "diff", "HEAD", "--numstat", "-z"])
        .output()
    {
        if output.status.success() {
            let raw = String::from_utf8_lossy(&output.stdout);
            // Walk the stream chunk-by-chunk so we can pick up the
            // extra rename path that follows the leading record.
            let mut iter = raw.split('\0').peekable();
            while let Some(chunk) = iter.next() {
                if chunk.is_empty() {
                    continue;
                }
                let parts: Vec<&str> = chunk.splitn(3, '\t').collect();
                if parts.len() < 2 {
                    continue;
                }
                let adds = parts[0].parse::<u32>().unwrap_or(0);
                let dels = parts[1].parse::<u32>().unwrap_or(0);
                let path_field = parts.get(2).copied().unwrap_or("");
                let (file_path, status) = if path_field.is_empty() {
                    // Rename: next chunk is the old path, the one
                    // after that is the new path. We display the
                    // new path and tag the row "renamed".
                    let _old = iter.next().unwrap_or("");
                    let new_path = iter.next().unwrap_or("");
                    (new_path.to_string(), "renamed")
                } else {
                    let status = if adds == 0 && dels > 0 {
                        "deleted"
                    } else if dels == 0 && adds > 0 {
                        "added"
                    } else {
                        "modified"
                    };
                    (path_field.to_string(), status)
                };
                if file_path.is_empty() {
                    continue;
                }
                entries.push(GitFileSummary {
                    path: file_path,
                    status: status.to_string(),
                    additions: adds,
                    deletions: dels,
                });
            }
        }
    }

    // Untracked (new) files honoring .gitignore. `git diff HEAD`
    // doesn't see these so we list them separately and count the
    // lines ourselves.
    if let Ok(output) = Command::new("git")
        .args([
            "-C",
            &path,
            "ls-files",
            "--others",
            "--exclude-standard",
            "-z",
        ])
        .output()
    {
        if output.status.success() {
            let raw = String::from_utf8_lossy(&output.stdout);
            for file_path in raw.split('\0').filter(|s| !s.is_empty()) {
                let abs = project_path.join(file_path);
                let additions = match std::fs::read_to_string(&abs) {
                    Ok(c) => {
                        if c.is_empty() {
                            0
                        } else if c.ends_with('\n') {
                            c.matches('\n').count() as u32
                        } else {
                            c.matches('\n').count() as u32 + 1
                        }
                    }
                    Err(_) => 0,
                };
                entries.push(GitFileSummary {
                    path: file_path.to_string(),
                    status: "added".to_string(),
                    additions,
                    deletions: 0,
                });
            }
        }
    }

    entries
}

/// Lazy per-file content fetch. Called from the frontend the moment
/// the user expands a file row in the diff panel. The summary call
/// has already given us the path; this fills in before+after only
/// when needed, so we never ship the contents of files the user
/// doesn't actually look at.
#[tauri::command]
fn get_git_diff_file(path: String, file: String) -> GitFileContents {
    let project_path = Path::new(&path);
    let abs = project_path.join(&file);
    let after = if abs.exists() {
        read_file_capped(&abs)
    } else {
        String::new()
    };
    let before = git_show_head(&path, &file);
    GitFileContents { before, after }
}

struct AppLifecycle {
    lifecycle: Arc<DaemonLifecycle>,
}

/// Initialize tracing. Debug builds stream to stderr so `cargo tauri dev`
/// surfaces logs in the terminal; release builds keep writing to a log
/// file alongside the daemon log.
fn init_tracing() {
    let env_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("flowzen=info,zenui=info,warn"));

    if cfg!(debug_assertions) {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(env_filter)
            .with_writer(std::io::stderr)
            .try_init();
        eprintln!("flowzen: dev build, logging to stderr");
        return;
    }

    let log_dir = std::env::temp_dir().join("flowzen").join("logs");
    let _ = std::fs::create_dir_all(&log_dir);
    let log_path = log_dir.join("flowzen.log");

    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .ok();

    if let Some(file) = file {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(env_filter)
            .with_writer(std::sync::Mutex::new(file))
            .with_ansi(false)
            .try_init();
        eprintln!("flowzen: logging to {}", log_path.display());
    } else {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(env_filter)
            .try_init();
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    init_tracing();

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
            get_git_branch,
            get_git_diff_summary,
            get_git_diff_file,
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
