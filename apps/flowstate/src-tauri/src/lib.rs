use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Shutdown-gate atomics for the two-phase Tauri exit flow. Every
/// close path (red traffic light, Cmd+Q, SIGTERM, SIGINT) funnels
/// through `RunEvent::ExitRequested`; the gate checks these two
/// atomics to decide whether to allow the process to exit or to
/// prevent exit and let the daemon task keep running.
///
/// - `SHUTDOWN_STARTED` guards one-shot install of the watchdog
///   thread (prevents multiple watchdogs if the user clicks close
///   several times).
/// - `SHUTDOWN_COMPLETE` is flipped by the daemon task at the very
///   end of its `async move` block, after `graceful_shutdown` has
///   swept every adapter and handle/core/config have been dropped.
///   The next `ExitRequested` observes `true` and allows the actual
///   process termination.
///
/// See the big block comment above the `on_window_event` handler at
/// the bottom of `run()` for the full sequence diagram.
static SHUTDOWN_STARTED: AtomicBool = AtomicBool::new(false);
static SHUTDOWN_COMPLETE: AtomicBool = AtomicBool::new(false);

/// Label of the primary application window declared in
/// `tauri.conf.json`. Popout windows use deterministic labels of the
/// form `thread-<session_id>` (see `popout_window_label`). The
/// `on_window_event` handler and the macOS dock-reopen path both
/// compare against this constant to apply main-window-only policy
/// (hide-on-close, daemon shutdown, dock-icon resurrection) without
/// mistakenly applying it to popouts — which must be free to close
/// and have their WKWebView subprocesses reaped normally.
const MAIN_WINDOW_LABEL: &str = "main";

/// Menu id for the App > "Close Window" item that we wire to ⌘Q in
/// place of muda's PredefinedMenuItem::quit (whose accelerator is
/// hard-wired to ⌘Q → NSApp `terminate:`, sidestepping our
/// `CloseRequested` → hide path). The Builder-level `on_menu_event`
/// handler matches this id and calls `window.close()` on the
/// focused window — same path as the red traffic light. macOS-only.
#[cfg(target_os = "macos")]
const FLOWSTATE_CLOSE_WINDOW_MENU_ID: &str = "flowstate-close-window";

use serde::Serialize;
use tauri::ipc::Channel;
use tauri::Emitter;
use tauri::Manager;
use tauri::State;
use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Modifiers, Shortcut, ShortcutState};
use tracing_subscriber::EnvFilter;
use transport_tauri::TauriTransport;
use zenui_daemon_core::{
    bootstrap_core_async, graceful_shutdown, transport_tauri, DaemonConfig, DaemonLifecycle,
    Transport,
};
use zenui_runtime_core::ConnectionObserver;

mod pty;
use pty::{PtyEvent, PtyId, PtyManager};

mod shell_env;

mod daemon_client;
mod file_index;
mod install_cli;
mod loopback_http;
mod orphan_scan;
use daemon_client::DaemonBaseUrl;
use file_index::{ContentSearchOptions, FileIndexRegistry, ProjectFileListing, SearchTasks};
// Phase 3 — user_config / usage / orchestration_adapters / git_worktree
// moved into `flowstate-app-layer` so the future daemon bin can link
// them without pulling Tauri. The Tauri crate now depends on the
// app-layer crate instead of mod-ing the files in-tree.
use flowstate_app_layer::git_worktree::{
    create_git_worktree_internal, list_git_worktrees_sync, resolve_git_root_sync, GitWorktree,
};
use flowstate_app_layer::orchestration_adapters::{
    AppMetadataProviderImpl, WorktreeProvisionerImpl,
};
use flowstate_app_layer::usage::{
    TopSessionRow, UsageAgentPayload, UsageBucket, UsageEvent, UsageGroupBy, UsageRange,
    UsageStore, UsageSummaryPayload, UsageTimeseriesPayload,
};
use flowstate_app_layer::user_config::{
    ProjectDisplay, ProjectWorktree, SessionDisplay, UserConfigStore,
};
use tokio::sync::broadcast::error::RecvError;
use zenui_provider_api::{OrchestrationIpcHandle, ProviderAdapter, RuntimeEvent};
use zenui_provider_claude_sdk::ClaudeSdkAdapter;
use zenui_provider_codex::CodexAdapter;
use zenui_provider_github_copilot::GitHubCopilotAdapter;
use zenui_provider_opencode::OpenCodeAdapter;

use std::collections::HashMap;

/// Cheap "does this filesystem entry exist?" probe. Used by the
/// chat view to flip a worktree thread into read-only mode when
/// the user has removed its folder out from under flowstate — the
/// banner explains why the composer is disabled and the existing
/// archived-readonly infra is reused to enforce it.
#[tauri::command]
fn path_exists(path: String) -> bool {
    Path::new(&path).exists()
}

/// Hard quit — fully terminate the process, including the daemon and
/// every PTY/subprocess it owns. Bound to ⌘⌥Q in the frontend
/// keyboard registry; the menu's ⌘Q (custom "Close Window" item)
/// only hides the main window on macOS, so this is the only
/// in-app affordance for "actually exit".
///
/// Implementation: just `app_handle.exit(0)`. The `RunEvent::ExitRequested`
/// gate at the bottom of `run()` is fully idempotent — it raises
/// `lifecycle.request_shutdown()`, kills the PTY pool, and installs
/// the 10 s watchdog before letting the daemon task drain. So this
/// command intentionally does NOT pre-call `request_shutdown()`
/// itself; doing so would just duplicate work the gate already does
/// (and risk drifting from the canonical sequence if it ever
/// changes).
#[tauri::command]
fn quit_app(app: tauri::AppHandle) {
    tracing::info!("quit_app: user-triggered shutdown via ⌘⌥Q");
    app.exit(0);
}

/// Payload returned by `read_file_as_base64` — enough for the chat
/// composer to turn a dropped file into an `AttachedImage`-shaped
/// attachment without re-guessing the media type on the TS side.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DroppedFilePayload {
    /// Display filename (basename of the absolute path).
    name: String,
    /// Inferred MIME, e.g. `image/png`, `audio/mpeg`, `video/mp4`.
    /// Best-effort from the extension; empty string when unknown.
    media_type: String,
    /// Raw base64 (no `data:` prefix). Mirrors `AttachedImage.dataBase64`.
    data_base64: String,
    /// On-disk size in bytes. Reported back so the caller can gate
    /// oversized drops without reading twice.
    size_bytes: u64,
}

/// Upper bound for the frontend drop → base64 pipeline. Larger than
/// the 5 MB image cap because users reasonably want to drop short
/// audio clips / screen recordings. The Rust validation is the last
/// line of defence; the TS side enforces its own limit first so the
/// UX error is immediate.
const DROP_MAX_BYTES: u64 = 50 * 1024 * 1024;

/// Read an arbitrary absolute path and return its bytes base64-encoded
/// alongside a best-effort media type. Backs the chat input's
/// drag-and-drop flow for media (image / audio / video) files — the
/// frontend gets enough to mint an `AttachedImage` chip without its
/// own disk access.
///
/// Intentionally unsandboxed — drops from Finder / Explorer / any
/// other app commonly come from outside the current project root.
/// This is a local-only desktop app; the user explicitly dragged the
/// file in, so we trust the path. Non-existent / unreadable paths
/// surface as an error string the caller can toast.
#[tauri::command]
async fn read_file_as_base64(path: String) -> Result<DroppedFilePayload, String> {
    use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};

    tauri::async_runtime::spawn_blocking(move || {
        let abs = Path::new(&path);
        let meta = std::fs::metadata(abs).map_err(|e| format!("metadata: {e}"))?;
        if !meta.is_file() {
            return Err("not a regular file".to_string());
        }
        let size = meta.len();
        if size > DROP_MAX_BYTES {
            return Err(format!(
                "file exceeds {} byte limit ({} bytes)",
                DROP_MAX_BYTES, size
            ));
        }
        let bytes = std::fs::read(abs).map_err(|e| format!("read: {e}"))?;
        let name = abs
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("file")
            .to_string();
        let media_type = media_type_for_extension(abs).unwrap_or_default();
        Ok(DroppedFilePayload {
            name,
            media_type,
            data_base64: BASE64_STANDARD.encode(&bytes),
            size_bytes: size,
        })
    })
    .await
    .map_err(|e| format!("join error: {e}"))?
}

/// Best-effort MIME inference from a file extension. Only covers
/// media types the app actually pipes through to the provider —
/// anything else returns `None` so the caller can route that path
/// through the `@file` mention flow instead.
fn media_type_for_extension(path: &Path) -> Option<String> {
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .map(|s| s.to_ascii_lowercase())?;
    let mt = match ext.as_str() {
        // Images — mirror ATTACHMENT_ALLOWED_MEDIA_TYPES.
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        // Audio.
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "ogg" | "oga" => "audio/ogg",
        "m4a" => "audio/mp4",
        "flac" => "audio/flac",
        "aac" => "audio/aac",
        "opus" => "audio/opus",
        "webm" if is_probably_audio(path) => "audio/webm",
        // Video.
        "mp4" | "m4v" => "video/mp4",
        "mov" => "video/quicktime",
        "webm" => "video/webm",
        "mkv" => "video/x-matroska",
        "avi" => "video/x-msvideo",
        _ => return None,
    };
    Some(mt.to_string())
}

/// Placeholder hook for future magic-byte sniffing when we want to
/// disambiguate `.webm` audio from `.webm` video. Currently we just
/// treat `.webm` as video since that's the overwhelmingly common
/// case; keeping the hook here so the matcher above reads cleanly
/// and a follow-up can plug in real sniffing without churn.
fn is_probably_audio(_path: &Path) -> bool {
    false
}

/// Resolve the git repository root for `path` by running
/// `git rev-parse --show-toplevel`. Returns `None` if `path` is not
/// inside a git repo. Used by the frontend to normalise the project
/// path before running worktree / branch commands — critical when the
/// project directory is a git submodule (`.git` is a file, not a
/// directory) or a linked worktree, where the raw file-picker path
/// may differ from what git considers the repo root.
#[tauri::command]
async fn resolve_git_root(path: String) -> Option<String> {
    tauri::async_runtime::spawn_blocking(move || resolve_git_root_sync(&path))
        .await
        .ok()
        .flatten()
}

// `resolve_git_root_sync`, `resolve_worktree_path`, `list_git_worktrees_sync`,
// `create_git_worktree_internal`, and the `GitWorktree` struct now live in
// `flowstate-app-layer::git_worktree`. The Tauri command wrappers below
// import and call them via the `use` at the top of this file; only the
// async `#[tauri::command]` wrappers remain here because they're what
// Tauri's `generate_handler!` macro registers.

/// Return the current git branch for `path`, or `None` if `path` is not
/// inside a git repo (or git itself fails). Used by the chat header to
/// surface the active branch under the thread title.
///
/// The subprocess wait is dispatched through
/// `tauri::async_runtime::spawn_blocking` so a slow repo can never hold
/// up the IPC handler — the rule is "git never blocks UI", and making
/// that explicit in the source protects it against future runtime
/// changes.
#[tauri::command]
async fn get_git_branch(path: String) -> Option<String> {
    tauri::async_runtime::spawn_blocking(move || get_git_branch_sync(path))
        .await
        .ok()
        .flatten()
}

/// `Command::new("git")` with `CREATE_NO_WINDOW` already applied so
/// the synchronous git probes scattered through the Tauri command
/// surface (branch detection, worktree listing, diff, etc.) don't
/// flash a cmd window on Windows GUI launches. No-op on non-Windows.
/// Tiny helper because every git spawn in this file would otherwise
/// need the same two-line dance.
///
/// Resolves `git` through the workspace binary resolver so the
/// user's `binaries.search_paths` setting (and platform fallbacks
/// like `/opt/homebrew/bin`) take effect — critical on Windows,
/// where the OS launcher would otherwise only consult the GUI
/// process's stripped PATH and miss explicit overrides. Also sets
/// the child's PATH to the augmented value so anything `git` itself
/// forks (ssh, hooks, the configured editor, ...) inherits the
/// extras.
fn git_cmd() -> Command {
    let mut c = Command::new(zenui_provider_api::resolve_cli_command("git"));
    zenui_provider_api::hide_console_window_std(&mut c);
    c.env("PATH", zenui_provider_api::path_with_extras(&[]));
    c
}

fn get_git_branch_sync(path: String) -> Option<String> {
    let output = git_cmd()
        .args(["-C", &path, "rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let branch = String::from_utf8(output.stdout).ok()?.trim().to_string();
    if branch.is_empty() {
        None
    } else {
        Some(branch)
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GitBranchList {
    current: Option<String>,
    local: Vec<String>,
    remote: Vec<String>,
}

/// List every local and remote-tracking ref in `path`, ordered by
/// most recent committer date. One `for-each-ref` call gives us the
/// `*` marker for the current branch, the short name, and the full
/// refname in a single pass — NUL-delimited so whitespace in refs
/// can't corrupt parsing. Skips `origin/HEAD` symbolic refs.
///
/// Async wrapper pushes the subprocess wait onto `spawn_blocking`, so
/// the branch-switcher popover opening never blocks other IPC while
/// `for-each-ref` runs on a slow repo.
#[tauri::command]
async fn list_git_branches(path: String) -> Result<GitBranchList, String> {
    tauri::async_runtime::spawn_blocking(move || list_git_branches_sync(path))
        .await
        .map_err(|e| format!("spawn_blocking join: {e}"))?
}

fn list_git_branches_sync(path: String) -> Result<GitBranchList, String> {
    let output = git_cmd()
        .args([
            "-C",
            &path,
            "for-each-ref",
            "--format=%(HEAD)%00%(refname:short)%00%(refname)",
            "--sort=-committerdate",
            "refs/heads",
            "refs/remotes",
        ])
        .output()
        .map_err(|e| format!("failed to run git: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if stderr.is_empty() {
            format!(
                "git for-each-ref failed (status {:?})",
                output.status.code()
            )
        } else {
            stderr
        });
    }
    let stdout =
        String::from_utf8(output.stdout).map_err(|e| format!("git output not utf-8: {e}"))?;

    let mut current: Option<String> = None;
    let mut local: Vec<String> = Vec::new();
    let mut remote: Vec<String> = Vec::new();
    for line in stdout.split('\n') {
        if line.is_empty() {
            continue;
        }
        let mut parts = line.splitn(3, '\0');
        let head_marker = parts.next().unwrap_or("");
        let short = parts.next().unwrap_or("");
        let full = parts.next().unwrap_or("");
        if short.is_empty() || full.is_empty() {
            continue;
        }
        // Skip symbolic refs like `origin/HEAD` — they alias a real
        // branch and showing both is noise.
        if short.ends_with("/HEAD") {
            continue;
        }
        if head_marker.trim() == "*" {
            current = Some(short.to_string());
        }
        if full.starts_with("refs/heads/") {
            local.push(short.to_string());
        } else if full.starts_with("refs/remotes/") {
            remote.push(short.to_string());
        }
    }

    Ok(GitBranchList {
        current,
        local,
        remote,
    })
}

// `GitWorktree` struct lives in `flowstate-app-layer::git_worktree`.
// Re-exported via the `use` at the top of this file.

/// List every worktree attached to the repo containing `path`,
/// parsed from `git worktree list --porcelain`. Async wrapper that
/// runs the blocking `list_git_worktrees_sync` from the app-layer on
/// a blocking thread so the Tauri IPC handler never stalls on a slow
/// git subprocess.
#[tauri::command]
async fn list_git_worktrees(path: String) -> Result<Vec<GitWorktree>, String> {
    tauri::async_runtime::spawn_blocking(move || list_git_worktrees_sync(path))
        .await
        .map_err(|e| format!("spawn_blocking join: {e}"))?
}

// `list_git_worktrees_sync` + `create_git_worktree_internal` +
// `resolve_git_root_sync` + `resolve_worktree_path` + `GitWorktree`
// all moved into `flowstate-app-layer::git_worktree` during Phase 3.
// The Tauri `#[tauri::command]` wrappers import them via the `use`
// at the top of this file.

/// Create a brand-new local branch based on the current HEAD and
/// switch to it. Separate from `git_checkout` because the call shape
/// is different (plain `checkout -b <name>`, no tracking ref) and
/// because the UI surfaces it as a distinct action — typing a branch
/// name that doesn't match any existing ref in the branch picker.
#[tauri::command]
fn git_create_branch(path: String, branch: String) -> Result<(), String> {
    if branch.trim().is_empty() {
        return Err("empty branch name".into());
    }
    let output = git_cmd()
        .args(["-C", &path, "checkout", "-b", &branch])
        .output()
        .map_err(|e| format!("failed to run git: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        return Err(if !stderr.is_empty() {
            stderr
        } else if !stdout.is_empty() {
            stdout
        } else {
            format!(
                "git checkout -b exited with status {:?}",
                output.status.code()
            )
        });
    }
    Ok(())
}

/// Force-delete a local branch via `git branch -D <branch>`.
/// Cannot delete the currently checked-out branch — git itself rejects
/// that with a clear error message which we forward verbatim.
#[tauri::command]
fn git_delete_branch(path: String, branch: String) -> Result<(), String> {
    if branch.trim().is_empty() {
        return Err("empty branch name".into());
    }
    let output = git_cmd()
        .args(["-C", &path, "branch", "-D", &branch])
        .output()
        .map_err(|e| format!("failed to run git: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        return Err(if !stderr.is_empty() {
            stderr
        } else if !stdout.is_empty() {
            stdout
        } else {
            format!(
                "git branch -D exited with status {:?}",
                output.status.code()
            )
        });
    }
    Ok(())
}

/// Create a new git worktree for `project_path` at `worktree_path`.
///
/// When `checkout_existing` is `None` or `Some(false)` (the default),
/// runs `git -C <project_path> worktree add -b <branch> <worktree_path>
/// <base_ref>` to create a brand-new branch forked from `base_ref`.
///
/// When `checkout_existing` is `Some(true)`, runs
/// `git -C <project_path> worktree add <worktree_path> <branch>` to
/// check out an already-existing local or remote-tracking branch into
/// the new worktree without creating a new branch.
///
/// Returns the freshly-listed `GitWorktree` entry for the new path so
/// the frontend can hydrate caches without a second round-trip.
/// Stderr is forwarded verbatim on failure so errors like
/// "pathspec is already checked out" or "a branch named X already
/// exists" surface to the user.
#[tauri::command]
fn create_git_worktree(
    project_path: String,
    worktree_path: String,
    branch: String,
    base_ref: String,
    checkout_existing: Option<bool>,
) -> Result<GitWorktree, String> {
    create_git_worktree_internal(
        &project_path,
        &worktree_path,
        &branch,
        &base_ref,
        checkout_existing.unwrap_or(false),
    )
}

// `create_git_worktree_internal` body moved into
// `flowstate-app-layer::git_worktree`; the Tauri command above
// delegates to it via the `use` at the top of this file.

/// Remove the worktree rooted at `worktree_path`. When `force` is
/// false, git refuses if the worktree has uncommitted changes or
/// locked state — the frontend surfaces stderr inline and can offer
/// a retry with `force = true`, which runs `git worktree remove -f`.
/// Does NOT soft-delete the SDK project linked to this worktree; the
/// caller is responsible for cleaning up any flowstate-side metadata
/// so history stays visible until the user explicitly deletes the
/// threads.
#[tauri::command]
fn remove_git_worktree(
    project_path: String,
    worktree_path: String,
    force: bool,
) -> Result<(), String> {
    if worktree_path.trim().is_empty() {
        return Err("empty worktree path".into());
    }
    let mut cmd = Command::new(zenui_provider_api::resolve_cli_command("git"));
    zenui_provider_api::hide_console_window_std(&mut cmd);
    cmd.env("PATH", zenui_provider_api::path_with_extras(&[]));
    cmd.args(["-C", &project_path, "worktree", "remove"]);
    if force {
        cmd.arg("--force");
    }
    cmd.arg(&worktree_path);
    let output = cmd
        .output()
        .map_err(|e| format!("failed to run git: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        return Err(if !stderr.is_empty() {
            stderr
        } else if !stdout.is_empty() {
            stdout
        } else {
            format!(
                "git worktree remove exited with status {:?}",
                output.status.code()
            )
        });
    }
    Ok(())
}

/// Switch the working tree in `path` to `branch`. When `create_track`
/// is `Some(remote_ref)`, we run `checkout -b <branch> --track
/// <remote_ref>` to create a new local branch tracking a remote; when
/// it's `None`, a plain `checkout <branch>`. On failure, git's stderr
/// is returned verbatim so the UI can show the user exactly why (dirty
/// tree, merge conflict, nonexistent branch, etc.) rather than a
/// generic "checkout failed" message.
#[tauri::command]
fn git_checkout(path: String, branch: String, create_track: Option<String>) -> Result<(), String> {
    if branch.trim().is_empty() {
        return Err("empty branch name".into());
    }
    let mut cmd = Command::new(zenui_provider_api::resolve_cli_command("git"));
    zenui_provider_api::hide_console_window_std(&mut cmd);
    cmd.env("PATH", zenui_provider_api::path_with_extras(&[]));
    cmd.args(["-C", &path, "checkout"]);
    match &create_track {
        Some(remote_ref) => {
            cmd.args(["-b", &branch, "--track", remote_ref]);
        }
        None => {
            cmd.arg(&branch);
        }
    }
    let output = cmd
        .output()
        .map_err(|e| format!("failed to run git: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        return Err(if !stderr.is_empty() {
            stderr
        } else if !stdout.is_empty() {
            stdout
        } else {
            format!("git checkout exited with status {:?}", output.status.code())
        });
    }
    Ok(())
}

/// Lightweight per-file entry returned by `get_git_diff_summary`.
/// Just path + line stats — no file contents. Designed so the diff
/// panel can show the full file list immediately without paying the
/// IPC + render cost of every file's before/after content. The
/// expensive content fetch happens lazily, one file at a time,
/// through `get_git_diff_file` when the user expands a row.
#[derive(Serialize, Clone)]
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
            return format!("<file too large to inline: {} bytes>", meta.len());
        }
    }
    std::fs::read_to_string(abs).unwrap_or_default()
}

fn git_show_head(repo: &str, file: &str) -> String {
    let Ok(output) = git_cmd()
        .args(["-C", repo, "show", &format!("HEAD:{file}")])
        .output()
    else {
        return String::new();
    };
    if !output.status.success() {
        return String::new();
    }
    if output.stdout.len() as u64 > GIT_DIFF_MAX_FILE_BYTES {
        return format!("<file too large to inline: {} bytes>", output.stdout.len());
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
///
/// Async wrapper: pushes all subprocess waits through
/// `spawn_blocking`, and inside the blocking task runs the two
/// independent git reads (tracked numstat + untracked ls-files)
/// concurrently via `std::thread::scope`. Both are read-only
/// queries that don't touch `.git/index.lock`, so they truly
/// overlap rather than serialise inside git.
#[tauri::command]
async fn get_git_diff_summary(path: String) -> Vec<GitFileSummary> {
    tauri::async_runtime::spawn_blocking(move || get_git_diff_summary_sync(path))
        .await
        .unwrap_or_default()
}

fn get_git_diff_summary_sync(path: String) -> Vec<GitFileSummary> {
    let project_path = Path::new(&path);
    if !project_path.is_dir() {
        return Vec::new();
    }

    let mut entries: Vec<GitFileSummary> = Vec::new();
    std::thread::scope(|s| {
        let tracked_h = s.spawn(|| run_git_diff_numstat(&path));
        let untracked_h = s.spawn(|| run_git_ls_files_others(project_path, &path));
        entries.extend(tracked_h.join().unwrap_or_default());
        entries.extend(untracked_h.join().unwrap_or_default());
    });
    entries
}

/// Tracked changes via `git diff HEAD --numstat -z`.
/// Format with `-z`:
///   For non-renames:  "<adds>\t<dels>\t<path>\0"
///   For renames:      "<adds>\t<dels>\t\0<old>\0<new>\0"
/// Binary files report `-` for both counts; we treat as 0/0.
fn run_git_diff_numstat(path: &str) -> Vec<GitFileSummary> {
    let mut entries: Vec<GitFileSummary> = Vec::new();
    let Ok(output) = git_cmd()
        .args(["-C", path, "diff", "HEAD", "--numstat", "-z"])
        .output()
    else {
        return entries;
    };
    if !output.status.success() {
        return entries;
    }
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
    entries
}

/// Untracked (new) files honoring .gitignore. `git diff HEAD`
/// doesn't see these so we list them separately and count the
/// lines ourselves.
fn run_git_ls_files_others(project_path: &Path, path: &str) -> Vec<GitFileSummary> {
    let mut entries: Vec<GitFileSummary> = Vec::new();
    let Ok(output) = git_cmd()
        .args([
            "-C",
            path,
            "ls-files",
            "--others",
            "--exclude-standard",
            "-z",
        ])
        .output()
    else {
        return entries;
    };
    if !output.status.success() {
        return entries;
    }
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
    entries
}

/// Lazy per-file content fetch. Called from the frontend the moment
/// the user expands a file row in the diff panel. The summary call
/// has already given us the path; this fills in before+after only
/// when needed, so we never ship the contents of files the user
/// doesn't actually look at.
///
/// Async wrapper: `git show HEAD:<file>` can take hundreds of ms on
/// a slow repo, so the subprocess wait lives on `spawn_blocking`.
#[tauri::command]
async fn get_git_diff_file(path: String, file: String) -> GitFileContents {
    tauri::async_runtime::spawn_blocking(move || get_git_diff_file_sync(path, file))
        .await
        .unwrap_or_else(|_| GitFileContents {
            before: String::new(),
            after: String::new(),
        })
}

fn get_git_diff_file_sync(path: String, file: String) -> GitFileContents {
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

// ─────────────────────────────────────────────────────────────────
// Streamed diff summary — Phase 1 (git status) + Phase 2 (numstat)
// ─────────────────────────────────────────────────────────────────
//
// The blocking `get_git_diff_summary` above ships the entire file
// list in one shot once `git diff HEAD --numstat` completes. On a
// monorepo with many changes that one subprocess can sit there for
// 10–60 seconds, during which the UI has literally nothing to
// render. `watch_git_diff_summary` fixes that by splitting the work:
//
//   1. `git status --porcelain=v1 -z --untracked-files=all` is near
//      instant even on huge repos — it gives us paths + statuses and
//      we emit a single `Files` event immediately.
//   2. `git diff HEAD --numstat -z` is spawned with a piped stdout
//      and parsed NUL-by-NUL. Each record becomes a `Numstat` event
//      the moment git produces it.
//
// The subprocess is killable via `stop_git_diff_summary(token)` so
// closing the panel or navigating away doesn't leak a git process.
// A 30 s wall-clock watchdog is the backstop in case the subprocess
// hangs for any other reason.

/// Events streamed from `watch_git_diff_summary` back to the
/// frontend over a Tauri `Channel<T>`. Tagged enum with snake_case
/// kind so the JS side can discriminate with a simple switch.
#[derive(Serialize, Clone)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum DiffSummaryEvent {
    /// Initial fast-path file list, emitted exactly once right after
    /// `git status` completes. Untracked files already carry an
    /// `additions` count (see `count_file_lines_bounded`); all other
    /// entries arrive with `additions = 0, deletions = 0` and get
    /// hydrated by the subsequent `Numstat` events.
    Files { files: Vec<GitFileSummary> },
    /// Streamed numstat record, one per tracked-change file, emitted
    /// in the order git produces them. The frontend upserts by path
    /// into its local map.
    Numstat {
        path: String,
        additions: u32,
        deletions: u32,
    },
    /// Terminal event. `ok = false` surfaces timeout / cancellation /
    /// subprocess errors; the frontend uses this to flip its status
    /// badge out of "streaming" mode.
    Done { ok: bool, error: Option<String> },
}

/// Handle tracked in `DiffTasks` so `stop_git_diff_summary(token)`
/// can kill an in-flight subscription from another thread. `child`
/// is taken (and killed) when we cancel; `cancelled` is checked
/// between read loops in the streaming thread for cooperative
/// shutdown in case the kill signal races the loop.
struct DiffTaskHandle {
    child: Arc<Mutex<Option<Child>>>,
    cancelled: Arc<AtomicBool>,
}

#[derive(Default)]
struct DiffTasks {
    tasks: Mutex<HashMap<u64, DiffTaskHandle>>,
}

/// Cap for per-file line counting during the fast path. Reading a
/// 50 MB untracked generated artifact just to show `+N` in the
/// header would defeat the whole point of Phase 1 being fast.
const UNTRACKED_COUNT_MAX_BYTES: u64 = 2 * 1024 * 1024;

fn count_file_lines_bounded(abs: &Path) -> u32 {
    let Ok(meta) = std::fs::metadata(abs) else {
        return 0;
    };
    if meta.len() > UNTRACKED_COUNT_MAX_BYTES {
        return 0;
    }
    let Ok(bytes) = std::fs::read(abs) else {
        return 0;
    };
    if bytes.is_empty() {
        return 0;
    }
    let nl = bytes.iter().filter(|&&b| b == b'\n').count() as u32;
    if bytes.last() == Some(&b'\n') {
        nl
    } else {
        nl + 1
    }
}

/// Phase 1: enumerate changed + untracked files via `git status`.
/// Status codes map to the same strings `run_git_diff_numstat`
/// produces so the frontend doesn't have to special-case the
/// streamed path. Rename entries consume TWO NUL-delimited chunks
/// in v1 porcelain: `<XY> <new>\0<old>\0` — we use the new path and
/// skip the old one.
fn collect_git_status_files(project_path: &Path, path: &str) -> Vec<GitFileSummary> {
    let Ok(output) = git_cmd()
        .args([
            "-C",
            path,
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
        ])
        .output()
    else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    let chunks: Vec<&[u8]> = output.stdout.split(|&b| b == 0).collect();
    let mut files: Vec<GitFileSummary> = Vec::new();
    let mut i = 0;
    while i < chunks.len() {
        let chunk = chunks[i];
        i += 1;
        if chunk.len() < 3 {
            continue;
        }
        let x = chunk[0];
        let y = chunk[1];
        let file_path = String::from_utf8_lossy(&chunk[3..]).into_owned();
        let is_rename = x == b'R' || x == b'C';
        if is_rename {
            // Skip the trailing old-path chunk that v1 porcelain
            // writes for renames/copies.
            i += 1;
        }
        if file_path.is_empty() {
            continue;
        }
        let (status, additions) = if x == b'?' && y == b'?' {
            // Untracked — git doesn't count lines for these, so we
            // do it ourselves so the badge has something to show.
            let abs = project_path.join(&file_path);
            ("added", count_file_lines_bounded(&abs))
        } else if is_rename {
            ("renamed", 0)
        } else if x == b'D' || y == b'D' {
            ("deleted", 0)
        } else if x == b'A' || y == b'A' {
            ("added", 0)
        } else {
            ("modified", 0)
        };
        files.push(GitFileSummary {
            path: file_path,
            status: status.to_string(),
            additions,
            deletions: 0,
        });
    }
    files
}

/// Run the whole streaming pipeline on a blocking worker thread.
/// Returns Ok on natural completion, Err("cancelled") on user
/// cancel, Err("timeout") on watchdog trip, or Err with any other
/// subprocess / io error message.
fn run_watch_diff(
    path: &str,
    on_event: &Channel<DiffSummaryEvent>,
    cancelled: Arc<AtomicBool>,
    child_slot: Arc<Mutex<Option<Child>>>,
) -> Result<(), String> {
    let project_path = Path::new(path);
    if !project_path.is_dir() {
        return Err("not a directory".into());
    }

    // Phase 1: status-based file list. Near-instant even on huge
    // repos because git only has to walk the index + worktree once.
    let files = collect_git_status_files(project_path, path);
    if cancelled.load(Ordering::SeqCst) {
        return Err("cancelled".into());
    }
    on_event
        .send(DiffSummaryEvent::Files { files })
        .map_err(|e| e.to_string())?;

    // Phase 2: stream numstat. Piped stdout + read_until(b'\0') so
    // each record surfaces as soon as git's buffer flushes, rather
    // than at the end of the whole run.
    let child = git_cmd()
        .args(["-C", path, "diff", "HEAD", "--numstat", "-z"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| e.to_string())?;
    let mut child = child;
    let stdout = child.stdout.take().ok_or_else(|| "no stdout".to_string())?;
    *child_slot.lock().unwrap() = Some(child);

    // 30s watchdog. Using an mpsc channel instead of sleeping the
    // full window lets us return the watchdog thread immediately on
    // success. On timeout it flips `cancelled` and kills the child,
    // which the read loop below notices on its next iteration.
    let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
    let watchdog = {
        let cancelled = cancelled.clone();
        let child_slot = child_slot.clone();
        std::thread::spawn(move || {
            if let Err(std::sync::mpsc::RecvTimeoutError::Timeout) =
                done_rx.recv_timeout(Duration::from_secs(30))
            {
                cancelled.store(true, Ordering::SeqCst);
                if let Some(mut c) = child_slot.lock().unwrap().take() {
                    let _ = c.kill();
                }
            }
        })
    };

    let mut reader = std::io::BufReader::new(stdout);
    let mut buf: Vec<u8> = Vec::with_capacity(256);
    let mut loop_err: Option<String> = None;
    loop {
        if cancelled.load(Ordering::SeqCst) {
            break;
        }
        buf.clear();
        let n = match reader.read_until(b'\0', &mut buf) {
            Ok(n) => n,
            Err(e) => {
                loop_err = Some(e.to_string());
                break;
            }
        };
        if n == 0 {
            break;
        }
        if buf.last() == Some(&b'\0') {
            buf.pop();
        }
        if buf.is_empty() {
            continue;
        }
        let record = String::from_utf8_lossy(&buf).into_owned();
        let mut parts = record.splitn(3, '\t');
        let Some(adds_s) = parts.next() else {
            continue;
        };
        let Some(dels_s) = parts.next() else {
            continue;
        };
        let path_field = parts.next().unwrap_or("");
        let adds = adds_s.parse::<u32>().unwrap_or(0);
        let dels = dels_s.parse::<u32>().unwrap_or(0);
        let file_path = if path_field.is_empty() {
            // Rename: next NUL-chunk is the old path, the one after
            // is the new path. We report the new path so it matches
            // what the Phase 1 status list emitted.
            let mut old_buf: Vec<u8> = Vec::new();
            if reader.read_until(b'\0', &mut old_buf).is_err() {
                break;
            }
            let mut new_buf: Vec<u8> = Vec::new();
            if reader.read_until(b'\0', &mut new_buf).is_err() {
                break;
            }
            if new_buf.last() == Some(&b'\0') {
                new_buf.pop();
            }
            String::from_utf8_lossy(&new_buf).into_owned()
        } else {
            path_field.to_string()
        };
        if file_path.is_empty() {
            continue;
        }
        let _ = on_event.send(DiffSummaryEvent::Numstat {
            path: file_path,
            additions: adds,
            deletions: dels,
        });
    }

    // Tear down: signal the watchdog so it returns instead of
    // sleeping the remainder of its 30s budget, join it, then reap
    // the child process if we still hold it.
    let _ = done_tx.send(());
    let _ = watchdog.join();
    let child_opt = child_slot.lock().unwrap().take();
    if let Some(mut c) = child_opt {
        let _ = c.wait();
    }

    if let Some(e) = loop_err {
        return Err(e);
    }
    if cancelled.load(Ordering::SeqCst) {
        return Err("cancelled".into());
    }
    Ok(())
}

/// Start a streaming diff subscription. Returns immediately — the
/// actual work runs on a blocking thread and streams events back
/// over `on_event`. The caller identifies the subscription by
/// `token` so it can cancel via `stop_git_diff_summary`.
#[tauri::command]
async fn watch_git_diff_summary(
    app: tauri::AppHandle,
    path: String,
    token: u64,
    on_event: Channel<DiffSummaryEvent>,
) {
    let cancelled = Arc::new(AtomicBool::new(false));
    let child_slot: Arc<Mutex<Option<Child>>> = Arc::new(Mutex::new(None));

    if let Some(tasks) = app.try_state::<DiffTasks>() {
        tasks.tasks.lock().unwrap().insert(
            token,
            DiffTaskHandle {
                child: child_slot.clone(),
                cancelled: cancelled.clone(),
            },
        );
    }

    let app_for_thread = app.clone();
    let cancelled_for_thread = cancelled.clone();
    let child_for_thread = child_slot.clone();
    // `spawn_blocking` returns a JoinHandle — explicitly drop it so
    // the task detaches. The command returns immediately and the
    // subscription runs until it completes or is cancelled; all
    // cleanup is self-contained inside the closure.
    drop(tauri::async_runtime::spawn_blocking(move || {
        let result = run_watch_diff(&path, &on_event, cancelled_for_thread, child_for_thread);
        if let Some(tasks) = app_for_thread.try_state::<DiffTasks>() {
            tasks.tasks.lock().unwrap().remove(&token);
        }
        let done_event = match result {
            Ok(()) => DiffSummaryEvent::Done {
                ok: true,
                error: None,
            },
            Err(e) => DiffSummaryEvent::Done {
                ok: false,
                error: Some(e),
            },
        };
        let _ = on_event.send(done_event);
    }));
}

/// Cancel an in-flight `watch_git_diff_summary` subscription. The
/// streaming thread notices the cancellation on its next read-loop
/// iteration; killing the child process here short-circuits any
/// slow read that was otherwise blocking that loop.
#[tauri::command]
fn stop_git_diff_summary(tasks: State<'_, DiffTasks>, token: u64) {
    let handle = tasks.tasks.lock().unwrap().remove(&token);
    if let Some(handle) = handle {
        handle.cancelled.store(true, Ordering::SeqCst);
        if let Some(mut child) = handle.child.lock().unwrap().take() {
            let _ = child.kill();
        }
    }
}

// ─────────────────────────────────────────────────────────────────
// /code editor view — file picker + single-file read
// ─────────────────────────────────────────────────────────────────

/// Maximum file size we'll inline into the code view. The editor
/// uses @pierre/diffs' Virtualizer so a 10k-line plain-text file is
/// fine, but anything past this is probably generated / binary /
/// not useful to read inline and we return a placeholder marker.
const CODE_VIEW_MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;

/// List the project's files via the per-worktree fff-search index
/// (see `file_index.rs`). Returns a `ProjectFileListing` snapshot —
/// `files` is whatever's been walked so far, `indexing` flips to
/// `false` when fff's background scanner finishes, and React Query
/// re-fetches on a short stale window so the picker fills in live.
///
/// We deliberately do NOT cap the list (no `PROJECT_FILE_LIST_MAX`
/// cut). The previous `ignore::WalkBuilder` impl quit at 20k files
/// across all parallel threads, which on a 100k-file repo silently
/// dropped ~80% of paths and made it impossible to find files even
/// by typing the exact name. fff's mmap-backed file table makes the
/// full list cheap to materialise.
#[tauri::command]
fn list_project_files(
    path: String,
    state: State<Arc<FileIndexRegistry>>,
) -> ProjectFileListing {
    file_index::list_project_files(state.inner(), &path)
}

/// Drop the cached `FilePicker` for `path` so the next
/// `list_project_files` call rebuilds it from a fresh scan. Wired up
/// from the `turn_completed` session event in
/// `useSessionStreamSubscription` — agent edits that touch many
/// files in quick succession can outrun fs-event coalescing on
/// macOS, so we trigger an explicit re-walk at the moment the user
/// is most likely to look at the picker again.
///
/// No-op when `path` was never indexed; the next list call indexes
/// it cold for free.
#[tauri::command]
fn reindex_project_files(
    path: String,
    state: State<Arc<FileIndexRegistry>>,
) -> Result<(), String> {
    state.inner().reindex(Path::new(&path))
}

/// Return the contents of a single project file as a UTF-8 string.
/// Used by the /code editor view when the user opens a file from
/// the picker. Caps the payload so opening a binary / generated
/// mega-file doesn't freeze the bridge.
#[tauri::command]
fn read_project_file(path: String, file: String) -> Result<String, String> {
    let project_path = Path::new(&path);
    let abs = project_path.join(&file);
    // Canonicalise both and make sure the requested file is
    // actually inside the project root. Without this, a crafted
    // `file = "../../etc/passwd"` could escape — not a big deal
    // for a local-only desktop app but cheap to defend against.
    let project_canon = project_path
        .canonicalize()
        .map_err(|e| format!("project path: {e}"))?;
    let abs_canon = abs.canonicalize().map_err(|e| format!("file path: {e}"))?;
    if !abs_canon.starts_with(&project_canon) {
        return Err("file is outside the project root".into());
    }
    let meta = std::fs::metadata(&abs_canon).map_err(|e| format!("metadata: {e}"))?;
    if meta.len() > CODE_VIEW_MAX_FILE_BYTES {
        return Err(format!(
            "file too large to inline: {} bytes (max {})",
            meta.len(),
            CODE_VIEW_MAX_FILE_BYTES
        ));
    }
    std::fs::read_to_string(&abs_canon).map_err(|e| format!("read: {e}"))
}

/// Write UTF-8 `contents` to a project file at `path / file`. Used
/// by the /code editor view's save flow (Cmd+S, Vim `:w`, and
/// auto-save on focus-out).
///
/// Path-escape defense: we canonicalise the **parent directory** —
/// not the target file — because `canonicalize` fails on paths that
/// don't exist yet, and a save can target a freshly-renamed file
/// or a brand-new file the user just typed into. Canonicalising the
/// parent then re-joining the basename is the standard workaround
/// and still rejects `..` traversal (the canonical parent must lie
/// inside the canonical project root).
///
/// No size cap on write — the frontend guards at 10 MiB before
/// invoking this. The 4 MiB read cap (`CODE_VIEW_MAX_FILE_BYTES`)
/// is a read-side concern and doesn't apply here.
#[tauri::command]
fn write_project_file(path: String, file: String, contents: String) -> Result<(), String> {
    let project_path = Path::new(&path);
    let abs = project_path.join(&file);
    let project_canon = project_path
        .canonicalize()
        .map_err(|e| format!("project path: {e}"))?;
    let parent = abs.parent().ok_or("no parent directory")?;
    let parent_canon = parent
        .canonicalize()
        .map_err(|e| format!("parent path: {e}"))?;
    let basename = abs.file_name().ok_or("no filename")?;
    let final_path = parent_canon.join(basename);
    if !final_path.starts_with(&project_canon) {
        return Err("file is outside the project root".into());
    }
    std::fs::write(&final_path, contents.as_bytes()).map_err(|e| format!("write: {e}"))
}

/// Generic binary-write sibling of `write_project_file`. Used by:
///   * the Excalidraw editor when saving `.excalidraw.png` (which is
///     a binary PNG with the scene encoded into a `tEXt` chunk), and
///   * any future flow that needs to round-trip raw bytes inside the
///     project root (e.g. dragged-in screenshots saved verbatim).
///
/// Same path-escape defense as `write_project_file`: canonicalise the
/// **parent** directory and re-join the basename, then check the
/// final path lies inside the canonical project root.
#[tauri::command]
fn write_project_file_bytes(
    path: String,
    file: String,
    bytes: Vec<u8>,
) -> Result<(), String> {
    let project_path = Path::new(&path);
    let abs = project_path.join(&file);
    let project_canon = project_path
        .canonicalize()
        .map_err(|e| format!("project path: {e}"))?;
    let parent = abs.parent().ok_or("no parent directory")?;
    // Create the parent on demand — pasted images land in `pasted/`
    // which usually doesn't exist yet on a fresh project.
    if !parent.exists() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir: {e}"))?;
    }
    let parent_canon = parent
        .canonicalize()
        .map_err(|e| format!("parent path: {e}"))?;
    let basename = abs.file_name().ok_or("no filename")?;
    let final_path = parent_canon.join(basename);
    if !final_path.starts_with(&project_canon) {
        return Err("file is outside the project root".into());
    }
    std::fs::write(&final_path, &bytes).map_err(|e| format!("write: {e}"))
}

/// Save a clipboard-pasted image into a `pasted/` subfolder of
/// `targetDir` (the directory of the open `.md`). Creates the subfolder
/// if missing and dedupes the filename when collisions occur.
///
/// Returns the **path the editor should embed in `![…](…)`** — that's
/// always relative to `targetDir`, e.g. `pasted/foo-123.png`.
///
/// Sandbox: refuses any `targetDir` whose canonical form does not lie
/// inside the canonical `projectPath`.
#[tauri::command]
fn markdown_save_pasted_image(
    project_path: String,
    target_dir: String,
    file_name: String,
    bytes: Vec<u8>,
) -> Result<String, String> {
    const PASTED_SUBDIR: &str = "pasted";
    let project = Path::new(&project_path)
        .canonicalize()
        .map_err(|e| format!("project path: {e}"))?;
    let dir = Path::new(&target_dir);
    if !dir.exists() {
        return Err(format!(
            "target directory does not exist: {}",
            dir.display()
        ));
    }
    let dir_canon = dir
        .canonicalize()
        .map_err(|e| format!("target dir: {e}"))?;
    if !dir_canon.starts_with(&project) {
        return Err("target directory is outside the project root".into());
    }
    let pasted_dir = dir_canon.join(PASTED_SUBDIR);
    std::fs::create_dir_all(&pasted_dir).map_err(|e| format!("mkdir pasted/: {e}"))?;
    let stem = Path::new(&file_name)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("image")
        .to_string();
    let ext = Path::new(&file_name)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("png")
        .to_string();
    let mut candidate = file_name.clone();
    let mut counter = 1u32;
    loop {
        let path = pasted_dir.join(&candidate);
        if !path.exists() {
            std::fs::write(&path, &bytes).map_err(|e| format!("write image: {e}"))?;
            return Ok(format!("{PASTED_SUBDIR}/{candidate}"));
        }
        candidate = format!("{stem}-{counter}.{ext}");
        counter += 1;
        if counter > 9999 {
            return Err("could not find a unique filename after 9999 attempts".into());
        }
    }
}

/// Rasterise `svg` to RGBA at `scale`× and put the resulting bitmap on
/// the OS clipboard via `tauri-plugin-clipboard-manager`.
///
/// Why Rust-side: WKWebView taints any `<canvas>` an SVG `<img>` is
/// drawn into (`getImageData` / `toBlob` throw `SecurityError`). resvg
/// has no such hang-up — it parses the SVG and renders to pixels via
/// tiny-skia, then we hand those bytes to the clipboard plugin.
#[tauri::command]
async fn markdown_copy_svg_as_png(
    app: tauri::AppHandle,
    svg: String,
    scale: f32,
) -> Result<(), String> {
    use tauri::image::Image as TauriImage;
    use tauri_plugin_clipboard_manager::ClipboardExt;
    let rgba = tokio::task::spawn_blocking(move || rasterise_svg(&svg, scale))
        .await
        .map_err(|e| format!("rasterise task panicked: {e}"))??;
    let image = TauriImage::new(&rgba.rgba, rgba.width, rgba.height);
    app.clipboard()
        .write_image(&image)
        .map_err(|e| format!("clipboard write_image: {e}"))?;
    Ok(())
}

struct RasterisedImage {
    rgba: Vec<u8>,
    width: u32,
    height: u32,
}

fn rasterise_svg(svg: &str, scale: f32) -> Result<RasterisedImage, String> {
    let mut opts = resvg::usvg::Options::default();
    let mut fontdb = resvg::usvg::fontdb::Database::new();
    fontdb.load_system_fonts();
    opts.fontdb = std::sync::Arc::new(fontdb);

    let tree = resvg::usvg::Tree::from_str(svg, &opts)
        .map_err(|e| format!("parse svg: {e}"))?;
    let size = tree.size();
    let scale = scale.max(0.1);
    let width = ((size.width() as f32) * scale).ceil().max(1.0) as u32;
    let height = ((size.height() as f32) * scale).ceil().max(1.0) as u32;

    let mut pixmap = resvg::tiny_skia::Pixmap::new(width, height)
        .ok_or_else(|| format!("tiny_skia pixmap {width}x{height} alloc failed"))?;
    pixmap.fill(resvg::tiny_skia::Color::WHITE);
    let transform = resvg::tiny_skia::Transform::from_scale(scale, scale);
    resvg::render(&tree, transform, &mut pixmap.as_mut());
    Ok(RasterisedImage {
        rgba: pixmap.take(),
        width,
        height,
    })
}

/// A single entry returned by `list_directory`. `is_ignored` is true
/// when the entry would be excluded by `.gitignore` / `.git/info/exclude`
/// / the global gitignore — the frontend still receives the entry, but
/// renders it dimmed so the user can drill into `node_modules/`,
/// `dist/`, `.env`, etc. on demand without polluting search or the
/// mention picker.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DirEntry {
    /// Basename (no slashes).
    name: String,
    /// True for directories; false for regular files. Symlinks are
    /// skipped entirely (we don't follow them here to avoid loops).
    is_dir: bool,
    /// True if the entry is covered by a gitignore rule.
    is_ignored: bool,
}

/// List the immediate children (1 level only) of `sub_path` inside
/// the project at `path`. Unlike `list_project_files`, this INCLUDES
/// gitignored entries, flagging each with `is_ignored`. Backs the
/// /code view's file tree, which lazy-expands one directory at a
/// time so `node_modules/` and friends never get eagerly walked.
///
/// `sub_path` is forward-slash, project-relative; empty string means
/// the project root. The callback sandboxes every request against
/// the canonicalised root so a crafted `../../etc` escape can't work.
#[tauri::command]
fn list_directory(path: String, sub_path: String) -> Result<Vec<DirEntry>, String> {
    use std::collections::HashSet;

    let project_path = Path::new(&path);
    if !project_path.is_dir() {
        return Err(format!("not a directory: {path}"));
    }
    let project_canon = project_path
        .canonicalize()
        .map_err(|e| format!("project path: {e}"))?;

    // Resolve the sub_path relative to the project root, then sandbox
    // it. Empty sub_path means "list the project root itself".
    let target = if sub_path.is_empty() {
        project_canon.clone()
    } else {
        project_canon.join(&sub_path)
    };
    let target_canon = target
        .canonicalize()
        .map_err(|e| format!("sub path: {e}"))?;
    if !target_canon.starts_with(&project_canon) {
        return Err("sub path is outside the project root".into());
    }
    if !target_canon.is_dir() {
        return Err(format!("not a directory: {sub_path}"));
    }

    // Pass 1: walk with gitignore rules ON and depth 1 to capture the
    // "visible" subset. `max_depth(Some(1))` yields the target dir at
    // depth 0 plus every immediate child at depth 1, so we filter to
    // depth == 1 to drop the starting entry itself.
    let mut visible: HashSet<String> = HashSet::new();
    let walker_visible = ignore::WalkBuilder::new(&target_canon)
        .hidden(false)
        .ignore(false)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .parents(true)
        .require_git(false)
        .max_depth(Some(1))
        .follow_links(false)
        .build();
    for result in walker_visible {
        let Ok(entry) = result else { continue };
        if entry.depth() != 1 {
            continue;
        }
        if let Some(name) = entry.file_name().to_str() {
            visible.insert(name.to_string());
        }
    }

    // Pass 2: raw `fs::read_dir` — gives every on-disk entry at this
    // level, ignored or not. Set-difference against `visible` tells
    // us which ones the gitignore rules would have hidden. We skip
    // symlinks and the `.git` dir itself (always noise in the tree).
    let mut entries: Vec<DirEntry> = Vec::new();
    let iter = std::fs::read_dir(&target_canon).map_err(|e| format!("read_dir: {e}"))?;
    for entry in iter {
        let Ok(entry) = entry else { continue };
        let file_type = match entry.file_type() {
            Ok(ft) => ft,
            Err(_) => continue,
        };
        if file_type.is_symlink() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if name == ".git" {
            continue;
        }
        let is_dir = file_type.is_dir();
        let is_ignored = !visible.contains(&name);
        entries.push(DirEntry {
            name,
            is_dir,
            is_ignored,
        });
    }

    // Folders first, then alphabetically — matches VS Code / Finder.
    entries.sort_by(|a, b| {
        if a.is_dir != b.is_dir {
            if a.is_dir {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Greater
            }
        } else {
            a.name.to_lowercase().cmp(&b.name.to_lowercase())
        }
    });
    Ok(entries)
}

// ────────────────────────────────────────────────────────────────────────
// /code file-tree mutations: create / rename / move / trash
// ────────────────────────────────────────────────────────────────────────
//
// Every command below operates on **forward-slash, project-relative**
// paths so the frontend never has to think about absolute paths. The
// sandbox pattern matches `read_project_file` / `write_project_file`:
//   1. Canonicalise the project root.
//   2. Canonicalise the parent directory of the target (canonicalising
//      the target itself fails when it doesn't exist yet — fine for
//      reads, but creates and renames need a forward-looking path).
//   3. Re-join the basename onto the canonical parent.
//   4. Verify the final path is still inside the canonical root.
//
// The frontend mirrors the legality checks (`canDropInto`, name-has-
// no-slash, no-op rename) for prompt UI feedback, but the backend is
// the source of truth — every check above is enforced here too.

/// Validate that `name` is a usable basename: non-empty after trim,
/// no path separators, no `..`, no NUL.
fn validate_basename(name: &str) -> Result<String, String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err("name cannot be empty".into());
    }
    if trimmed.contains('/') || trimmed.contains('\\') {
        return Err("name must not contain path separators".into());
    }
    if trimmed == "." || trimmed == ".." {
        return Err("name cannot be `.` or `..`".into());
    }
    if trimmed.contains('\0') {
        return Err("name contains a NUL byte".into());
    }
    Ok(trimmed.to_string())
}

/// Resolve a project-relative `sub_path` against `path` and verify the
/// result lives inside the canonical project root. Returns the
/// canonicalised absolute path. Used for inputs that must already
/// exist (rename/move/trash sources).
fn resolve_existing_subpath(path: &str, sub_path: &str) -> Result<PathBuf, String> {
    let project_path = Path::new(path);
    if !project_path.is_dir() {
        return Err(format!("not a directory: {path}"));
    }
    let project_canon = project_path
        .canonicalize()
        .map_err(|e| format!("project path: {e}"))?;
    let target = project_canon.join(sub_path);
    let target_canon = target
        .canonicalize()
        .map_err(|e| format!("sub path: {e}"))?;
    if !target_canon.starts_with(&project_canon) {
        return Err("sub path is outside the project root".into());
    }
    Ok(target_canon)
}

/// Resolve a project-relative parent directory + basename into a
/// forward-looking absolute path (the basename does not need to
/// exist yet). Verifies the canonical parent is inside the canonical
/// project root. Used for creates and the destination side of moves
/// and renames.
fn resolve_new_path_in_dir(
    path: &str,
    parent_sub: &str,
    basename: &str,
) -> Result<(PathBuf, PathBuf), String> {
    let project_path = Path::new(path);
    if !project_path.is_dir() {
        return Err(format!("not a directory: {path}"));
    }
    let project_canon = project_path
        .canonicalize()
        .map_err(|e| format!("project path: {e}"))?;
    let parent_abs = if parent_sub.is_empty() {
        project_canon.clone()
    } else {
        project_canon.join(parent_sub)
    };
    let parent_canon = parent_abs
        .canonicalize()
        .map_err(|e| format!("parent path: {e}"))?;
    if !parent_canon.starts_with(&project_canon) {
        return Err("parent path is outside the project root".into());
    }
    if !parent_canon.is_dir() {
        return Err("parent path is not a directory".into());
    }
    let final_path = parent_canon.join(basename);
    if !final_path.starts_with(&project_canon) {
        return Err("target path is outside the project root".into());
    }
    Ok((project_canon, final_path))
}

/// Convert an absolute path back into the forward-slash project-
/// relative sub-path the frontend expects. The caller has already
/// verified `abs` lies under `project_canon`, so `strip_prefix`
/// can't fail in practice — but we still surface a readable error
/// rather than panic if it ever does.
fn to_sub_path(project_canon: &Path, abs: &Path) -> Result<String, String> {
    let rel = abs
        .strip_prefix(project_canon)
        .map_err(|_| "path outside project root".to_string())?;
    let mut s = String::new();
    for (i, comp) in rel.components().enumerate() {
        if i > 0 {
            s.push('/');
        }
        s.push_str(&comp.as_os_str().to_string_lossy());
    }
    Ok(s)
}

/// Create an empty file at `<path>/<sub_path>/<name>`. Errors if a
/// sibling with the same name already exists (no silent overwrite).
/// Returns the new file's project-relative sub-path.
#[tauri::command]
fn create_project_file(
    path: String,
    sub_path: String,
    name: String,
) -> Result<String, String> {
    let basename = validate_basename(&name)?;
    let (project_canon, final_path) =
        resolve_new_path_in_dir(&path, &sub_path, &basename)?;
    if final_path.exists() {
        return Err(format!("`{basename}` already exists"));
    }
    std::fs::write(&final_path, b"").map_err(|e| format!("create file: {e}"))?;
    to_sub_path(&project_canon, &final_path)
}

/// Create an empty directory at `<path>/<sub_path>/<name>`. Errors
/// if a sibling with the same name already exists. Returns the new
/// directory's project-relative sub-path.
#[tauri::command]
fn create_project_dir(
    path: String,
    sub_path: String,
    name: String,
) -> Result<String, String> {
    let basename = validate_basename(&name)?;
    let (project_canon, final_path) =
        resolve_new_path_in_dir(&path, &sub_path, &basename)?;
    if final_path.exists() {
        return Err(format!("`{basename}` already exists"));
    }
    std::fs::create_dir(&final_path).map_err(|e| format!("create dir: {e}"))?;
    to_sub_path(&project_canon, &final_path)
}

/// Rename a file or directory in place. `new_name` is the **basename
/// only** — callers pass `Untitled.txt` or `daily-notes`, not a full
/// path. For files, preserves the original extension if the new name
/// has none (so renaming `foo.md` → `bar` produces `bar.md`).
/// Returns the new project-relative sub-path.
#[tauri::command]
fn rename_project_path(
    path: String,
    sub_path: String,
    new_name: String,
) -> Result<String, String> {
    let new_basename = validate_basename(&new_name)?;
    let project_path = Path::new(&path);
    let project_canon = project_path
        .canonicalize()
        .map_err(|e| format!("project path: {e}"))?;
    let src = resolve_existing_subpath(&path, &sub_path)?;
    let parent = src
        .parent()
        .ok_or_else(|| "source has no parent".to_string())?;
    if !parent.starts_with(&project_canon) {
        return Err("cannot rename the project root".into());
    }

    // Preserve extension on files when the new name has none.
    let final_name = if src.is_file() && Path::new(&new_basename).extension().is_none() {
        match src.extension().and_then(|e| e.to_str()) {
            Some(ext) => format!("{new_basename}.{ext}"),
            None => new_basename.clone(),
        }
    } else {
        new_basename.clone()
    };

    let new_path = parent.join(&final_name);
    if new_path == src {
        // No-op rename.
        return to_sub_path(&project_canon, &src);
    }
    if new_path.exists() {
        return Err(format!("`{final_name}` already exists in this folder"));
    }
    if !new_path.starts_with(&project_canon) {
        return Err("target path is outside the project root".into());
    }
    std::fs::rename(&src, &new_path).map_err(|e| format!("rename: {e}"))?;
    to_sub_path(&project_canon, &new_path)
}

/// Move a file or directory into a different parent directory,
/// keeping its basename. `target_sub` is the project-relative path
/// of the destination folder (empty string = project root).
///
/// Validates:
///   - source exists and target is a directory inside the project
///   - target is not the source itself, nor any descendant of it
///   - destination doesn't already exist (no silent overwrite)
#[tauri::command]
fn move_project_path(
    path: String,
    source_sub: String,
    target_sub: String,
) -> Result<String, String> {
    let project_path = Path::new(&path);
    let project_canon = project_path
        .canonicalize()
        .map_err(|e| format!("project path: {e}"))?;
    let src = resolve_existing_subpath(&path, &source_sub)?;
    let target_dir = if target_sub.is_empty() {
        project_canon.clone()
    } else {
        let abs = project_canon.join(&target_sub);
        let canon = abs
            .canonicalize()
            .map_err(|e| format!("target path: {e}"))?;
        if !canon.starts_with(&project_canon) {
            return Err("target path is outside the project root".into());
        }
        if !canon.is_dir() {
            return Err("target path is not a directory".into());
        }
        canon
    };

    if target_dir == src {
        return Err("cannot move a folder into itself".into());
    }
    if target_dir.starts_with(&src) {
        return Err("cannot move a folder into one of its descendants".into());
    }

    let basename = src
        .file_name()
        .ok_or_else(|| "source has no basename".to_string())?
        .to_owned();
    let new_path = target_dir.join(&basename);

    // Same-parent move is a no-op.
    if let Some(parent) = src.parent() {
        if parent == target_dir {
            return to_sub_path(&project_canon, &src);
        }
    }
    if new_path.exists() {
        return Err(format!(
            "`{}` already exists in the destination",
            basename.to_string_lossy()
        ));
    }
    if !new_path.starts_with(&project_canon) {
        return Err("destination is outside the project root".into());
    }
    std::fs::rename(&src, &new_path).map_err(|e| format!("move: {e}"))?;
    to_sub_path(&project_canon, &new_path)
}

/// Move a file or directory to the OS trash. Recoverable via
/// Finder/Explorer — the same semantics as Cmd+Delete in a file
/// manager. `trash::delete` is synchronous, so we wrap it in
/// `spawn_blocking` to keep the runtime responsive for big folders.
#[tauri::command]
async fn trash_project_path(path: String, sub_path: String) -> Result<(), String> {
    let abs = resolve_existing_subpath(&path, &sub_path)?;
    tokio::task::spawn_blocking(move || trash::delete(&abs))
        .await
        .map_err(|e| format!("join trash worker: {e}"))?
        .map_err(|e| format!("move to trash: {e}"))
}

/// Launch an external code editor on `path` (the project root) by
/// running `<editor> .` with the child's cwd set to `path`. All the
/// standard editor launchers — `zed`, `code`, `cursor`, `idea`,
/// `subl` — treat `.` relative to cwd as "open this directory as a
/// project", and some of them behave better with `.` than with an
/// absolute path positional.
///
/// We spawn and detach a reaper thread that calls `wait()` on the
/// child so it doesn't sit around as a `<defunct>` zombie after it
/// exits. We intentionally don't kill the editor when flowstate quits.
///
/// Returns `Err` with a readable message when:
///   * `path` isn't a directory (no project to open)
///   * the editor binary isn't on `$PATH` (e.g. user picked
///     "Zed" but never installed Zed's `zed` CLI helper)
#[tauri::command]
fn open_in_editor(editor: String, path: String) -> Result<(), String> {
    let trimmed = editor.trim();
    if trimmed.is_empty() {
        return Err("no editor configured".into());
    }
    let project_path = Path::new(&path);
    if !project_path.is_dir() {
        return Err(format!("not a directory: {path}"));
    }
    // Resolve the editor name to an absolute path through the
    // workspace binary resolver — picks up the user's configured
    // extras and platform fallbacks. Especially important here
    // because users routinely point this at editor shims like
    // `code`, `cursor`, `zed` that live in non-system bin dirs.
    let mut editor_cmd = Command::new(zenui_provider_api::resolve_cli_command(trimmed));
    zenui_provider_api::hide_console_window_std(&mut editor_cmd);
    editor_cmd.env("PATH", zenui_provider_api::path_with_extras(&[]));
    let mut child = editor_cmd
        .arg(".")
        .current_dir(project_path)
        .spawn()
        .map_err(|e| format!("failed to launch `{trimmed}`: {e}"))?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

/// Live content search across the project, backed by fff-search's
/// mmap-mounted file index (see `file_index::search_file_contents`).
/// Supports literal / regex / fuzzy modes via `ContentSearchOptions`,
/// with include/exclude globs filtering the indexed file list before
/// the grep pass.
///
/// `cancel_token` registers a cooperative cancellation flag so the
/// frontend can interrupt a slow search by calling
/// `stop_content_search`. Pass `None` for fire-and-forget calls (the
/// chat composer's @-mention preview, for example).
///
/// Returns one `ContentBlock` per disjoint match group per file:
/// each block is the match line(s) plus
/// `CONTENT_SEARCH_CONTEXT_LINES` lines of context on either side.
/// The frontend renders these as Zed-style multibuffer chunks.
#[tauri::command]
fn search_file_contents(
    path: String,
    query: String,
    options: ContentSearchOptions,
    cancel_token: Option<u64>,
    registry: State<Arc<FileIndexRegistry>>,
    tasks: State<Arc<SearchTasks>>,
) -> Result<Vec<file_index::ContentBlock>, String> {
    // Register / look up the cancellation flag *before* doing any
    // work so a `stop_content_search(token)` racing the start of
    // this call still cancels it.
    let flag = cancel_token.map(|t| tasks.inner().register(t));
    let result = file_index::search_file_contents(
        registry.inner(),
        &path,
        &query,
        &options,
        flag.as_deref(),
    );
    if let Some(t) = cancel_token {
        tasks.inner().unregister(t);
    }
    result
}

/// Cancel the in-flight content search registered under `token`.
/// Idempotent — unknown tokens are silently ignored.
#[tauri::command]
fn stop_content_search(token: u64, tasks: State<Arc<SearchTasks>>) {
    tasks.inner().cancel(token);
}

struct AppLifecycle {
    lifecycle: Arc<DaemonLifecycle>,
}

/// Tauri-managed state wrapping the cross-platform sleep-prevention
/// controller. macOS implements it via `caffeinate`; Windows via
/// `SetThreadExecutionState`. Linux and other platforms have no
/// backing OS hook today, so the state — and the commands that read
/// it — aren't installed there. The frontend probes for command
/// availability at startup (`useCaffeinateSupport()`), so an absent
/// command simply hides the toggle in Settings.
#[cfg(any(target_os = "macos", windows))]
struct CaffeinateState(Arc<flowstate_app_layer::caffeinate::CaffeinateController>);

/// Payload sent from the daemon spawn task to the
/// `flowstate-daemon-ready` thread once bootstrap completes. The thread
/// owns the `AppHandle` clone needed for `manage(...)`, but the daemon
/// task is where the relevant Arcs are constructed — so we hand them
/// across the channel together. macOS + Windows add a second field
/// for the sleep-prevention controller; other platforms compile
/// without it.
struct DaemonReadyPayload {
    lifecycle: Arc<DaemonLifecycle>,
    #[cfg(any(target_os = "macos", windows))]
    caffeinate: Arc<flowstate_app_layer::caffeinate::CaffeinateController>,
}

/// Initialize tracing. Debug builds stream to stderr so `cargo tauri dev`
/// surfaces logs in the terminal; release builds keep writing to a log
/// file alongside the daemon log.
fn init_tracing() {
    // `fff_search::file_picker=off` suppresses a known false-positive
    // ERROR in fff-search 0.5.2: its filesystem walker filters out
    // binary files (icons, build artifacts) from the in-memory index,
    // but a parallel `git status --include-untracked` thread returns
    // every tracked path regardless. When the status applier can't
    // find the filtered-out entry it logs ERROR per file, spamming
    // hundreds of lines on every refresh in a repo with many icon
    // assets. Demote to `off` until either an .fffignore mechanism
    // ships upstream or we patch the crate. See `file_index.rs`.
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new("flowstate=info,zenui=info,fff_search::file_picker=off,warn")
    });

    if cfg!(debug_assertions) {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(env_filter)
            .with_writer(std::io::stderr)
            .try_init();
        eprintln!("flowstate: dev build, logging to stderr");
        return;
    }

    // Platform-conventional log dir — `~/Library/Logs/Flowstate` on
    // macOS, XDG state on Linux, %LOCALAPPDATA% on Windows. Earlier
    // builds wrote to `$TMPDIR/flowstate/logs`, which is sandboxed
    // per-user and pruned by macOS, and the splash screen still
    // tells users to look in `~/Library/Logs/Flowstate`. Honouring
    // that path here keeps the splash text honest and gives users a
    // stable place to find logs across reboots.
    let log_dir = default_log_dir();
    let _ = std::fs::create_dir_all(&log_dir);
    let log_path = log_dir.join("flowstate.log");

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
        eprintln!("flowstate: logging to {}", log_path.display());
    } else {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(env_filter)
            .try_init();
    }
}

#[tauri::command]
fn pty_open(
    manager: State<'_, PtyManager>,
    cols: u16,
    rows: u16,
    cwd: Option<String>,
    shell: Option<String>,
    on_event: Channel<PtyEvent>,
) -> Result<PtyId, String> {
    manager.open(cols, rows, cwd, shell, on_event)
}

#[tauri::command]
fn pty_write(manager: State<'_, PtyManager>, id: PtyId, data: Vec<u8>) -> Result<(), String> {
    manager.write(id, &data)
}

#[tauri::command]
fn pty_resize(
    manager: State<'_, PtyManager>,
    id: PtyId,
    cols: u16,
    rows: u16,
) -> Result<(), String> {
    manager.resize(id, cols, rows)
}

#[tauri::command]
fn pty_pause(manager: State<'_, PtyManager>, id: PtyId) -> Result<(), String> {
    manager.pause(id)
}

#[tauri::command]
fn pty_resume(manager: State<'_, PtyManager>, id: PtyId) -> Result<(), String> {
    manager.resume(id)
}

#[tauri::command]
fn pty_kill(manager: State<'_, PtyManager>, id: PtyId) -> Result<(), String> {
    manager.kill(id)
}

// ─────────────────────────────────────────────────────────────────
// Thread popout — spawn a second webview window pinned to a single
// session, with an optional "always on top" flag
// ─────────────────────────────────────────────────────────────────
//
// The popout window reuses the main app bundle: the frontend reads
// `?popout=1` from the URL to render a stripped shell (no sidebar /
// terminal dock) around the same `<ChatView sessionId=…>` the main
// window uses. Because `connect` subscribes to a broadcast channel
// (see `transport-tauri::commands::connect`), every popout gets its
// own independent subscription and stays in sync with the main
// window with zero extra plumbing.
//
// Labels are deterministic (`thread-<session_id>`) so a second click
// on the same thread's "Pop out" button just re-focuses the existing
// window instead of stacking duplicates. The capability file
// (capabilities/default.json) widens its `windows` list with a
// `thread-*` glob so the new window inherits the same IPC surface
// the main window has.

/// Build the deterministic window label for a session's popout.
/// Extracted so the frontend (via `getCurrentWindow().label`) and
/// the Rust side never disagree on the format.
fn popout_window_label(session_id: &str) -> String {
    format!("thread-{session_id}")
}

#[tauri::command]
async fn popout_thread(
    app: tauri::AppHandle,
    session_id: String,
    always_on_top: bool,
) -> Result<(), String> {
    let label = popout_window_label(&session_id);

    // Already open? Flip the pin to match the caller's current
    // preference (the user may have toggled it in the main header
    // since the window was last focused), unminimize if needed,
    // and bring it forward. Errors from set_always_on_top are
    // swallowed — the window still exists and focusing it is the
    // more important half of the contract.
    if let Some(existing) = app.get_webview_window(&label) {
        let _ = existing.set_always_on_top(always_on_top);
        let _ = existing.unminimize();
        let _ = existing.show();
        return existing.set_focus().map_err(|e| e.to_string());
    }

    // `WebviewUrl::App` is resolved by Tauri relative to the
    // frontend's base URL (the Vite dev server in `tauri dev`,
    // the bundled `index.html` in release). The `?popout=1`
    // query string is what the frontend keys off to render the
    // stripped shell — see `isPopoutWindow` in `src/lib/popout.ts`.
    let url = format!("/chat/{session_id}?popout=1");
    let builder = tauri::WebviewWindowBuilder::new(&app, &label, tauri::WebviewUrl::App(url.into()))
        .title("flowstate — thread")
        .inner_size(480.0, 720.0)
        .min_inner_size(360.0, 480.0)
        .always_on_top(always_on_top);

    // Overlay traffic-light buttons over our custom in-app header so
    // popouts match the main window's frameless look. macOS-only — the
    // `title_bar_style` / `hidden_title` builder methods don't exist on
    // other platforms in Tauri 2.
    #[cfg(target_os = "macos")]
    let builder = builder
        .title_bar_style(tauri::TitleBarStyle::Overlay)
        .hidden_title(true);

    builder
        .build()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn set_window_always_on_top(
    app: tauri::AppHandle,
    label: String,
    enabled: bool,
) -> Result<(), String> {
    let win = app
        .get_webview_window(&label)
        .ok_or_else(|| format!("no window with label {label}"))?;
    win.set_always_on_top(enabled).map_err(|e| e.to_string())
}

// ─────────────────────────────────────────────────────────────────
// user_config — flowstate-app-owned key/value store
// ─────────────────────────────────────────────────────────────────
//
// Backed by `~/.flowstate/user_config.sqlite` (its own file, not the
// daemon's database). Used for app-level UI tunables like the
// highlighter pool size. Frontend wraps these as
// `getUserConfig` / `setUserConfig` in `src/lib/api.ts`.

// Phase 5 — every `#[tauri::command]` in this block flips to the
// `DaemonClient` proxy: Tauri command body → reqwest POST →
// loopback HTTP handler (served by the `flowstate_app_layer::http`
// router) → `UserConfigStore` / `UsageStore`. The Tauri state no
// longer carries the stores directly for these commands; Phase 6
// will move them into a separate daemon process and the command
// bodies won't need any further change, only the base URL they
// already read from the `DaemonBaseUrl` channel.

#[tauri::command]
async fn get_user_config(
    base_url: State<'_, DaemonBaseUrl>,
    key: String,
) -> Result<Option<String>, String> {
    base_url.client().get_user_config(key).await
}

#[tauri::command]
async fn set_user_config(
    base_url: State<'_, DaemonBaseUrl>,
    key: String,
    value: String,
) -> Result<(), String> {
    base_url.client().set_user_config(key, value).await
}

// ─────────────────────────────────────────────────────────────────
// user MCP — global MCP-server list at ~/.flowstate/mcp.json
// ─────────────────────────────────────────────────────────────────
//
// File is canonical: every provider adapter loads it on session
// spawn, the UI reads/writes through these two commands. No SQLite
// mirror — would create a second source of truth. New sessions pick
// up changes immediately; running sessions keep their config.

/// Read `~/.flowstate/mcp.json` and return the parsed
/// [`zenui_provider_api::McpConfigFile`] envelope. Missing or
/// invalid file → empty `{mcpServers: {}}` (matches what each
/// adapter sees on a malformed config). The reserved `flowstate`
/// key is stripped at load time and never returned to the UI.
#[tauri::command]
async fn get_user_mcp_servers(
    registry: State<'_, zenui_provider_api::UserMcpRegistry>,
) -> Result<zenui_provider_api::McpConfigFile, String> {
    let snapshot = registry.load();
    Ok(zenui_provider_api::McpConfigFile {
        mcp_servers: snapshot.servers,
    })
}

/// Atomically replace the contents of `~/.flowstate/mcp.json` with
/// the supplied server map. Each entry is validated with
/// [`zenui_provider_api::validate_mcp_server_config`] before any
/// write happens — invalid input rejects the whole update so the
/// file never lands in a broken state. The reserved `"flowstate"`
/// key is silently dropped if present (orchestration owns it).
///
/// Returns the written contents on success so the UI can re-render
/// from the canonical source.
#[tauri::command]
async fn set_user_mcp_servers(
    registry: State<'_, zenui_provider_api::UserMcpRegistry>,
    servers: std::collections::BTreeMap<String, zenui_provider_api::McpServerConfig>,
) -> Result<zenui_provider_api::McpConfigFile, String> {
    let mut clean = std::collections::BTreeMap::new();
    for (name, cfg) in servers {
        if name == zenui_provider_api::RESERVED_FLOWSTATE_KEY {
            continue;
        }
        zenui_provider_api::validate_mcp_server_config(&name, &cfg)?;
        clean.insert(name, cfg);
    }
    let cfg = zenui_provider_api::McpConfigFile {
        mcp_servers: clean,
    };
    zenui_provider_api::write_mcp_config_file(registry.path(), &cfg)
        .map_err(|e| format!("write mcp.json: {e}"))?;
    Ok(cfg)
}

// ─────────────────────────────────────────────────────────────────
// Sleep prevention — keeps the display awake while turns are running
// ─────────────────────────────────────────────────────────────────
//
// Thin wrappers over the `CaffeinateController` Tauri-managed state.
// All coordination (single-instance, respawn semantics, force-kill)
// lives in the controller; these commands just surface user-driven
// actions and a status snapshot. macOS implements the underlying
// hook with `caffeinate`; Windows with `SetThreadExecutionState`.
// Other platforms don't register the state or the commands; the
// settings UI hides the toggle when the command isn't available.

#[cfg(any(target_os = "macos", windows))]
#[tauri::command]
fn caffeinate_status(
    state: State<'_, CaffeinateState>,
) -> flowstate_app_layer::caffeinate::CaffeinateStatus {
    state.0.status()
}

/// Settings UI calls this after writing the `system.caffeinate` key
/// so the controller acts on the change immediately (release the
/// hook if just disabled; arm it if just enabled and turns are in
/// flight). Without this hop the controller would still pick up the
/// change at the next turn boundary, but the UI feels laggy.
#[cfg(any(target_os = "macos", windows))]
#[tauri::command]
fn caffeinate_refresh(state: State<'_, CaffeinateState>) {
    state.0.refresh();
}

/// User clicked "Force kill" in settings. Releases the active hook
/// immediately. Setting stays enabled — caffeinate will re-arm
/// naturally on the next 0→1 turn transition (after current in-flight
/// turns finish).
#[cfg(any(target_os = "macos", windows))]
#[tauri::command]
fn caffeinate_kill(state: State<'_, CaffeinateState>) {
    state.0.force_kill();
}

// ─────────────────────────────────────────────────────────────────
// User-configured binary search paths
// ─────────────────────────────────────────────────────────────────
//
// `binaries.search_paths` is a JSON-encoded array of directories
// the user has added as the explicit "look here too" escape hatch
// for `find_cli_binary`. Used when their `claude` / `codex` /
// `copilot` is installed somewhere our PATH walk + curated
// fallbacks don't cover — particularly common on Windows GUI
// launches where the inherited PATH is much narrower than the
// user's shell.

/// Read `binaries.search_paths` from the store, parse the JSON, and
/// push the resulting directories into the process-wide resolver.
/// Called once at daemon startup and again from `refresh_binary_
/// search_paths` after the settings UI writes a new value. Quiet on
/// failure — a malformed entry shouldn't crash the daemon, just log
/// and leave the previous list in place.
fn apply_binary_search_paths_from_config(store: &UserConfigStore) {
    const KEY: &str = "binaries.search_paths";
    let raw = match store.get(KEY) {
        Ok(Some(s)) => s,
        Ok(None) => {
            // No user override yet — clear so a previously-applied
            // value from this session doesn't linger after the user
            // emptied the setting.
            zenui_provider_api::set_extra_search_paths(Vec::new());
            return;
        }
        Err(err) => {
            tracing::warn!(%err, "failed to read binaries.search_paths from user config; leaving previous list applied");
            return;
        }
    };
    let parsed: Vec<String> = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(err) => {
            tracing::warn!(
                %err,
                value = %raw,
                "binaries.search_paths is not a JSON array of strings; ignoring"
            );
            return;
        }
    };
    let paths: Vec<std::path::PathBuf> = parsed
        .into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .map(std::path::PathBuf::from)
        .collect();
    tracing::debug!(count = paths.len(), "applying user-configured binary search paths");
    zenui_provider_api::set_extra_search_paths(paths);
}

/// Settings UI calls this after writing the `binaries.search_paths`
/// key so the in-process resolver picks up the change immediately —
/// otherwise providers would only see the new list after a daemon
/// restart. Idempotent.
#[tauri::command]
fn refresh_binary_search_paths(state: State<'_, UserConfigStore>) {
    apply_binary_search_paths_from_config(&state);
}

/// Snapshot the current resolver state — primarily for the settings
/// UI to render a "currently active" badge alongside the editable
/// list, so users can verify the daemon is seeing what they
/// configured. Returned as plain strings for trivial JSON
/// serialization across the Tauri boundary.
#[tauri::command]
fn list_binary_search_paths() -> Vec<String> {
    zenui_provider_api::extra_search_paths()
        .into_iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect()
}

// Per-session and per-project display metadata: titles, names,
// previews, ordering. Lives in the same `user_config.sqlite`
// file as the kv table above, in dedicated tables. The agent
// SDK no longer persists any of this — its persistence layer
// only stores fields the runtime needs to execute or resume
// agents. See `rs-agent-sdk/crates/core/persistence/CLAUDE.md`
// for the boundary.

#[tauri::command]
async fn set_session_display(
    base_url: State<'_, DaemonBaseUrl>,
    session_id: String,
    display: SessionDisplay,
) -> Result<(), String> {
    base_url
        .client()
        .set_session_display(session_id, display)
        .await
}

#[tauri::command]
async fn get_session_display(
    base_url: State<'_, DaemonBaseUrl>,
    session_id: String,
) -> Result<Option<SessionDisplay>, String> {
    base_url.client().get_session_display(session_id).await
}

#[tauri::command]
async fn list_session_display(
    base_url: State<'_, DaemonBaseUrl>,
) -> Result<HashMap<String, SessionDisplay>, String> {
    base_url.client().list_session_display().await
}

#[tauri::command]
async fn delete_session_display(
    base_url: State<'_, DaemonBaseUrl>,
    session_id: String,
) -> Result<(), String> {
    base_url.client().delete_session_display(session_id).await
}

#[tauri::command]
async fn set_project_display(
    base_url: State<'_, DaemonBaseUrl>,
    project_id: String,
    display: ProjectDisplay,
) -> Result<(), String> {
    base_url
        .client()
        .set_project_display(project_id, display)
        .await
}

#[tauri::command]
async fn get_project_display(
    base_url: State<'_, DaemonBaseUrl>,
    project_id: String,
) -> Result<Option<ProjectDisplay>, String> {
    base_url.client().get_project_display(project_id).await
}

#[tauri::command]
async fn list_project_display(
    base_url: State<'_, DaemonBaseUrl>,
) -> Result<HashMap<String, ProjectDisplay>, String> {
    base_url.client().list_project_display().await
}

#[tauri::command]
async fn delete_project_display(
    base_url: State<'_, DaemonBaseUrl>,
    project_id: String,
) -> Result<(), String> {
    base_url.client().delete_project_display(project_id).await
}

// Parent/child worktree links — a flowstate-app concept, not an SDK
// concept. Each worktree has its own SDK project (so the SDK's
// existing cwd resolution picks up the worktree folder), and this
// table just records "project X is a worktree of project Y, on
// branch Z". The sidebar uses these links to group worktree threads
// under the parent project visually.

#[tauri::command]
async fn set_project_worktree(
    base_url: State<'_, DaemonBaseUrl>,
    project_id: String,
    parent_project_id: String,
    branch: Option<String>,
) -> Result<(), String> {
    base_url
        .client()
        .set_project_worktree(project_id, parent_project_id, branch)
        .await
}

#[tauri::command]
async fn get_project_worktree(
    base_url: State<'_, DaemonBaseUrl>,
    project_id: String,
) -> Result<Option<ProjectWorktree>, String> {
    base_url.client().get_project_worktree(project_id).await
}

#[tauri::command]
async fn list_project_worktree(
    base_url: State<'_, DaemonBaseUrl>,
) -> Result<HashMap<String, ProjectWorktree>, String> {
    base_url.client().list_project_worktree().await
}

#[tauri::command]
async fn delete_project_worktree(
    base_url: State<'_, DaemonBaseUrl>,
    project_id: String,
) -> Result<(), String> {
    base_url.client().delete_project_worktree(project_id).await
}

// ─────────────────────────────────────────────────────────────────
// usage — flowstate-app-owned analytics store
// ─────────────────────────────────────────────────────────────────
//
// The Usage dashboard in the frontend reads per-turn aggregates
// (cost, tokens, duration) sliced by time / provider / model /
// session. Backed by `~/.flowstate/usage.sqlite` — its own file,
// never shared with the SDK's database. The subscriber task in
// `setup` writes rows into it on every `RuntimeEvent::TurnCompleted`.

#[tauri::command]
async fn get_usage_summary(
    base_url: State<'_, DaemonBaseUrl>,
    range: UsageRange,
    group_by: Option<UsageGroupBy>,
) -> Result<UsageSummaryPayload, String> {
    base_url.client().get_usage_summary(range, group_by).await
}

#[tauri::command]
async fn get_usage_timeseries(
    base_url: State<'_, DaemonBaseUrl>,
    range: UsageRange,
    bucket: UsageBucket,
    split_by: Option<UsageGroupBy>,
) -> Result<UsageTimeseriesPayload, String> {
    base_url
        .client()
        .get_usage_timeseries(range, bucket, split_by)
        .await
}

#[tauri::command]
async fn get_top_sessions(
    base_url: State<'_, DaemonBaseUrl>,
    range: UsageRange,
    limit: Option<u32>,
) -> Result<Vec<TopSessionRow>, String> {
    base_url.client().get_top_sessions(range, limit).await
}

/// Per-agent dashboard breakdown. See the app-layer `UsageStore`
/// method for the SQL shape.
#[tauri::command]
async fn get_usage_by_agent(
    base_url: State<'_, DaemonBaseUrl>,
    range: UsageRange,
) -> Result<UsageAgentPayload, String> {
    base_url.client().get_usage_by_agent(range).await
}

/// Two-row rollup of `usage_event_agents`. See the app-layer method
/// for the SQL shape.
#[tauri::command]
async fn get_usage_by_agent_role(
    base_url: State<'_, DaemonBaseUrl>,
    range: UsageRange,
) -> Result<UsageAgentPayload, String> {
    base_url.client().get_usage_by_agent_role(range).await
}

/// Last-seen snapshot of every rate-limit bucket the providers have
/// reported. The Anthropic 5-hour and weekly limits only land as a
/// side-effect of inference responses — there is no API to poll for
/// them — so we persist the latest value to `usage.sqlite` and seed
/// `state.rateLimits` from this on app boot. The frontend dispatches
/// one `seed_rate_limits` action with the returned vec; live
/// `RuntimeEvent::RateLimitUpdated` events overwrite individual
/// buckets via the existing `rate_limit_updated` reducer arm.
#[tauri::command]
async fn get_rate_limit_cache(
    base_url: State<'_, DaemonBaseUrl>,
) -> Result<Vec<zenui_provider_api::RateLimitInfo>, String> {
    base_url.client().get_rate_limit_cache().await
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

/// Path to the directory tracing writes `flowstate.log` into. On
/// macOS this is `~/Library/Logs/Flowstate`; on Linux it follows
/// XDG_STATE_HOME (or XDG_DATA_HOME); on Windows it lives under
/// %LOCALAPPDATA%. Used by Settings → Diagnostics for the "Logs dir"
/// row + Reveal-in-Finder button.
#[tauri::command]
fn get_log_dir() -> Result<String, String> {
    Ok(default_log_dir().to_string_lossy().to_string())
}

/// Path to the cache directory holding the embedded Node.js runtime
/// and the provider-SDK `node_modules/` trees (~350 MB after first
/// launch). Surfaced in Settings so users can find / wipe the cache
/// when troubleshooting a botched first install.
#[tauri::command]
fn get_cache_dir() -> Result<String, String> {
    Ok(runtime_cache_dir()?.to_string_lossy().to_string())
}

/// Delete the entire runtime cache directory (`~/Library/Caches/zenui`
/// on macOS — embedded Node, both SDK bridges, and the npm cache).
/// Used by Settings → Diagnostics → Clear cache when a botched
/// first install left the cache in a bad state.
///
/// Important caveat: the in-process `OnceLock`s in `embedded-node`
/// and the bridge runtimes still hold paths into the now-deleted
/// directory, so a relaunch is required before any provider-SDK
/// session will work again. The frontend surfaces this in the
/// "restart required" toast.
///
/// Returns Ok with the byte count freed (best-effort) so the toast
/// can show how much was reclaimed; falls through to Ok(0) if the
/// directory was already gone.
#[tauri::command]
fn clear_runtime_cache() -> Result<u64, String> {
    let dir = runtime_cache_dir()?;
    if !dir.exists() {
        return Ok(0);
    }
    let freed = dir_size_best_effort(&dir);
    std::fs::remove_dir_all(&dir)
        .map_err(|e| format!("remove cache dir {}: {e}", dir.display()))?;
    tracing::info!(
        bytes = freed,
        path = %dir.display(),
        "runtime cache cleared on user request"
    );
    Ok(freed)
}

/// Shared resolver for both `get_cache_dir` and `clear_runtime_cache`
/// so the path-shown and the path-deleted can never disagree.
fn runtime_cache_dir() -> Result<std::path::PathBuf, String> {
    Ok(dirs::cache_dir()
        .ok_or_else(|| "could not resolve per-user cache dir".to_string())?
        .join("zenui"))
}

/// Recursively sum file sizes under `dir`. Best-effort — IO errors
/// (permission denied on a stray file, broken symlink, etc.) are
/// swallowed so a partial measurement never blocks the user-facing
/// "Clear cache" action.
fn dir_size_best_effort(dir: &std::path::Path) -> u64 {
    fn walk(p: &std::path::Path, acc: &mut u64) {
        let Ok(entries) = std::fs::read_dir(p) else {
            return;
        };
        for entry in entries.flatten() {
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_dir() {
                walk(&entry.path(), acc);
            } else {
                *acc = acc.saturating_add(meta.len());
            }
        }
    }
    let mut total = 0u64;
    walk(dir, &mut total);
    total
}

/// Snapshot of provisioning failures held by the Tauri shell. Each
/// entry is one phase that failed during `provision_runtimes` (or a
/// subsequent retry). The Settings page consumes this on mount via
/// `get_provision_failures` and listens to live `provision` events
/// thereafter.
#[derive(Default)]
struct ProvisionState {
    inner: std::sync::Mutex<Vec<flowstate_app_layer::provision::ProvisionFailure>>,
}

impl ProvisionState {
    fn snapshot(&self) -> Vec<flowstate_app_layer::provision::ProvisionFailure> {
        self.inner.lock().map(|g| g.clone()).unwrap_or_default()
    }
    fn set(&self, v: Vec<flowstate_app_layer::provision::ProvisionFailure>) {
        if let Ok(mut g) = self.inner.lock() {
            *g = v;
        }
    }
    fn remove(&self, phase: flowstate_app_layer::provision::ProvisionPhase) {
        if let Ok(mut g) = self.inner.lock() {
            g.retain(|f| f.phase != phase);
        }
    }
    fn upsert(&self, failure: flowstate_app_layer::provision::ProvisionFailure) {
        if let Ok(mut g) = self.inner.lock() {
            g.retain(|f| f.phase != failure.phase);
            g.push(failure);
        }
    }
}

/// Read the current set of provisioning failures the Tauri shell is
/// holding. Returned as plain serializable structs so the React store
/// can seed `provisionFailures` on mount (covers the case where the
/// frontend mounts after the splash already dismissed and the live
/// `provision` events were missed).
#[tauri::command]
fn get_provision_failures(
    state: State<'_, ProvisionState>,
) -> Vec<flowstate_app_layer::provision::ProvisionFailure> {
    state.snapshot()
}

/// Re-run a single provisioning phase on user request (Settings page
/// "Retry" button). Resolves with `Ok(())` on success and updates
/// `ProvisionState` accordingly; resolves with `Err(string)` on
/// failure (the entry stays in the state). Live progress is also
/// emitted via the `provision` event so any open UI updates inline.
#[tauri::command]
async fn retry_provision_phase(
    app: tauri::AppHandle,
    state: State<'_, ProvisionState>,
    phase: String,
) -> Result<(), String> {
    let phase = flowstate_app_layer::provision::ProvisionPhase::from_str(&phase)
        .ok_or_else(|| format!("unknown provisioning phase: {phase}"))?;

    let reporter_handle = app.clone();
    let result = tokio::task::spawn_blocking(move || {
        let reporter: Box<flowstate_app_layer::provision::ProvisionReporter> =
            Box::new(move |event| {
                if let Err(e) = reporter_handle.emit("provision", &event) {
                    tracing::debug!(%e, "emit provision retry event failed");
                }
            });
        flowstate_app_layer::provision::retry_phase(phase, &reporter)
    })
    .await
    .map_err(|e| format!("retry task panicked: {e}"))?;

    match result {
        Ok(()) => {
            state.remove(phase);
            Ok(())
        }
        Err(e) => {
            let error = format!("{e:?}");
            state.upsert(flowstate_app_layer::provision::ProvisionFailure {
                phase,
                error: error.clone(),
            });
            Err(error)
        }
    }
}

/// Platform-appropriate default location for Flowstate's release-build
/// log file. Centralised so `init_tracing` and the
/// `get_log_dir` Tauri command never disagree.
fn default_log_dir() -> std::path::PathBuf {
    #[cfg(target_os = "macos")]
    {
        dirs::home_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join("Library/Logs/Flowstate")
    }
    #[cfg(target_os = "linux")]
    {
        dirs::state_dir()
            .or_else(dirs::data_local_dir)
            .unwrap_or_else(std::env::temp_dir)
            .join("flowstate/logs")
    }
    #[cfg(target_os = "windows")]
    {
        dirs::data_local_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join("Flowstate/logs")
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    init_tracing();

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
        // Global hotkey toggle (Cmd+Option+Shift+O). Registration of
        // the accelerator itself happens in the `.setup()` block below
        // once we have an `AppHandle` to clone into the callback.
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        // Clipboard image / text writes — used by the markdown editor
        // mermaid widget's "Copy as PNG" action, which rasterises an
        // SVG via `resvg` on the Rust side and pushes the bitmap
        // straight onto the OS clipboard.  Webview-side `<canvas>`
        // taints when an SVG is drawn into it on WKWebView, so going
        // through the native plugin is the only reliable path.
        .plugin(tauri_plugin_clipboard_manager::init())
        // ⌘Q on macOS routes through our custom "Close Window" menu
        // item (id `FLOWSTATE_CLOSE_WINDOW_MENU_ID`) installed in the
        // setup block below. Translate that menu click into
        // `window.close()` on whichever window currently has focus —
        // same path the red traffic light already takes, so the
        // existing `WindowEvent::CloseRequested` arm decides between
        // hiding the main window and destroying a popout based on
        // `is_main`. Falls back to the main window if no focused
        // window can be found (defensive — shouldn't happen for a
        // menu-accelerator press, but cheap insurance). Registered
        // unconditionally because non-macOS platforms install no
        // menu at all, so the id never matches there.
        .on_menu_event(|app, event| {
            #[cfg(target_os = "macos")]
            if event.id() == FLOWSTATE_CLOSE_WINDOW_MENU_ID {
                let focused = app.webview_windows().into_iter().find_map(|(_, w)| {
                    matches!(w.is_focused(), Ok(true)).then_some(w)
                });
                let target = focused
                    .or_else(|| app.get_webview_window(MAIN_WINDOW_LABEL));
                if let Some(window) = target {
                    if let Err(e) = window.close() {
                        tracing::warn!(
                            %e,
                            "⌘Q close-window: window.close() failed"
                        );
                    }
                }
            }
            // Suppress unused-variable warnings on non-macOS where
            // the cfg-gated body is empty.
            #[cfg(not(target_os = "macos"))]
            {
                let _ = (app, event);
            }
        })
        .manage(PtyManager::new())
        .manage(DiffTasks::default())
        // Per-worktree fff-search file pickers. Lazily populated on
        // first `list_project_files` call per worktree; the
        // `reindex_project_files` command drops entries (next list
        // re-walks). Wrapped in `Arc` so cancellation tokens and
        // command handlers can clone cheaply.
        .manage(Arc::new(FileIndexRegistry::default()))
        // Cancellation registry for `search_file_contents` — mirrors
        // `DiffTasks` above. `stop_content_search(token)` flips the
        // registered `AtomicBool` and the in-flight grep bails on
        // its next cooperative check.
        .manage(Arc::new(SearchTasks::default()))
        // Holds the snapshot of provisioning failures seen during
        // `provision_runtimes()` (and any subsequent retries from the
        // Settings page). Frontend pulls via `get_provision_failures`
        // on mount and keeps in sync via `provision` events.
        .manage(ProvisionState::default())
        .setup(|app| {
            let app_handle = app.handle().clone();

            // Install a custom app menu on macOS.
            //
            // We replicate the structure of `tauri::menu::Menu::default()`
            // (App / Edit / View / Window submenus so all the standard
            // input-editing accelerators — Cmd+C/V/X/A, Undo/Redo, etc. —
            // keep working in the WKWebView) with two surgical
            // departures:
            //
            //   1. The App submenu's "Quit" item is replaced with a
            //      custom "Close Window" item bound to ⌘Q. muda's
            //      PredefinedMenuItem::quit hard-wires ⌘Q to NSApp
            //      `terminate:`, which goes straight to
            //      `RunEvent::ExitRequested` and tears down the daemon.
            //      The user wants ⌘Q to mirror the red traffic light
            //      (hide the main window, destroy popouts) — the
            //      `on_menu_event` handler on the Builder catches our
            //      custom id and calls `window.close()` on the focused
            //      window, which fires `WindowEvent::CloseRequested`
            //      and goes through the existing per-label close path.
            //
            //   2. The Window submenu omits PredefinedMenuItem::close_window
            //      entirely. That item's accelerator is hard-wired to
            //      ⌘W in muda; leaving it in place would let macOS fire
            //      `CloseRequested` (= hide main window) on every ⌘W
            //      press, racing the JS-level handlers in CodeView
            //      (close active tab) and AppShell (no-op everywhere
            //      else). With no menu binding, ⌘W belongs to the JS
            //      layer entirely.
            //
            // Non-macOS platforms keep no explicit menu (Linux/Windows
            // Tauri apps have no menubar by default and the
            // close-button already terminates the process — see the
            // `WindowEvent::CloseRequested` arm below).
            #[cfg(target_os = "macos")]
            {
                use tauri::menu::{
                    AboutMetadata, Menu, MenuItemBuilder, PredefinedMenuItem, Submenu,
                };

                let handle = app.handle().clone();
                let pkg_info = handle.package_info();
                let config = handle.config();
                let about_metadata = AboutMetadata {
                    name: Some(pkg_info.name.clone()),
                    version: Some(pkg_info.version.to_string()),
                    copyright: config.bundle.copyright.clone(),
                    authors: config.bundle.publisher.clone().map(|p| vec![p]),
                    ..Default::default()
                };
                let app_name = pkg_info.name.clone();

                // Wrap the build in an inner closure that returns a
                // `tauri::Result` so `?` can be used freely on each
                // PredefinedMenuItem constructor without aborting setup
                // — the outer `match` still logs and continues if
                // anything fails (matches the original Menu::default
                // tolerance).
                let menu_result: tauri::Result<Menu<_>> = (|| {
                    let close_window_item = MenuItemBuilder::with_id(
                        FLOWSTATE_CLOSE_WINDOW_MENU_ID,
                        "Close Window",
                    )
                    .accelerator("CmdOrCtrl+Q")
                    .build(&handle)?;

                    let app_submenu = Submenu::with_items(
                        &handle,
                        app_name,
                        true,
                        &[
                            &PredefinedMenuItem::about(
                                &handle,
                                None,
                                Some(about_metadata),
                            )?,
                            &PredefinedMenuItem::separator(&handle)?,
                            &PredefinedMenuItem::services(&handle, None)?,
                            &PredefinedMenuItem::separator(&handle)?,
                            &PredefinedMenuItem::hide(&handle, None)?,
                            &PredefinedMenuItem::hide_others(&handle, None)?,
                            &PredefinedMenuItem::show_all(&handle, None)?,
                            &PredefinedMenuItem::separator(&handle)?,
                            &close_window_item,
                        ],
                    )?;

                    let edit_submenu = Submenu::with_items(
                        &handle,
                        "Edit",
                        true,
                        &[
                            &PredefinedMenuItem::undo(&handle, None)?,
                            &PredefinedMenuItem::redo(&handle, None)?,
                            &PredefinedMenuItem::separator(&handle)?,
                            &PredefinedMenuItem::cut(&handle, None)?,
                            &PredefinedMenuItem::copy(&handle, None)?,
                            &PredefinedMenuItem::paste(&handle, None)?,
                            &PredefinedMenuItem::select_all(&handle, None)?,
                        ],
                    )?;

                    let view_submenu = Submenu::with_items(
                        &handle,
                        "View",
                        true,
                        &[&PredefinedMenuItem::fullscreen(&handle, None)?],
                    )?;

                    // Window submenu intentionally omits close_window
                    // — see (2) in the block comment above.
                    let window_submenu = Submenu::with_items(
                        &handle,
                        "Window",
                        true,
                        &[
                            &PredefinedMenuItem::minimize(&handle, None)?,
                            &PredefinedMenuItem::maximize(&handle, None)?,
                        ],
                    )?;

                    Menu::with_items(
                        &handle,
                        &[
                            &app_submenu,
                            &edit_submenu,
                            &view_submenu,
                            &window_submenu,
                        ],
                    )
                })();

                match menu_result {
                    Ok(menu) => {
                        if let Err(e) = handle.set_menu(menu) {
                            tracing::warn!(
                                %e,
                                "failed to install custom app menu; ⌘Q close-window will be inoperative"
                            );
                        }
                    }
                    Err(e) => tracing::warn!(%e, "failed to build custom app menu"),
                }
            }

            // Global toggle hotkey: Cmd+Option+Shift+O.
            //
            // - Pressed while flowstate owns focus -> hide() (same path
            //   as the red traffic light on macOS).
            // - Pressed while flowstate is hidden / minimized / behind
            //   another app -> show() + unminimize() + set_focus().
            //   `set_focus()` calls [NSApp activateIgnoringOtherApps:YES]
            //   under the hood, so we steal focus from the frontmost
            //   app automatically — no extra NSApp.activate call.
            //
            // `is_focused()` (not `is_visible()`) is the right predicate:
            // if the window is visible but the user alt-tabbed to
            // another app, pressing the hotkey should bring flowstate
            // forward, not hide it.
            //
            // The plugin fires the callback for both Pressed AND
            // Released — gate on `ShortcutState::Pressed` so one
            // keystroke toggles exactly once.
            //
            // Cross-platform: the plugin handles macOS / Linux / Windows
            // differences internally (on macOS it uses Carbon's
            // `RegisterEventHotKey`, which needs no Accessibility
            // permission). Registration failure is non-fatal — if some
            // other app already owns the combo we log and continue so
            // the rest of startup proceeds.
            let toggle_shortcut = Shortcut::new(
                Some(Modifiers::META | Modifiers::ALT | Modifiers::SHIFT),
                Code::KeyO,
            );
            let shortcut_handle = app.handle().clone();
            if let Err(e) = app.global_shortcut().on_shortcut(
                toggle_shortcut,
                move |_app, _shortcut, event| {
                    if event.state() != ShortcutState::Pressed {
                        return;
                    }
                    let Some(window) = shortcut_handle.get_webview_window(MAIN_WINDOW_LABEL)
                    else {
                        return;
                    };
                    match window.is_focused() {
                        Ok(true) => {
                            let _ = window.hide();
                        }
                        _ => {
                            let _ = window.show();
                            let _ = window.unminimize();
                            let _ = window.set_focus();
                        }
                    }
                },
            ) {
                tracing::warn!(
                    %e,
                    "failed to register global toggle shortcut (Cmd+Opt+Shift+O); \
                     likely held by another app — continuing without it"
                );
            }

            // Orphan scan — first thing in setup, BEFORE we bind the
            // loopback HTTP port. If a previous flowstate was SIGKILL'd
            // (routine during `tauri dev` reload), its `opencode serve`
            // and `flowstate mcp-server` grandchildren reparent to PID
            // 1 and keep running on their old ports. Reap them now so
            // this flowstate's new-port allocation can't collide and
            // so zombie MCP proxies pointing at a dead port don't hang
            // the next orchestration turn. Unix-only; on non-Unix this
            // is a no-op returning 0.
            let reaped = orphan_scan::reap_orphaned_subprocesses();
            if reaped > 0 {
                tracing::info!(reaped, "startup orphan scan reaped stale subprocesses");
            }

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
                .expect("failed to resolve app data dir");
            std::fs::create_dir_all(&flowstate_root).expect("failed to create app data dir");
            std::fs::create_dir_all(flowstate_root.join("threads")).ok();

            // Open the flowstate-app-owned user config store. Lives in
            // its own file at <app_data_dir>/user_config.sqlite — a
            // separate database from the daemon's. SDK and app each
            // own their own SQLite; nothing about app-level UI config
            // belongs in the daemon's schema.
            let user_config_store =
                UserConfigStore::open(&flowstate_root).expect("failed to open user_config store");
            // Push the user's configured extra binary search paths
            // into the process-wide resolver before any provider
            // adapter spawns. The resolver consults this list right
            // after PATH walk and before the curated platform
            // fallbacks, giving users an explicit escape hatch for
            // CLIs installed to non-standard locations (a frequent
            // need on Windows where Tauri's GUI-launch PATH is much
            // narrower than the user's shell PATH). The Tauri
            // command `refresh_binary_search_paths` re-runs this
            // helper after the settings UI writes a new value.
            apply_binary_search_paths_from_config(&user_config_store);
            // Keep a clone for the orchestration adapters below —
            // `app.manage` takes ownership but UserConfigStore is
            // cheap to clone (Arc<Mutex<Connection>> inside).
            let user_config_for_orch = user_config_store.clone();
            // Same trick for the sleep-prevention controller: it
            // reads the `system.caffeinate` toggle on every turn
            // boundary to decide whether to arm the OS hook (macOS
            // `caffeinate` subprocess; Windows
            // `SetThreadExecutionState`). Constructed below inside
            // the daemon task once we have a tokio Handle to spawn
            // any watcher onto.
            #[cfg(any(target_os = "macos", windows))]
            let user_config_for_caffeinate = user_config_store.clone();
            app.manage(user_config_store);

            // User-defined global MCP servers (`~/.flowstate/mcp.json`).
            // Cheap to clone (just a path); each adapter calls
            // `load()` on the registry inside its own spawn path so
            // file edits are picked up by new sessions without a
            // daemon restart. Exposed to the Settings UI via
            // `get_user_mcp_servers` / `set_user_mcp_servers` Tauri
            // commands. Created here (outside the async spawn)
            // because `app.manage` requires `&mut App`, which isn't
            // `Send`.
            let user_mcp_registry =
                zenui_provider_api::UserMcpRegistry::new(&flowstate_root);
            let user_mcp_for_adapters = user_mcp_registry.clone();
            app.manage(user_mcp_registry);

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

            // Phase 5 — shared channel for the loopback HTTP base URL.
            // Every `#[tauri::command]` in the app-layer block reads
            // its receiver via `DaemonClient`; `loopback_http::spawn`
            // publishes once the transport has bound.
            let daemon_base_url = DaemonBaseUrl::new();
            let daemon_base_url_for_spawn = daemon_base_url.clone();
            // Register the channel as Tauri state so the command
            // handlers can obtain a `DaemonClient` per call.
            app.manage(daemon_base_url.clone());

            // CLI bridge channel — the `flow` CLI POSTs project paths
            // to `/api/open-project` (handled in
            // `flowstate-app-layer::http`); the handler pushes the
            // path onto this mpsc, and the bridge task below
            // forwards each path to the webview as an
            // `"open-project"` event. Mpsc (not watch) so two rapid
            // `flow .` invocations queue independently rather than
            // coalesce. The webview handler reads the user's saved
            // default provider/model/effort/permission_mode and
            // calls `create_project` + `start_session` (existing
            // app-store flow).
            let (open_project_tx, mut open_project_rx) =
                tokio::sync::mpsc::unbounded_channel::<String>();
            let open_project_emit_handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                while let Some(path) = open_project_rx.recv().await {
                    tracing::info!(path = %path, "CLI open-project request received");
                    // Bring the main window to front so the user sees
                    // the new thread immediately. `get_webview_window`
                    // returns None if the main window was already
                    // destroyed (shouldn't happen on macOS where close
                    // hides; on other OSes close exits the process).
                    if let Some(main) =
                        open_project_emit_handle.get_webview_window(MAIN_WINDOW_LABEL)
                    {
                        let _ = main.show();
                        let _ = main.unminimize();
                        let _ = main.set_focus();
                    }
                    if let Err(e) = open_project_emit_handle.emit("open-project", &path) {
                        tracing::warn!(%e, path = %path, "emit open-project event failed");
                    }
                }
            });

            // Clone the app handle once for the provisioning reporter
            // below. The spawn block moves the original; this clone
            // lives inside the spawn_blocking closure so each
            // ProvisionEvent can be forwarded to the webview via
            // `emit("provision", …)`.
            let provision_app_handle = app.handle().clone();

            // Clone for the daemon task's end-of-scope exit trigger.
            // After `graceful_shutdown` + explicit drops complete, the
            // daemon task sets `SHUTDOWN_COMPLETE` and calls
            // `app_handle_for_daemon.exit(0)`. That re-enters the
            // `RunEvent::ExitRequested` gate in `.run(|_,_| …)` below
            // which now observes `SHUTDOWN_COMPLETE == true` and lets
            // Tauri tear the event loop down cleanly.
            let app_handle_for_daemon = app.handle().clone();

            // Run the daemon on Tauri's existing tokio runtime so the
            // process has exactly one thread pool. The previous shape
            // (std::thread::spawn + bootstrap_core's own runtime) was
            // a workaround for "cannot start a runtime from within a
            // runtime"; bootstrap_core_async removes that need by
            // letting us share the host runtime.
            tauri::async_runtime::spawn(async move {
                // Front-load Node.js + provider-SDK node_modules
                // hydration BEFORE any adapter construction so the
                // first user-initiated turn can't be the thing that
                // pays for a 30–90 second first-launch download. On
                // warm caches every step is a sentinel hit — sub-
                // millisecond.
                //
                // Events get emitted to the webview as a Tauri
                // `provision` event; the `<ProvisioningSplash />`
                // component listens and renders a full-screen loading
                // overlay until the runtime's own `welcome` message
                // arrives and the app flips to `ready: true`.
                //
                // Runs on `spawn_blocking` because everything inside
                // (`ureq::get`, tar extract, `npm install`) is sync IO
                // that would otherwise block the tokio worker this
                // task was scheduled on.
                let reporter_app_handle = provision_app_handle.clone();
                let provision_result = tokio::task::spawn_blocking(move || {
                    let reporter: Box<
                        flowstate_app_layer::provision::ProvisionReporter,
                    > = Box::new(move |event| {
                        // Errors here are not actionable (webview may
                        // not be fully wired yet during early boot).
                        // We still log so a missing splash transition
                        // is debuggable.
                        if let Err(e) = reporter_app_handle.emit("provision", &event) {
                            tracing::debug!(%e, "emit provision event failed");
                        }
                    });
                    flowstate_app_layer::provision::provision_runtimes(&reporter)
                })
                .await;

                // Provisioning is non-fatal: failures (no network,
                // npm registry hiccup, etc.) populate the
                // `ProvisionState` shared with the frontend so the
                // Settings page can render Retry banners + a red dot
                // on the sidebar Settings icon. Even with every phase
                // failed we still boot the daemon — CLI-style providers
                // (claude-code, etc.) and the rest of the app stay
                // usable, and the user gets a one-click recovery path
                // instead of a frozen splash screen.
                match provision_result {
                    Ok(outcome) => {
                        if outcome.failures.is_empty() {
                            tracing::info!("provision_runtimes completed cleanly");
                        } else {
                            for f in &outcome.failures {
                                tracing::error!(
                                    phase = %f.phase.as_str(),
                                    error = %f.error,
                                    "provisioning phase failed; daemon will boot but provider is unavailable until retried"
                                );
                            }
                        }
                        if let Some(state) =
                            provision_app_handle.try_state::<ProvisionState>()
                        {
                            state.set(outcome.failures);
                        }
                    }
                    Err(e) => {
                        // Spawn task panicked — different from a phase
                        // failure. We still boot but record a synthetic
                        // failure entry so the user sees something in
                        // Settings rather than the symptom (every SDK
                        // provider broken with no explanation).
                        tracing::error!(%e, "provision_runtimes task panicked");
                        if let Some(state) =
                            provision_app_handle.try_state::<ProvisionState>()
                        {
                            state.set(vec![
                                flowstate_app_layer::provision::ProvisionFailure {
                                    phase:
                                        flowstate_app_layer::provision::ProvisionPhase::Node,
                                    error: format!("provisioning task panicked: {e}"),
                                },
                            ]);
                        }
                    }
                }

                let mut config = DaemonConfig::with_project_root(flowstate_root.clone());
                config.idle_timeout = Duration::MAX;
                // Advertised to every connected client via the
                // Bootstrap wire payload. Keeping it here means
                // `runtime-core` never knows its host app's name.
                config.app_name = "Flowstate".to_string();

                // Shared handle every provider adapter reads at
                // session-spawn time to discover the loopback HTTP
                // base URL + bearer token for the `flowstate
                // mcp-server` subprocesses they launch. Starts empty;
                // `loopback_http::spawn` (further down) populates it
                // once the listener binds. Adapters that see it as
                // empty skip MCP orchestration wiring — graceful
                // degradation to "Claude-SDK-only orchestration",
                // which is the pre-refactor behaviour.
                let ipc_handle = OrchestrationIpcHandle::new();

                // Phase 5 — publisher for the DaemonClient channel.
                // The DaemonClient held in Tauri state reads the
                // receiver side; `loopback_http::spawn` below
                // publishes the base URL once the transport binds,
                // at which point the app-layer Tauri commands start
                // routing via HTTP. Pre-bind, they return a
                // "base URL not yet available" error — the webview
                // already handles that shape as a command error, so
                // a race during startup surfaces as a visible retry
                // rather than a silent corruption.
                let daemon_base_url = daemon_base_url_for_spawn.clone();

                // Construct the provider adapters the app wants to
                // expose. Adding or removing providers now lives in a
                // single call site here — `daemon-core` stays
                // provider-agnostic. Per-provider `default_enabled()`
                // decides which are on out of the box.
                //
                // Adapters that want orchestration tooling take a
                // clone of `ipc_handle` through their constructor.
                // Adapters that don't (Claude SDK — registers
                // in-process via `createSdkMcpServer`) keep the
                // existing single-arg constructor. Threading is
                // per-provider because each bridge wires its MCP
                // subprocess differently (`SessionConfig.mcpServers`
                // for Copilot SDK, `.mcp.json` for the CLIs,
                // per-session `opencode.json` for opencode).
                // Sleep-prevention controller (macOS + Windows):
                // built here so we can register it as an extra
                // `TurnLifecycleObserver` BEFORE the bootstrap call
                // wires the runtime's observer chain. The controller
                // is `TurnLifecycleObserver`-aware: on the 0→1 turn
                // transition it arms the OS sleep hook (macOS:
                // `caffeinate -d -t <timeout>`; Windows:
                // `SetThreadExecutionState(ES_CONTINUOUS |
                // ES_SYSTEM_REQUIRED | ES_DISPLAY_REQUIRED)`), on
                // 1→0 it releases it. Re-registers the same
                // `Arc<CaffeinateController>` in two places (here as
                // an observer, below as Tauri-managed state) so the
                // settings UI can also drive `force_kill` / `status`
                // through the Tauri command boundary.
                //
                // On macOS the controller spawns watcher tasks on
                // the current tokio runtime (this Tauri spawn block
                // IS running inside it). Windows has no watcher —
                // the execution-state flag stays armed until we
                // clear it. Either way, the controller reads the
                // `system.caffeinate` setting from `UserConfigStore`
                // on every transition so a toggle flip is picked up
                // at the next turn boundary without any plumbing.
                #[cfg(any(target_os = "macos", windows))]
                let caffeinate_controller = {
                    let ctrl = flowstate_app_layer::caffeinate::CaffeinateController::new(
                        user_config_for_caffeinate,
                        tokio::runtime::Handle::current(),
                    );
                    config.extra_turn_observers.push(
                        ctrl.clone() as Arc<dyn zenui_runtime_core::TurnLifecycleObserver>,
                    );
                    ctrl
                };

                // User MCP registry: constructed outside the spawn
                // (see `let user_mcp_registry = ...` above) and moved
                // in here for adapter construction. Each adapter takes
                // a clone; on session spawn it calls
                // `registry.load()` to read fresh `~/.flowstate/mcp.json`
                // and merges entries into its provider-native MCP
                // injection path.
                let user_mcp_registry = user_mcp_for_adapters;

                // Read the user's preferred provider idle timeout from
                // `user_config.sqlite` (key: "provider.idle_timeout_mins").
                // Falls back to 30 minutes when unset or out of range.
                // Applies to Claude SDK, GitHub Copilot, and OpenCode
                // bridges. Takes effect immediately — adapters are
                // constructed once at daemon start each launch.
                let idle_timeout_secs: u64 = user_config_for_orch
                    .get("provider.idle_timeout_mins")
                    .ok()
                    .flatten()
                    .and_then(|s| s.parse::<u64>().ok())
                    .map(|m| m.clamp(5, 480) * 60)
                    .unwrap_or(30 * 60);

                config.adapters = vec![
                    Arc::new(ClaudeSdkAdapter::new_with_orchestration_and_idle_ttl(
                        flowstate_root.clone(),
                        Some(ipc_handle.clone()),
                        Some(user_mcp_registry.clone()),
                        Some(idle_timeout_secs),
                    )) as Arc<dyn ProviderAdapter>,
                    Arc::new(CodexAdapter::new_with_orchestration_and_idle_ttl(
                        flowstate_root.clone(),
                        Some(ipc_handle.clone()),
                        Some(user_mcp_registry.clone()),
                        Some(idle_timeout_secs),
                    )),
                    Arc::new(GitHubCopilotAdapter::new_with_orchestration_and_idle_ttl(
                        flowstate_root.clone(),
                        Some(ipc_handle.clone()),
                        Some(user_mcp_registry.clone()),
                        Some(idle_timeout_secs),
                    )),
                    // Opencode runs as a shared-server singleton for
                    // startup-latency reasons (one `opencode serve`
                    // child reused across every flowstate-opencode
                    // session). The shared server's `opencode.json`
                    // registers the flowstate MCP with a sentinel
                    // session id so opencode-side agents DO see
                    // the orchestration tools. Tradeoff: every
                    // opencode-side tool call arrives at the runtime
                    // with the same origin.session_id — see the
                    // docstring on `OPENCODE_SHARED_SESSION_ID` for
                    // the implications.
                    Arc::new(OpenCodeAdapter::new_with_orchestration_and_idle_ttl(
                        flowstate_root.clone(),
                        Some(ipc_handle.clone()),
                        Some(user_mcp_registry.clone()),
                        Some(Duration::from_secs(idle_timeout_secs)),
                    )),
                ];

                let core = bootstrap_core_async(&config)
                    .await
                    .expect("daemon bootstrap failed");

                // Wire the app-layer orchestration adapters now that
                // the runtime exists. Metadata provider lets the
                // orchestration dispatcher read sidebar titles;
                // worktree provisioner lets agents spin up git
                // worktrees via the `create_worktree` / `spawn_in_worktree`
                // MCP tools. Both hold their own clones of the
                // UserConfigStore + a Weak back-ref into RuntimeCore.
                core.runtime_core.install_metadata_provider(Arc::new(
                    AppMetadataProviderImpl::new(user_config_for_orch.clone()),
                ));
                core.runtime_core.install_worktree_provisioner(Arc::new(
                    WorktreeProvisionerImpl::new(
                        user_config_for_orch.clone(),
                        Arc::downgrade(&core.runtime_core),
                    ),
                ));

                // Usage analytics subscriber. Runs for the life of
                // the daemon, filtering the RuntimeEvent broadcast
                // for TurnCompleted events and writing one row per
                // turn to the usage sqlite. Missing this task is
                // never fatal — a broadcast lag skips some telemetry
                // but never corrupts runtime state. We subscribe
                // BEFORE the transport's serve() so no event is lost
                // between bootstrap and the first client connect.
                if let Some(writer) = usage_writer {
                    let mut rx = core.runtime_core.subscribe();
                    tauri::async_runtime::spawn(async move {
                        loop {
                            match rx.recv().await {
                                Ok(RuntimeEvent::TurnCompleted { session, turn, .. }) => {
                                    let event = UsageEvent::from_turn(&session, &turn);
                                    if let Err(e) = writer.record_turn(&event) {
                                        tracing::warn!("record turn usage failed: {e}");
                                    }
                                }
                                // Persist the latest snapshot of every
                                // rate-limit bucket so the chat-toolbar
                                // chips render their last-known values
                                // on next app boot. The Anthropic 5h /
                                // weekly windows only arrive as a
                                // side-effect of inference responses —
                                // there's no API to poll — so without
                                // this the chips stay blank until the
                                // user sends a message after every
                                // restart.
                                Ok(RuntimeEvent::RateLimitUpdated { info }) => {
                                    if let Err(e) = writer.upsert_rate_limit(&info) {
                                        tracing::warn!(
                                            "persist rate_limit_cache failed: {e}"
                                        );
                                    }
                                }
                                Ok(_) => {}
                                Err(RecvError::Lagged(n)) => {
                                    tracing::warn!(
                                        "usage subscriber lagged by {n} events; continuing"
                                    );
                                }
                                Err(RecvError::Closed) => break,
                            }
                        }
                    });
                }

                let bound = transport.bind().expect("transport bind failed");
                let observer: Arc<dyn ConnectionObserver> = core.lifecycle.clone();
                let handle = bound
                    .serve(core.runtime_core.clone(), observer.clone())
                    .expect("transport serve failed");

                // Loopback HTTP transport, mounted alongside the
                // Tauri transport. Both share the same
                // `Arc<RuntimeCore>` so every route reflects the live
                // runtime. The `flowstate mcp-server` subprocesses
                // that provider adapters launch per-session read the
                // handshake file this call writes to discover the
                // port + auth token. Failure is non-fatal: the Tauri
                // UI works without it, only cross-provider
                // orchestration degrades (to "Claude SDK only" —
                // which is the pre-refactor state).
                // Bind return value ignored by design — `LoopbackHttp`
                // owns the server's `TransportHandle`, and the whole
                // struct lives for the duration of this spawned task
                // (which itself lives for the life of the app). When
                // the task returns, dropping `_loopback` aborts the
                // HTTP accept loop cleanly.
                // Phase 4 — open a dedicated UsageStore for the HTTP
                // handlers. Each `UsageStore::open` allocates a
                // fresh rusqlite `Connection`; SQLite handles the
                // concurrency via WAL + per-connection locks, so
                // this third handle sits alongside the writer (in
                // the analytics subscriber above) and the reader
                // (managed by Tauri's `app.manage`) without
                // contention.
                let usage_http: Option<Arc<flowstate_app_layer::usage::UsageStore>> =
                    match flowstate_app_layer::usage::UsageStore::open(&flowstate_root) {
                        Ok(s) => Some(Arc::new(s)),
                        Err(e) => {
                            tracing::warn!(
                                "failed to open HTTP usage store: {e}; /api/usage/* will 503"
                            );
                            None
                        }
                    };
                let _loopback = match loopback_http::spawn(
                    &flowstate_root,
                    core.runtime_core.clone(),
                    observer.clone(),
                    ipc_handle.clone(),
                    user_config_for_orch.clone(),
                    usage_http,
                    daemon_base_url.clone(),
                    open_project_tx.clone(),
                ) {
                    Ok(l) => Some(l),
                    Err(err) => {
                        tracing::warn!(
                            %err,
                            "loopback HTTP transport failed to start; \
                             cross-provider orchestration will be unavailable"
                        );
                        None
                    }
                };

                // Signal main thread AFTER serve() has managed TauriDaemonState.
                // This guarantees the connect command can access it.
                //
                // Marker logged at info so a future "splash stuck on
                // Finishing up…" report can be triaged by reading the
                // daemon log: if this line is present the daemon came
                // up cleanly and the issue is on the frontend connect
                // path; if it's absent the daemon spawn task crashed
                // somewhere upstream (e.g. an `expect` in
                // `bootstrap_core_async`).
                tracing::info!("daemon ready; connect command available");
                ready_tx
                    .send(DaemonReadyPayload {
                        lifecycle: core.lifecycle.clone(),
                        #[cfg(any(target_os = "macos", windows))]
                        caffeinate: caffeinate_controller.clone(),
                    })
                    .expect("failed to signal ready");

                core.lifecycle.wait_for_shutdown().await;

                let _ = graceful_shutdown(
                    core.runtime_core.clone(),
                    core.lifecycle.clone(),
                    &config.adapters,
                    config.shutdown_grace,
                )
                .await;

                // `handle.shutdown()` consumes the handle; it's
                // already gone by the time we reach the explicit-drop
                // block below, so no `drop(handle)` is needed.
                handle.shutdown().await;

                // Explicit drops to force any residual Arc chains
                // (RuntimeCore.adapters, transport managed state) to
                // collapse BEFORE we signal done. Belt-and-braces with
                // the per-adapter kill that `graceful_shutdown` now
                // performs; one of the two reliably terminates each
                // subprocess but both together rule out any "Drop
                // never fired" edge case.
                drop(_loopback);
                drop(core);
                drop(config);

                // Tell the Tauri exit gate (RunEvent::ExitRequested in
                // `.run(...)` below) that shutdown has fully drained.
                // Then request exit — this re-enters the gate with
                // `SHUTDOWN_COMPLETE == true`, so Tauri tears the
                // event loop down and the process terminates cleanly.
                SHUTDOWN_COMPLETE.store(true, Ordering::SeqCst);
                app_handle_for_daemon.exit(0);
            });

            // Don't block setup waiting for the daemon to come up.
            //
            // Why: on a first launch (or any launch where the embedded
            // Node + provider `node_modules` aren't cached yet) the
            // daemon task above spends 10–90 s downloading Node.js and
            // running `npm install`. If we `recv()` here we hold the
            // Tauri setup closure open for that whole window, which
            // means Tauri never creates the webview window — the user
            // sees a dock icon and nothing else while provisioning
            // runs, with no splash and no "the app is doing something"
            // signal. The whole point of `<ProvisioningSplash />` is
            // to render during exactly that window, so setup must
            // return NOW and let the webview mount.
            //
            // We still need the `AppLifecycle` state + the SIGTERM /
            // SIGINT handler (both depend on the daemon's
            // `LifecycleHandle`), so offload the wait-and-wire onto a
            // std thread that owns an `AppHandle` clone. When the
            // daemon signals ready, the thread calls
            // `AppHandle::manage` and spawns the signal handler via
            // `tauri::async_runtime::spawn` exactly as the inline path
            // used to. Commands that read `State<AppLifecycle>` use
            // `try_state` (see the `on_window_event` handler below),
            // so the brief window between setup-return and
            // daemon-ready is safe: a close before ready simply skips
            // `request_shutdown()` — the daemon task's own Drop chain
            // still runs because `tauri::generate_context!().run()`
            // returning drops every spawned task.
            {
                let app_handle_for_wire = app.handle().clone();
                std::thread::Builder::new()
                    .name("flowstate-daemon-ready".into())
                    .spawn(move || {
                        let payload = match ready_rx.recv() {
                            Ok(p) => p,
                            Err(e) => {
                                // Daemon task early-exited (e.g.
                                // provisioning failed). The splash
                                // already rendered a `Failed` phase
                                // event — leave the webview on it so
                                // the user sees why, rather than
                                // panicking the host process as we
                                // used to.
                                tracing::error!(
                                    %e,
                                    "daemon never signalled ready; SIGTERM handler + \
                                     AppLifecycle state will not be wired"
                                );
                                return;
                            }
                        };
                        let lifecycle = payload.lifecycle.clone();
                        app_handle_for_wire.manage(AppLifecycle {
                            lifecycle: lifecycle.clone(),
                        });
                        #[cfg(any(target_os = "macos", windows))]
                        app_handle_for_wire.manage(CaffeinateState(payload.caffeinate.clone()));

                        // SIGTERM / SIGINT handler.
                        //
                        // Why this exists: without it, SIGTERM (e.g.
                        // `tauri dev` hot-reload, `pkill`, systemd
                        // stop, macOS Activity Monitor "Quit")
                        // terminates flowstate without running any
                        // Drop code — `opencode serve` and its
                        // grandchildren (including the
                        // `flowstate mcp-server` proxies) orphan to
                        // PID 1 and keep running, pointing at the
                        // now-dead loopback port. Users observed ~22
                        // such orphans accumulating during a normal
                        // dev session.
                        //
                        // This handler intercepts both signals and
                        // walks the existing graceful-shutdown path:
                        //   1. `lifecycle.request_shutdown()` tips the
                        //      daemon task out of its
                        //      `wait_for_shutdown()` await.
                        //   2. Daemon proceeds to
                        //      `graceful_shutdown()` → drops
                        //      `DaemonConfig::adapters` → drops
                        //      `OpenCodeAdapter` → drops
                        //      `OpenCodeServer` → its Drop impl
                        //      sends `killpg(pgid, SIGTERM)` to the
                        //      whole opencode process group.
                        //   3. `app_handle.exit(0)` breaks the Tauri
                        //      event loop so the process actually
                        //      exits instead of hanging.
                        //
                        // SIGKILL is still uncatchable — the startup
                        // orphan scan (see `loopback_http::spawn` /
                        // orphan-scan helper) handles that case.
                        let app_handle_for_signal = app_handle_for_wire.clone();
                        let lifecycle_for_signal = lifecycle.clone();
                        tauri::async_runtime::spawn(async move {
                            // Unix path: SIGTERM + SIGINT, the canonical
                            // "please stop now" signals. SIGKILL is still
                            // uncatchable; the orphan-scan helper covers
                            // that path.
                            #[cfg(unix)]
                            let signal_name = {
                                let mut sigterm = match tokio::signal::unix::signal(
                                    tokio::signal::unix::SignalKind::terminate(),
                                ) {
                                    Ok(s) => s,
                                    Err(err) => {
                                        tracing::warn!(
                                            %err,
                                            "failed to install SIGTERM handler; graceful \
                                             shutdown on external signals will be unavailable"
                                        );
                                        return;
                                    }
                                };
                                let mut sigint = match tokio::signal::unix::signal(
                                    tokio::signal::unix::SignalKind::interrupt(),
                                ) {
                                    Ok(s) => s,
                                    Err(err) => {
                                        tracing::warn!(%err, "failed to install SIGINT handler");
                                        return;
                                    }
                                };
                                tokio::select! {
                                    _ = sigterm.recv() => "SIGTERM",
                                    _ = sigint.recv() => "SIGINT",
                                }
                            };

                            // Windows path: there's no SIGTERM. Watch the
                            // three console-control events that map onto
                            // "user / system asked us to stop":
                            //   * Ctrl+C in an attached console
                            //   * Ctrl+Close — console host closing
                            //   * Ctrl+Shutdown — system is shutting down
                            // ctrl_logoff and ctrl_break are intentionally
                            // skipped: logoff races with WM_ENDSESSION
                            // (Tauri already wires that up via window
                            // lifecycle), and ctrl_break is too aggressive
                            // for a graceful teardown. TerminateProcess
                            // (the SIGKILL analogue) is still uncatchable;
                            // orphan-scan covers that case here too.
                            #[cfg(windows)]
                            let signal_name = {
                                let mut ctrl_c = match tokio::signal::windows::ctrl_c() {
                                    Ok(s) => s,
                                    Err(err) => {
                                        tracing::warn!(
                                            %err,
                                            "failed to install Ctrl+C handler; graceful \
                                             shutdown on external signals will be unavailable"
                                        );
                                        return;
                                    }
                                };
                                let mut ctrl_close = match tokio::signal::windows::ctrl_close() {
                                    Ok(s) => s,
                                    Err(err) => {
                                        tracing::warn!(%err, "failed to install Ctrl+Close handler");
                                        return;
                                    }
                                };
                                let mut ctrl_shutdown = match tokio::signal::windows::ctrl_shutdown() {
                                    Ok(s) => s,
                                    Err(err) => {
                                        tracing::warn!(%err, "failed to install Ctrl+Shutdown handler");
                                        return;
                                    }
                                };
                                tokio::select! {
                                    _ = ctrl_c.recv() => "Ctrl+C",
                                    _ = ctrl_close.recv() => "Ctrl+Close",
                                    _ = ctrl_shutdown.recv() => "Ctrl+Shutdown",
                                }
                            };

                            tracing::info!(
                                signal = signal_name,
                                "received termination signal; requesting daemon shutdown"
                            );
                            lifecycle_for_signal.request_shutdown();
                            app_handle_for_signal.exit(0);
                        });
                    })
                    .expect("failed to spawn flowstate-daemon-ready thread");
            }

            Ok(())
        })
        .invoke_handler({
            // `tauri::generate_handler!` is a proc macro that doesn't
            // process inline `#[cfg]` attributes on its arguments, so
            // platform-conditional commands need to be added through
            // a small helper macro that splices an extra-name list
            // into the canonical handler list. The macOS-only
            // caffeinate commands flow through this gate; every
            // other command is unconditional.
            macro_rules! flowstate_invoke_handler {
                ($($extra:ident),* $(,)?) => {
                    tauri::generate_handler![
                        transport_tauri::commands::connect,
                        transport_tauri::commands::handle_message,
                        get_git_branch,
                        list_git_branches,
                        list_git_worktrees,
                        git_checkout,
                        git_create_branch,
                        git_delete_branch,
                        create_git_worktree,
                        remove_git_worktree,
                        resolve_git_root,
                        path_exists,
                        read_file_as_base64,
                        get_git_diff_summary,
                        get_git_diff_file,
                        watch_git_diff_summary,
                        stop_git_diff_summary,
                        list_project_files,
                        reindex_project_files,
                        list_directory,
                        read_project_file,
                        write_project_file,
                        write_project_file_bytes,
                        markdown_save_pasted_image,
                        markdown_copy_svg_as_png,
                        create_project_file,
                        create_project_dir,
                        rename_project_path,
                        move_project_path,
                        trash_project_path,
                        search_file_contents,
                        stop_content_search,
                        open_in_editor,
                        pty_open,
                        pty_write,
                        pty_resize,
                        pty_pause,
                        pty_resume,
                        pty_kill,
                        popout_thread,
                        set_window_always_on_top,
                        get_user_config,
                        set_user_config,
                        get_user_mcp_servers,
                        set_user_mcp_servers,
                        set_session_display,
                        get_session_display,
                        list_session_display,
                        delete_session_display,
                        set_project_display,
                        get_project_display,
                        list_project_display,
                        delete_project_display,
                        set_project_worktree,
                        get_project_worktree,
                        list_project_worktree,
                        delete_project_worktree,
                        get_usage_summary,
                        get_usage_timeseries,
                        get_top_sessions,
                        get_usage_by_agent,
                        get_usage_by_agent_role,
                        get_rate_limit_cache,
                        get_app_data_dir,
                        get_log_dir,
                        get_cache_dir,
                        clear_runtime_cache,
                        get_provision_failures,
                        retry_provision_phase,
                        install_cli::install_cli,
                        install_cli::install_cli_status,
                        refresh_binary_search_paths,
                        list_binary_search_paths,
                        quit_app,
                        $($extra,)*
                    ]
                };
            }
            #[cfg(any(target_os = "macos", windows))]
            { flowstate_invoke_handler!(caffeinate_status, caffeinate_kill, caffeinate_refresh) }
            #[cfg(not(any(target_os = "macos", windows)))]
            { flowstate_invoke_handler!() }
        })
        .on_window_event(|window, event| {
            // The handler is registered globally and fires for *every*
            // window — main + every `thread-<id>` popout. Main-window
            // policy (hide-on-close, daemon shutdown) must be gated
            // behind this label check; otherwise closing a popout
            // either leaks its WKWebView subprocess (macOS hide-and-
            // never-destroy) or quits the entire app (Linux/Windows
            // exit(0) path). Popouts get the default Tauri close
            // behaviour, which destroys the window and reaps the
            // webview process — the connect broadcast loop in
            // `transport-tauri` self-cleans when its channel send
            // fails on teardown.
            let is_main = window.label() == MAIN_WINDOW_LABEL;
            match event {
                // Red traffic light (or frontend-initiated
                // `getCurrentWindow().close()`).
                //
                // On macOS we follow platform convention: closing the
                // window *hides* it instead of quitting the process.
                // The daemon, `opencode serve`, PTY shells, and any
                // running sessions keep going in the background. The
                // user brings the window back by clicking the dock
                // icon (handled by `RunEvent::Reopen` in the run
                // closure below) and actually quits with Cmd+Q, which
                // goes straight to `RunEvent::ExitRequested`.
                //
                // On other platforms closing really does mean quit —
                // there's no dock-style reopen affordance, so keeping
                // a hidden window around would just orphan the
                // process. Fall through to the original shutdown path
                // there.
                //
                // Single macOS arm with branch on label. Popouts need
                // `window.destroy()` explicitly: Tauri/Tao's default
                // close path on macOS only orders the window out, it
                // does NOT release the NSWindow / WKWebView. Without
                // an explicit destroy, the WKWebView (visible as a
                // `tauri://localhost` row in Activity Monitor) stays
                // alive indefinitely, holding 100s of MB per popout
                // and keeping its Rust-side `connect` broadcast
                // subscription open.
                // Main window: red traffic light (or any close path)
                // quits the app, same on every platform. Routes through
                // `app.exit(0)` so the two-phase shutdown gate at
                // `RunEvent::ExitRequested` runs the graceful daemon
                // teardown before the process actually terminates.
                // Previously on macOS this hid the window instead — bad
                // DX, made the red button feel broken.
                tauri::WindowEvent::CloseRequested { .. } if is_main => {
                    if let Some(pty) = window.try_state::<PtyManager>() {
                        pty.kill_all();
                    }
                    if let Some(state) = window.try_state::<AppLifecycle>() {
                        state.lifecycle.request_shutdown();
                    }
                    window.app_handle().exit(0);
                }
                #[cfg(target_os = "macos")]
                tauri::WindowEvent::CloseRequested { api, .. } => {
                    // Popouts on macOS need explicit destroy: Tauri/Tao's
                    // default close path only orders the NSWindow out and
                    // leaves the WKWebView alive. Stop the default first
                    // so the two paths can't race.
                    api.prevent_close();
                    tracing::info!(
                        label = %window.label(),
                        "popout close requested; destroying window"
                    );
                    if let Err(e) = window.destroy() {
                        tracing::warn!(
                            label = %window.label(),
                            error = %e,
                            "popout window.destroy() failed"
                        );
                    }
                }
                // Still wire the Destroyed path as a belt-and-braces
                // fallback — any code path that destroys the main
                // window without going through CloseRequested (plugin
                // teardown, OS-driven close) still trips the daemon
                // shutdown. For popouts this is a no-op: PtyManager
                // and AppLifecycle are global state, so calling them
                // here would tear down terminals and the daemon that
                // belong to the main window.
                tauri::WindowEvent::Destroyed if is_main => {
                    if let Some(pty) = window.try_state::<PtyManager>() {
                        pty.kill_all();
                    }
                    if let Some(state) = window.try_state::<AppLifecycle>() {
                        state.lifecycle.request_shutdown();
                    }
                }
                tauri::WindowEvent::Destroyed => {
                    tracing::info!(label = %window.label(), "popout window destroyed");
                }
                _ => {}
            }
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app_handle, event| {
            // macOS dock-icon click. When the main window is hidden
            // (which is what the `CloseRequested` arm above does on
            // macOS) clicking the dock icon should restore it —
            // otherwise there's no way back in short of Cmd+Tab +
            // Cmd+N-style tricks. Only the main window is targeted:
            // popouts that are still open are visible already, and
            // popouts that the user closed must NOT be resurrected
            // (they don't even exist anymore now that popout close
            // actually destroys the window).
            #[cfg(target_os = "macos")]
            if let tauri::RunEvent::Reopen {
                has_visible_windows, ..
            } = &event
            {
                if !*has_visible_windows {
                    if let Some(main) = app_handle.get_webview_window(MAIN_WINDOW_LABEL) {
                        let _ = main.show();
                        let _ = main.unminimize();
                        let _ = main.set_focus();
                    }
                }
            }

            // Two-phase exit gate. The full sequence for a red-traffic
            // light / Cmd+Q / SIGTERM close is:
            //
            //   1. User close → `request_shutdown()` + `exit(0)`.
            //   2. Tauri fires `RunEvent::ExitRequested` here with
            //      `SHUTDOWN_COMPLETE == false`. We call
            //      `api.prevent_exit()` so Tauri keeps the event loop
            //      running. The daemon task keeps getting CPU time.
            //   3. Daemon task's `wait_for_shutdown()` returns (it was
            //      tipped by step 1). It runs `graceful_shutdown`,
            //      which invokes `adapter.shutdown()` on every
            //      provider — opencode sends killpg(SIGTERM) to its
            //      process group (reaping `flowstate mcp-server`
            //      grandchildren along the way), the per-session CLI
            //      adapters sweep their cached children with
            //      `start_kill`.
            //   4. Daemon task explicitly drops `handle`, `_loopback`,
            //      `core`, `config`, then sets
            //      `SHUTDOWN_COMPLETE = true` and calls `exit(0)`.
            //   5. Tauri fires `RunEvent::ExitRequested` a second
            //      time. This branch now observes
            //      `SHUTDOWN_COMPLETE == true` and returns without
            //      calling `prevent_exit()`, so Tauri proceeds to
            //      `RunEvent::Exit` and the process terminates.
            //
            // A 10 s watchdog thread installed on the first entry
            // force-exits the process if the daemon task wedges (e.g.
            // an adapter.shutdown that somehow exceeds its outer
            // timeout). Residual orphans in that pathological case
            // are still reaped by the startup orphan scan on next
            // launch (see `orphan_scan::reap_orphaned_subprocesses`).
            if let tauri::RunEvent::ExitRequested { api, .. } = event {
                if SHUTDOWN_COMPLETE.load(Ordering::SeqCst) {
                    // Daemon task finished and re-entered exit(0).
                    // Don't prevent — let Tauri take the process down.
                    return;
                }

                // First (or still in-progress) entry. Keep the event
                // loop alive so the daemon task can run its async
                // teardown to completion.
                api.prevent_exit();

                // Idempotent kicks — CloseRequested / the signal
                // handler have almost certainly already done these,
                // but for Cmd+Q on macOS the window handler didn't
                // fire, so this is the only place the request gets
                // raised before the daemon task notices.
                if let Some(state) = app_handle.try_state::<AppLifecycle>() {
                    state.lifecycle.request_shutdown();
                }
                if let Some(pty) = app_handle.try_state::<PtyManager>() {
                    pty.kill_all();
                }

                // Install the shutdown watchdog exactly once. Runs on
                // a std::thread (not a Tauri-owned task) so it's
                // unaffected by whatever the tokio runtime is doing.
                if !SHUTDOWN_STARTED.swap(true, Ordering::SeqCst) {
                    let app_handle_for_watchdog = app_handle.clone();
                    if let Err(e) = std::thread::Builder::new()
                        .name("flowstate-shutdown-watchdog".into())
                        .spawn(move || {
                            std::thread::sleep(Duration::from_secs(10));
                            if !SHUTDOWN_COMPLETE.load(Ordering::SeqCst) {
                                tracing::warn!(
                                    "shutdown watchdog elapsed (>10s); forcing exit — \
                                     any surviving subprocesses will be reaped by \
                                     the startup orphan scan on next launch"
                                );
                                SHUTDOWN_COMPLETE.store(true, Ordering::SeqCst);
                                app_handle_for_watchdog.exit(0);
                            }
                        })
                    {
                        tracing::warn!(
                            %e,
                            "failed to spawn shutdown watchdog; a wedged daemon task \
                             will block the UI until the user force-quits"
                        );
                    }
                }
            }
        });
}
