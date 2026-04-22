use std::io::BufRead;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tauri::ipc::Channel;
use tauri::Manager;
use tauri::State;
use tracing_subscriber::EnvFilter;
use transport_tauri::TauriTransport;
use zenui_daemon_core::{
    bootstrap_core_async, graceful_shutdown, transport_tauri, DaemonConfig, DaemonLifecycle,
    Transport,
};
use zenui_runtime_core::ConnectionObserver;

mod pty;
use pty::{PtyId, PtyManager};

mod shell_env;

mod daemon_client;
mod daemon_supervisor;
mod loopback_http;
mod orphan_scan;
use daemon_client::{DaemonBaseUrl, DaemonClient};
// Phase 3 — user_config / usage / orchestration_adapters / git_worktree
// moved into `flowstate-app-layer` so the future daemon bin can link
// them without pulling Tauri. The Tauri crate now depends on the
// app-layer crate instead of mod-ing the files in-tree.
use flowstate_app_layer::git_worktree::{
    GitWorktree, create_git_worktree_internal, list_git_worktrees_sync, resolve_git_root_sync,
};
use flowstate_app_layer::orchestration_adapters::{
    AppMetadataProviderImpl, WorktreeProvisionerImpl,
};
use flowstate_app_layer::user_config::{
    ProjectDisplay, ProjectWorktree, SessionDisplay, UserConfigStore,
};
use flowstate_app_layer::usage::{
    TopSessionRow, UsageAgentPayload, UsageBucket, UsageEvent, UsageGroupBy, UsageRange,
    UsageStore, UsageSummaryPayload, UsageTimeseriesPayload,
};
use tokio::sync::broadcast::error::RecvError;
use zenui_provider_api::{OrchestrationIpcHandle, ProviderAdapter, RuntimeEvent};
use zenui_provider_claude_cli::ClaudeCliAdapter;
use zenui_provider_claude_sdk::ClaudeSdkAdapter;
use zenui_provider_codex::CodexAdapter;
use zenui_provider_github_copilot::GitHubCopilotAdapter;
use zenui_provider_github_copilot_cli::GitHubCopilotCliAdapter;
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

fn get_git_branch_sync(path: String) -> Option<String> {
    let output = Command::new("git")
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
    let output = Command::new("git")
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
    let output = Command::new("git")
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
    let output = Command::new("git")
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
    let mut cmd = Command::new("git");
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
    let mut cmd = Command::new("git");
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
    let Ok(output) = Command::new("git")
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
    let Ok(output) = Command::new("git")
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
    let Ok(output) = Command::new("git")
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
    let child = Command::new("git")
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

/// Cap the picker list so we never send a million entries to the
/// frontend for a huge repo. 20k is more than enough for a Cmd+P
/// picker.
const PROJECT_FILE_LIST_MAX: usize = 20_000;

/// Maximum file size we'll inline into the code view. The editor
/// uses @pierre/diffs' Virtualizer so a 10k-line plain-text file is
/// fine, but anything past this is probably generated / binary /
/// not useful to read inline and we return a placeholder marker.
const CODE_VIEW_MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;

/// List every file in `path` that isn't ignored by .gitignore,
/// .ignore, etc. Respects hidden-file convention (skips dotfiles)
/// and avoids the usual suspects (node_modules, target, dist, …)
/// via `ignore::WalkBuilder`'s standard filters. Returns relative
/// paths (forward-slash), sorted, capped at PROJECT_FILE_LIST_MAX.
///
/// Uses `WalkBuilder::build_parallel` so the gitignore walk fans
/// out across CPU cores. On a multi-core machine with SSD this is
/// 2-4x faster than the serial walker for large repos, which is
/// the dominant cost on a cold open of the /code view's picker.
#[tauri::command]
fn list_project_files(path: String) -> Vec<String> {
    use ignore::WalkState;
    use std::sync::Mutex;

    let project_path = Path::new(&path);
    if !project_path.is_dir() {
        return Vec::new();
    }

    // Match `git status` visibility: honor .gitignore (local, global,
    // and .git/info/exclude), but don't silently drop dotfolders or
    // `.ignore` files the way ripgrep does by default.
    let entries: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let project_path_owned = project_path.to_path_buf();

    let thread_count = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);

    ignore::WalkBuilder::new(project_path)
        .hidden(false)
        .ignore(false)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .parents(true)
        .require_git(false)
        .threads(thread_count)
        .build_parallel()
        .run(|| {
            let entries = Arc::clone(&entries);
            let project_path = project_path_owned.clone();
            Box::new(move |result| {
                let Ok(entry) = result else {
                    return WalkState::Continue;
                };
                // Only files — directories get walked into automatically.
                if !entry.file_type().is_some_and(|t| t.is_file()) {
                    return WalkState::Continue;
                }
                let abs = entry.path();
                let Ok(rel) = abs.strip_prefix(&project_path) else {
                    return WalkState::Continue;
                };
                // Forward-slash path, platform-normalised, so the
                // frontend can pattern-match without caring about
                // Windows back-slashes.
                let rel_str = rel
                    .components()
                    .map(|c| c.as_os_str().to_string_lossy())
                    .collect::<Vec<_>>()
                    .join("/");
                if rel_str.is_empty() {
                    return WalkState::Continue;
                }
                let mut guard = match entries.lock() {
                    Ok(g) => g,
                    Err(poisoned) => poisoned.into_inner(),
                };
                if guard.len() >= PROJECT_FILE_LIST_MAX {
                    return WalkState::Quit;
                }
                guard.push(rel_str);
                WalkState::Continue
            })
        });

    let mut entries = Arc::try_unwrap(entries)
        .ok()
        .and_then(|m| m.into_inner().ok())
        .unwrap_or_default();
    entries.sort();
    entries
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
    let iter =
        std::fs::read_dir(&target_canon).map_err(|e| format!("read_dir: {e}"))?;
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
    let mut child = Command::new(trimmed)
        .arg(".")
        .current_dir(project_path)
        .spawn()
        .map_err(|e| format!("failed to launch `{trimmed}`: {e}"))?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

/// One line inside a `ContentBlock`. `is_match` distinguishes the
/// matching line(s) from the surrounding context lines so the
/// frontend can highlight them.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct BlockLine {
    line: u64,
    text: String,
    is_match: bool,
}

/// A contiguous run of lines from one file that contains at least
/// one match plus its surrounding context. Matches close together
/// in the same file share a single block (`grep_searcher` issues
/// a `context_break` between disjoint groups, which we use as the
/// block boundary).
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ContentBlock {
    path: String,
    /// 1-based line number of the first line in `lines` — handy
    /// for the frontend gutter even though every line carries its
    /// own `line` field too.
    start_line: u64,
    lines: Vec<BlockLine>,
}

/// How many context lines to capture on each side of every match.
/// Three is the Zed multibuffer default and it's what the picker
/// header explains as "top 3 / bottom 3".
const CONTENT_SEARCH_CONTEXT_LINES: usize = 3;

/// Soft cap on total lines streamed across all blocks for one
/// query. Bounds the IPC payload so a pathological query (`a`)
/// can't ship megabytes through the bridge. Each line averages
/// around 60–80 chars, so 3000 lines ≈ ~200 KB of JSON.
const CONTENT_SEARCH_MAX_TOTAL_LINES: usize = 3_000;

/// Per-line truncation. Long lines (minified bundles, lockfiles)
/// get clipped + ellipsised so a single 100k-char line can't blow
/// up the payload either.
const CONTENT_SEARCH_MAX_LINE_LEN: usize = 240;

/// Custom `grep_searcher::Sink` that builds `ContentBlock`s as it
/// receives lines from the searcher. The default `sinks::UTF8`
/// only forwards match lines; we need both match AND context, so
/// we implement Sink ourselves and use `context_break` events to
/// separate disjoint match groups within a file.
struct BlockSink {
    rel_path: String,
    finished_blocks: Vec<ContentBlock>,
    current: Option<ContentBlock>,
    /// Shared budget across all files for one query. Decremented
    /// on every line we accept; once it hits zero we tell the
    /// searcher to stop by returning `Ok(false)`.
    line_budget_remaining: usize,
}

impl BlockSink {
    fn new(rel_path: String, line_budget_remaining: usize) -> Self {
        Self {
            rel_path,
            finished_blocks: Vec::new(),
            current: None,
            line_budget_remaining,
        }
    }

    fn push_line(&mut self, line_number: u64, text: String, is_match: bool) {
        if self.current.is_none() {
            self.current = Some(ContentBlock {
                path: self.rel_path.clone(),
                start_line: line_number,
                lines: Vec::new(),
            });
        }
        if let Some(block) = self.current.as_mut() {
            block.lines.push(BlockLine {
                line: line_number,
                text,
                is_match,
            });
            self.line_budget_remaining = self.line_budget_remaining.saturating_sub(1);
        }
    }

    fn flush_current(&mut self) {
        if let Some(block) = self.current.take() {
            // Only keep blocks that actually contain at least one
            // match. A pure-context block (no matched lines) means
            // the searcher emitted before/after context for a match
            // we already accounted for in a previous block — skip.
            if block.lines.iter().any(|l| l.is_match) {
                self.finished_blocks.push(block);
            }
        }
    }
}

fn truncate_line(raw: &[u8]) -> String {
    let text = std::str::from_utf8(raw).unwrap_or("");
    let text = text.trim_end_matches(['\n', '\r']);
    if text.len() > CONTENT_SEARCH_MAX_LINE_LEN {
        // Find a char boundary at or before the cap so we don't
        // split a multi-byte character mid-codepoint.
        let mut cut = CONTENT_SEARCH_MAX_LINE_LEN;
        while cut > 0 && !text.is_char_boundary(cut) {
            cut -= 1;
        }
        let mut t = text[..cut].to_string();
        t.push('…');
        t
    } else {
        text.to_string()
    }
}

impl grep_searcher::Sink for BlockSink {
    type Error = std::io::Error;

    fn matched(
        &mut self,
        _searcher: &grep_searcher::Searcher,
        mat: &grep_searcher::SinkMatch<'_>,
    ) -> Result<bool, Self::Error> {
        if self.line_budget_remaining == 0 {
            return Ok(false);
        }
        let line_number = mat.line_number().unwrap_or(0);
        let text = truncate_line(mat.bytes());
        self.push_line(line_number, text, true);
        Ok(self.line_budget_remaining > 0)
    }

    fn context(
        &mut self,
        _searcher: &grep_searcher::Searcher,
        ctx: &grep_searcher::SinkContext<'_>,
    ) -> Result<bool, Self::Error> {
        if self.line_budget_remaining == 0 {
            return Ok(false);
        }
        let line_number = ctx.line_number().unwrap_or(0);
        let text = truncate_line(ctx.bytes());
        self.push_line(line_number, text, false);
        Ok(self.line_budget_remaining > 0)
    }

    fn context_break(
        &mut self,
        _searcher: &grep_searcher::Searcher,
    ) -> Result<bool, Self::Error> {
        self.flush_current();
        Ok(self.line_budget_remaining > 0)
    }

    fn finish(
        &mut self,
        _searcher: &grep_searcher::Searcher,
        _finish: &grep_searcher::SinkFinish,
    ) -> Result<(), Self::Error> {
        self.flush_current();
        Ok(())
    }
}

/// Per-search options sent from the frontend's advanced controls.
/// Defaults intentionally match "boring literal case-sensitive
/// search with no path filtering" so omitting the field on the
/// frontend behaves like the old two-arg command.
#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct ContentSearchOptions {
    /// When true the query is treated as a `regex` crate regex,
    /// matching ripgrep's default dialect. When false the query
    /// is passed to grep-regex with `.fixed_strings(true)` so
    /// users can paste raw code fragments without escaping.
    #[serde(default)]
    use_regex: bool,
    /// Default true (matches the user's expectation that "Foo"
    /// doesn't match "foo" out of the box). The `aA` toggle in
    /// the UI flips this off.
    #[serde(default = "default_true")]
    case_sensitive: bool,
    /// Glob patterns to RESTRICT the walk to (ripgrep
    /// OverrideBuilder includes). Empty list means "everywhere".
    #[serde(default)]
    includes: Vec<String>,
    /// Glob patterns to EXCLUDE from the walk. The frontend sends
    /// plain globs; we prefix them with `!` for OverrideBuilder
    /// since that's the convention it expects.
    #[serde(default)]
    excludes: Vec<String>,
}

fn default_true() -> bool {
    true
}

/// Live content search across the project, ripgrep-style. Walks
/// the same gitignore-aware tree as `list_project_files` (with
/// optional include/exclude glob overrides) and runs each file
/// through ripgrep's own `Searcher`. The query is literal by
/// default (`fixed_strings(true)`) so users can paste raw code
/// fragments like `fn foo(` or `->` without escaping; flipping
/// `useRegex` switches into the full `regex` crate dialect.
/// `caseSensitive` defaults to true; flip it off for an
/// `aA`-style insensitive search.
///
/// Returns one `ContentBlock` per disjoint match group per file:
/// each block is the match line(s) plus 3 lines of context on
/// either side. The frontend renders these as Zed-style
/// multibuffer chunks.
#[tauri::command]
fn search_file_contents(
    path: String,
    query: String,
    options: ContentSearchOptions,
) -> Result<Vec<ContentBlock>, String> {
    use grep_regex::RegexMatcherBuilder;
    use grep_searcher::SearcherBuilder;
    use ignore::overrides::OverrideBuilder;

    let trimmed = query.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    let project_path = Path::new(&path);
    if !project_path.is_dir() {
        return Ok(Vec::new());
    }

    let matcher = RegexMatcherBuilder::new()
        .case_insensitive(!options.case_sensitive)
        .fixed_strings(!options.use_regex)
        .build(trimmed)
        .map_err(|e| format!("regex build: {e}"))?;

    let mut searcher = SearcherBuilder::new()
        .line_number(true)
        .before_context(CONTENT_SEARCH_CONTEXT_LINES)
        .after_context(CONTENT_SEARCH_CONTEXT_LINES)
        .build();

    // Build glob overrides from include/exclude lists, if any.
    // OverrideBuilder treats a leading `!` as "exclude" and bare
    // patterns as "include" — we hide that detail from users and
    // let them type plain globs in the exclude box.
    let overrides = if !options.includes.is_empty() || !options.excludes.is_empty() {
        let mut ob = OverrideBuilder::new(project_path);
        for inc in &options.includes {
            let trimmed = inc.trim();
            if trimmed.is_empty() {
                continue;
            }
            ob.add(trimmed)
                .map_err(|e| format!("include glob `{trimmed}`: {e}"))?;
        }
        for exc in &options.excludes {
            let trimmed = exc.trim();
            if trimmed.is_empty() {
                continue;
            }
            let pat = if trimmed.starts_with('!') {
                trimmed.to_string()
            } else {
                format!("!{trimmed}")
            };
            ob.add(&pat)
                .map_err(|e| format!("exclude glob `{trimmed}`: {e}"))?;
        }
        Some(ob.build().map_err(|e| format!("override build: {e}"))?)
    } else {
        None
    };

    let mut wb = ignore::WalkBuilder::new(project_path);
    wb.hidden(false)
        .ignore(false)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .parents(true)
        .require_git(false);
    if let Some(ov) = overrides {
        wb.overrides(ov);
    }

    let mut all_blocks: Vec<ContentBlock> = Vec::new();
    let mut lines_remaining = CONTENT_SEARCH_MAX_TOTAL_LINES;

    'walk: for result in wb.build() {
        if lines_remaining == 0 {
            break;
        }
        let Ok(entry) = result else { continue };
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let abs = entry.path();
        let Ok(rel) = abs.strip_prefix(project_path) else {
            continue;
        };
        let rel_str = rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        if rel_str.is_empty() {
            continue;
        }

        let mut sink = BlockSink::new(rel_str, lines_remaining);
        let _ = searcher.search_path(&matcher, abs, &mut sink);
        // The sink decrements its own budget; carry the remainder
        // into the next file's sink so the global cap holds.
        lines_remaining = sink.line_budget_remaining;
        all_blocks.append(&mut sink.finished_blocks);

        if lines_remaining == 0 {
            break 'walk;
        }
    }

    Ok(all_blocks)
}

struct AppLifecycle {
    lifecycle: Arc<DaemonLifecycle>,
}

/// Initialize tracing. Debug builds stream to stderr so `cargo tauri dev`
/// surfaces logs in the terminal; release builds keep writing to a log
/// file alongside the daemon log.
fn init_tracing() {
    let env_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("flowstate=info,zenui=info,warn"));

    if cfg!(debug_assertions) {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(env_filter)
            .with_writer(std::io::stderr)
            .try_init();
        eprintln!("flowstate: dev build, logging to stderr");
        return;
    }

    let log_dir = std::env::temp_dir().join("flowstate").join("logs");
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
    on_data: Channel<Vec<u8>>,
) -> Result<PtyId, String> {
    manager.open(cols, rows, cwd, shell, on_data)
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
    base_url.client().set_session_display(session_id, display).await
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
    base_url.client().set_project_display(project_id, display).await
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
        .manage(PtyManager::new())
        .manage(DiffTasks::default())
        .setup(|app| {
            let app_handle = app.handle().clone();

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
            // Keep a clone for the orchestration adapters below —
            // `app.manage` takes ownership but UserConfigStore is
            // cheap to clone (Arc<Mutex<Connection>> inside).
            let user_config_for_orch = user_config_store.clone();
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

            // Phase 5 — shared channel for the loopback HTTP base URL.
            // Every `#[tauri::command]` in the app-layer block reads
            // its receiver via `DaemonClient`; `loopback_http::spawn`
            // publishes once the transport has bound.
            let daemon_base_url = DaemonBaseUrl::new();
            let daemon_base_url_for_spawn = daemon_base_url.clone();
            // Register the channel as Tauri state so the command
            // handlers can obtain a `DaemonClient` per call.
            app.manage(daemon_base_url.clone());

            // Phase 6 — when `FLOWSTATE_USE_DAEMON=1` is set, spawn
            // the daemon as a separate process via the supervisor
            // and skip the embedded bootstrap below. The supervisor
            // reads the handshake file and publishes the base URL
            // to `DaemonBaseUrl` so every app-layer Tauri command
            // (already on `DaemonClient`) transparently routes to
            // the daemon. The embedded path (below) stays as the
            // default until the WS relay for the runtime-forwarder
            // commands (`connect` / `handle_message`) lands —
            // tracked in the plan's remaining Phase 6 items.
            let use_daemon_split = std::env::var("FLOWSTATE_USE_DAEMON")
                .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                .unwrap_or(false);
            if use_daemon_split {
                let data_dir = flowstate_root.clone();
                let base_url_for_supervisor = daemon_base_url.clone();
                let exe_path = std::env::current_exe()
                    .expect("resolve current_exe for daemon supervisor");
                tauri::async_runtime::spawn(async move {
                    let cfg = daemon_supervisor::SupervisorConfig::defaults(
                        data_dir,
                        exe_path,
                    );
                    match daemon_supervisor::spawn(cfg, base_url_for_supervisor).await {
                        Ok(_sup) => {
                            tracing::info!("daemon supervisor started");
                            // Drop the returned supervisor — its
                            // background task retains ownership of
                            // the Child via kill_on_drop, and the
                            // broadcast channel lives in the
                            // runtime's task graph. The shell's
                            // SIGTERM handler (signal handler
                            // elsewhere in this setup) calls
                            // `lifecycle.request_shutdown()` which
                            // also drops the supervisor reference
                            // and tears down the child.
                        }
                        Err(e) => {
                            tracing::error!(%e, "daemon supervisor failed to start");
                        }
                    }
                });
                // With the daemon separate, we don't run the
                // embedded bootstrap below — return early. The 2
                // `connect`/`handle_message` Tauri commands still
                // touch TauriTransport which won't be mounted;
                // those specific commands will error until the WS
                // relay lands. Callers who need the full UI today
                // run without `FLOWSTATE_USE_DAEMON=1`.
                return Ok(());
            }

            // Run the daemon on Tauri's existing tokio runtime so the
            // process has exactly one thread pool. The previous shape
            // (std::thread::spawn + bootstrap_core's own runtime) was
            // a workaround for "cannot start a runtime from within a
            // runtime"; bootstrap_core_async removes that need by
            // letting us share the host runtime.
            tauri::async_runtime::spawn(async move {
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
                config.adapters = vec![
                    Arc::new(ClaudeSdkAdapter::new(flowstate_root.clone()))
                        as Arc<dyn ProviderAdapter>,
                    Arc::new(ClaudeCliAdapter::new_with_orchestration(
                        flowstate_root.clone(),
                        Some(ipc_handle.clone()),
                    )),
                    Arc::new(CodexAdapter::new_with_orchestration(
                        flowstate_root.clone(),
                        Some(ipc_handle.clone()),
                    )),
                    Arc::new(GitHubCopilotAdapter::new_with_orchestration(
                        flowstate_root.clone(),
                        Some(ipc_handle.clone()),
                    )),
                    Arc::new(GitHubCopilotCliAdapter::new_with_orchestration(
                        flowstate_root.clone(),
                        Some(ipc_handle.clone()),
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
                    Arc::new(OpenCodeAdapter::new_with_orchestration(
                        flowstate_root.clone(),
                        Some(ipc_handle.clone()),
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

            // Block until serve() is done and TauriDaemonState is managed.
            let lifecycle = ready_rx.recv().expect("daemon failed to start");
            app.manage(AppLifecycle {
                lifecycle: lifecycle.clone(),
            });

            // SIGTERM / SIGINT handler.
            //
            // Why this exists: without it, SIGTERM (e.g. `tauri dev`
            // hot-reload, `pkill`, systemd stop, macOS Activity Monitor
            // "Quit") terminates flowstate without running any Drop
            // code — `opencode serve` and its grandchildren (including
            // the `flowstate mcp-server` proxies) orphan to PID 1 and
            // keep running, pointing at the now-dead loopback port.
            // Users observed ~22 such orphans accumulating during a
            // normal dev session.
            //
            // This handler intercepts both signals and walks the
            // existing graceful-shutdown path:
            //   1. `lifecycle.request_shutdown()` tips the daemon task
            //      out of its `wait_for_shutdown()` await.
            //   2. Daemon proceeds to `graceful_shutdown()` → drops
            //      `DaemonConfig::adapters` → drops `OpenCodeAdapter`
            //      → drops `OpenCodeServer` → its Drop impl sends
            //      `killpg(pgid, SIGTERM)` to the whole opencode
            //      process group (opencode + its mcp-server kids).
            //   3. `app_handle.exit(0)` breaks the Tauri event loop so
            //      the process actually exits instead of hanging.
            //
            // SIGKILL is still uncatchable — the startup orphan scan
            // (see `loopback_http::spawn` / orphan-scan helper)
            // handles that case. This handler covers every signal the
            // kernel lets us touch.
            {
                let app_handle = app.handle().clone();
                let lifecycle_for_signal = lifecycle.clone();
                tauri::async_runtime::spawn(async move {
                    let mut sigterm = match tokio::signal::unix::signal(
                        tokio::signal::unix::SignalKind::terminate(),
                    ) {
                        Ok(s) => s,
                        Err(err) => {
                            tracing::warn!(
                                %err,
                                "failed to install SIGTERM handler; graceful shutdown on \
                                 external signals will be unavailable"
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
                    let signal_name = tokio::select! {
                        _ = sigterm.recv() => "SIGTERM",
                        _ = sigint.recv() => "SIGINT",
                    };
                    tracing::info!(
                        signal = signal_name,
                        "received termination signal; requesting daemon shutdown"
                    );
                    lifecycle_for_signal.request_shutdown();
                    // Tauri's `exit()` runs `RunEvent::Exit` handlers
                    // and returns control from `.run()`. Combined with
                    // `request_shutdown()` above, the Drop chain runs
                    // (killing opencode via process group) before the
                    // process actually exits.
                    app_handle.exit(0);
                });
            }

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
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
            get_git_diff_summary,
            get_git_diff_file,
            watch_git_diff_summary,
            stop_git_diff_summary,
            list_project_files,
            list_directory,
            read_project_file,
            search_file_contents,
            open_in_editor,
            pty_open,
            pty_write,
            pty_resize,
            pty_pause,
            pty_resume,
            pty_kill,
            get_user_config,
            set_user_config,
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
