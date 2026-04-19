use std::io::BufRead;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;
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

mod file_index;

mod shell_env;

mod user_config;
use user_config::{ProjectDisplay, ProjectWorktree, SessionDisplay, UserConfigStore};

mod usage;
use tokio::sync::broadcast::error::RecvError;
use usage::{
    TopSessionRow, UsageAgentPayload, UsageBucket, UsageEvent, UsageGroupBy, UsageRange,
    UsageStore, UsageSummaryPayload, UsageTimeseriesPayload,
};
use zenui_provider_api::{ProviderAdapter, RuntimeEvent};
use zenui_provider_claude_cli::ClaudeCliAdapter;
use zenui_provider_claude_sdk::ClaudeSdkAdapter;
use zenui_provider_codex::CodexAdapter;
use zenui_provider_github_copilot::GitHubCopilotAdapter;
use zenui_provider_github_copilot_cli::GitHubCopilotCliAdapter;

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

fn resolve_git_root_sync(path: &str) -> Option<String> {
    let output = Command::new("git")
        .args(["-C", path, "rev-parse", "--show-toplevel"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let root = String::from_utf8(output.stdout).ok()?.trim().to_string();
    if root.is_empty() {
        None
    } else {
        Some(root)
    }
}

/// When git reports a worktree path inside a `.git/` directory
/// (submodule gitdir), resolve it back to the actual working
/// directory. For normal paths this is a cheap no-op string check.
fn resolve_worktree_path(path: &str) -> String {
    if path.contains("/.git/") {
        if let Some(resolved) = resolve_git_root_sync(path) {
            return resolved;
        }
    }
    path.to_string()
}

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

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GitWorktree {
    path: String,
    head: Option<String>,
    branch: Option<String>,
}

/// List every worktree attached to the repo containing `path`,
/// parsed from `git worktree list --porcelain`. The porcelain format
/// is newline-delimited key/value pairs grouped into blank-line
/// separated records — one record per worktree. Each record has a
/// mandatory `worktree <path>` header, and may carry `HEAD <sha>`,
/// `branch refs/heads/<name>`, `detached`, or `bare`. We ignore
/// `bare` records (they have no working tree to show) and strip the
/// `refs/heads/` prefix so the UI can render the short branch name
/// directly.
///
/// Async wrapper dispatches the subprocess wait through
/// `spawn_blocking`. Internal callers (e.g. `create_git_worktree`)
/// use `list_git_worktrees_sync` directly.
#[tauri::command]
async fn list_git_worktrees(path: String) -> Result<Vec<GitWorktree>, String> {
    tauri::async_runtime::spawn_blocking(move || list_git_worktrees_sync(path))
        .await
        .map_err(|e| format!("spawn_blocking join: {e}"))?
}

fn list_git_worktrees_sync(path: String) -> Result<Vec<GitWorktree>, String> {
    let output = Command::new("git")
        .args(["-C", &path, "worktree", "list", "--porcelain"])
        .output()
        .map_err(|e| format!("failed to run git: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if stderr.is_empty() {
            format!(
                "git worktree list failed (status {:?})",
                output.status.code()
            )
        } else {
            stderr
        });
    }
    let stdout =
        String::from_utf8(output.stdout).map_err(|e| format!("git output not utf-8: {e}"))?;

    let mut worktrees: Vec<GitWorktree> = Vec::new();
    let mut current_path: Option<String> = None;
    let mut current_head: Option<String> = None;
    let mut current_branch: Option<String> = None;
    let mut current_bare = false;

    let flush = |worktrees: &mut Vec<GitWorktree>,
                 path: &mut Option<String>,
                 head: &mut Option<String>,
                 branch: &mut Option<String>,
                 bare: &mut bool| {
        if let Some(p) = path.take() {
            if !*bare {
                worktrees.push(GitWorktree {
                    path: p,
                    head: head.take(),
                    branch: branch.take(),
                });
            } else {
                head.take();
                branch.take();
            }
        }
        *bare = false;
    };

    for line in stdout.lines() {
        if line.is_empty() {
            flush(
                &mut worktrees,
                &mut current_path,
                &mut current_head,
                &mut current_branch,
                &mut current_bare,
            );
            continue;
        }
        if let Some(rest) = line.strip_prefix("worktree ") {
            // A new record started without a blank separator — flush
            // whatever we accumulated to stay tolerant of git versions
            // that elide the trailing newline.
            flush(
                &mut worktrees,
                &mut current_path,
                &mut current_head,
                &mut current_branch,
                &mut current_bare,
            );
            current_path = Some(resolve_worktree_path(rest));
        } else if let Some(rest) = line.strip_prefix("HEAD ") {
            current_head = Some(rest.to_string());
        } else if let Some(rest) = line.strip_prefix("branch ") {
            current_branch = Some(rest.strip_prefix("refs/heads/").unwrap_or(rest).to_string());
        } else if line == "bare" {
            current_bare = true;
        }
        // "detached" and other single-word markers are ignored; a
        // detached worktree just ends up with branch = None.
    }
    // Flush the final record (git's porcelain output may or may not
    // end with a trailing blank line).
    flush(
        &mut worktrees,
        &mut current_path,
        &mut current_head,
        &mut current_branch,
        &mut current_bare,
    );

    Ok(worktrees)
}

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
    if branch.trim().is_empty() {
        return Err("empty branch name".into());
    }
    if worktree_path.trim().is_empty() {
        return Err("empty worktree path".into());
    }
    let output = if checkout_existing.unwrap_or(false) {
        // Check out an existing branch into the new worktree.
        Command::new("git")
            .args([
                "-C",
                &project_path,
                "worktree",
                "add",
                &worktree_path,
                &branch,
            ])
            .output()
            .map_err(|e| format!("failed to run git: {e}"))?
    } else {
        // Create a new branch and check it out into the new worktree.
        Command::new("git")
            .args([
                "-C",
                &project_path,
                "worktree",
                "add",
                "-b",
                &branch,
                &worktree_path,
                &base_ref,
            ])
            .output()
            .map_err(|e| format!("failed to run git: {e}"))?
    };
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        return Err(if !stderr.is_empty() {
            stderr
        } else if !stdout.is_empty() {
            stdout
        } else {
            format!(
                "git worktree add exited with status {:?}",
                output.status.code()
            )
        });
    }

    // Re-read the list so the caller gets the new entry with the
    // canonical fields the porcelain parser produces (in particular
    // the HEAD sha, which we don't otherwise have on the create
    // side). Linear scan — the list is short. Call the sync helper
    // directly so we don't need to make this command async just to
    // chain an `.await`.
    //
    // Resolve the git root first — when the project path is a
    // submodule directory git may report worktree paths relative to
    // the resolved repo root rather than the raw project path.
    let effective_path = resolve_git_root_sync(&project_path).unwrap_or(project_path);
    let all = list_git_worktrees_sync(effective_path)?;
    all.into_iter()
        .find(|w| w.path.trim_end_matches('/') == worktree_path.trim_end_matches('/'))
        .ok_or_else(|| {
            format!("worktree add succeeded but {worktree_path} not found in subsequent list")
        })
}

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

/// Maximum file size we'll inline into the code view. The editor
/// uses @pierre/diffs' Virtualizer so a 10k-line plain-text file is
/// fine, but anything past this is probably generated / binary /
/// not useful to read inline and we return a placeholder marker.
const CODE_VIEW_MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;

/// List every file in the worktree at `path` that the index knows
/// about. Delegates to the per-worktree `FilePicker` in
/// [`FileIndexRegistry`] — the first call for a given worktree pays
/// the cost of a background scan + fs watcher spin-up; subsequent
/// calls return live-updated results for free.
///
/// Returns relative forward-slash paths, alphabetically sorted,
/// capped at [`file_index::PROJECT_FILE_LIST_MAX`].
#[tauri::command]
fn list_project_files(
    registry: State<'_, file_index::FileIndexRegistry>,
    path: String,
) -> Vec<String> {
    file_index::list_project_files(&registry, &path)
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

// Content-search wire types and helpers live in `file_index` — see
// the module for `ContentSearchOptions`, `ContentBlock`, `BlockLine`,
// and the `grep_search`-based engine that replaces the old
// `grep_searcher::Sink` implementation.

/// Live content search across the current worktree, delegated to
/// `fff-search` via [`file_index::search_file_contents`]. Returns one
/// `ContentBlock` per disjoint match group per file, matching the
/// same wire shape the old ripgrep-backed implementation produced.
///
/// Cancellation: if `token` is supplied, the command registers an
/// `AtomicBool` in [`SearchTasks`] so the frontend can abort an
/// in-flight search by calling `stop_content_search(token)`.
#[tauri::command]
fn search_file_contents(
    registry: State<'_, file_index::FileIndexRegistry>,
    search_tasks: State<'_, file_index::SearchTasks>,
    path: String,
    query: String,
    options: file_index::ContentSearchOptions,
    token: Option<u64>,
) -> Result<Vec<file_index::ContentBlock>, String> {
    let cancel = token.map(|t| search_tasks.register(t));
    let result =
        file_index::search_file_contents(&registry, &path, &query, &options, cancel.as_deref());
    if let Some(t) = token {
        search_tasks.unregister(t);
    }
    result
}

/// Cancel an in-flight `search_file_contents` call by `token`.
/// Mirrors `stop_git_diff_summary` for the diff-streaming subsystem.
#[tauri::command]
fn stop_content_search(tasks: State<'_, file_index::SearchTasks>, token: u64) {
    tasks.cancel(token);
}

struct AppLifecycle {
    lifecycle: Arc<DaemonLifecycle>,
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
    // ships upstream or we patch the crate. See file_index.rs.
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

#[tauri::command]
fn get_user_config(
    store: State<'_, UserConfigStore>,
    key: String,
) -> Result<Option<String>, String> {
    store.get(&key)
}

#[tauri::command]
fn set_user_config(
    store: State<'_, UserConfigStore>,
    key: String,
    value: String,
) -> Result<(), String> {
    store.set(&key, &value)
}

// Per-session and per-project display metadata: titles, names,
// previews, ordering. Lives in the same `user_config.sqlite`
// file as the kv table above, in dedicated tables. The agent
// SDK no longer persists any of this — its persistence layer
// only stores fields the runtime needs to execute or resume
// agents. See `rs-agent-sdk/crates/core/persistence/CLAUDE.md`
// for the boundary.

#[tauri::command]
fn set_session_display(
    store: State<'_, UserConfigStore>,
    session_id: String,
    display: SessionDisplay,
) -> Result<(), String> {
    store.set_session_display(&session_id, &display)
}

#[tauri::command]
fn get_session_display(
    store: State<'_, UserConfigStore>,
    session_id: String,
) -> Result<Option<SessionDisplay>, String> {
    store.get_session_display(&session_id)
}

#[tauri::command]
fn list_session_display(
    store: State<'_, UserConfigStore>,
) -> Result<HashMap<String, SessionDisplay>, String> {
    store.list_session_display()
}

#[tauri::command]
fn delete_session_display(
    store: State<'_, UserConfigStore>,
    session_id: String,
) -> Result<(), String> {
    store.delete_session_display(&session_id)
}

#[tauri::command]
fn set_project_display(
    store: State<'_, UserConfigStore>,
    project_id: String,
    display: ProjectDisplay,
) -> Result<(), String> {
    store.set_project_display(&project_id, &display)
}

#[tauri::command]
fn get_project_display(
    store: State<'_, UserConfigStore>,
    project_id: String,
) -> Result<Option<ProjectDisplay>, String> {
    store.get_project_display(&project_id)
}

#[tauri::command]
fn list_project_display(
    store: State<'_, UserConfigStore>,
) -> Result<HashMap<String, ProjectDisplay>, String> {
    store.list_project_display()
}

#[tauri::command]
fn delete_project_display(
    store: State<'_, UserConfigStore>,
    project_id: String,
) -> Result<(), String> {
    store.delete_project_display(&project_id)
}

// Parent/child worktree links — a flowstate-app concept, not an SDK
// concept. Each worktree has its own SDK project (so the SDK's
// existing cwd resolution picks up the worktree folder), and this
// table just records "project X is a worktree of project Y, on
// branch Z". The sidebar uses these links to group worktree threads
// under the parent project visually.

#[tauri::command]
fn set_project_worktree(
    store: State<'_, UserConfigStore>,
    project_id: String,
    parent_project_id: String,
    branch: Option<String>,
) -> Result<(), String> {
    store.set_project_worktree(&project_id, &parent_project_id, branch.as_deref())
}

#[tauri::command]
fn get_project_worktree(
    store: State<'_, UserConfigStore>,
    project_id: String,
) -> Result<Option<ProjectWorktree>, String> {
    store.get_project_worktree(&project_id)
}

#[tauri::command]
fn list_project_worktree(
    store: State<'_, UserConfigStore>,
) -> Result<HashMap<String, ProjectWorktree>, String> {
    store.list_project_worktree()
}

#[tauri::command]
fn delete_project_worktree(
    store: State<'_, UserConfigStore>,
    project_id: String,
) -> Result<(), String> {
    store.delete_project_worktree(&project_id)
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
fn get_usage_summary(
    store: State<'_, UsageStore>,
    range: UsageRange,
    group_by: Option<UsageGroupBy>,
) -> Result<UsageSummaryPayload, String> {
    store.summary(range, group_by.unwrap_or_default())
}

#[tauri::command]
fn get_usage_timeseries(
    store: State<'_, UsageStore>,
    range: UsageRange,
    bucket: UsageBucket,
    split_by: Option<UsageGroupBy>,
) -> Result<UsageTimeseriesPayload, String> {
    store.timeseries(range, bucket, split_by)
}

#[tauri::command]
fn get_top_sessions(
    store: State<'_, UsageStore>,
    range: UsageRange,
    limit: Option<u32>,
) -> Result<Vec<TopSessionRow>, String> {
    store.top_sessions(range, limit.unwrap_or(10))
}

/// Per-agent dashboard breakdown: returns one row per (agent_type)
/// aggregated over the range, with the synthetic "main" key for the
/// parent agent. Cost is pre-allocated at insert time, so this is a
/// plain GROUP BY against `usage_event_agents` — cheap even on a
/// couple hundred thousand rows.
#[tauri::command]
fn get_usage_by_agent(
    store: State<'_, UsageStore>,
    range: UsageRange,
) -> Result<UsageAgentPayload, String> {
    store.summary_by_agent(range)
}

/// Two-row rollup of `usage_event_agents`: one row for the main
/// (parent) agent, one row aggregating every subagent invocation.
/// Same payload shape as `get_usage_by_agent` but the SQL collapses
/// all non-NULL `agent_type` values into a single `"subagent"` key.
#[tauri::command]
fn get_usage_by_agent_role(
    store: State<'_, UsageStore>,
    range: UsageRange,
) -> Result<UsageAgentPayload, String> {
    store.summary_by_agent_role(range)
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
        .manage(file_index::FileIndexRegistry::default())
        .manage(file_index::SearchTasks::default())
        .setup(|app| {
            let app_handle = app.handle().clone();

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

                let core = bootstrap_core_async(&config)
                    .await
                    .expect("daemon bootstrap failed");

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

            // Block until serve() is done and TauriDaemonState is managed.
            let lifecycle = ready_rx.recv().expect("daemon failed to start");
            app.manage(AppLifecycle { lifecycle });

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
            read_project_file,
            search_file_contents,
            stop_content_search,
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
