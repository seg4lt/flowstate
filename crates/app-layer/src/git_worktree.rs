//! Pure git-shell helpers for listing / creating worktrees.
//!
//! Moved here from `apps/flowstate/src-tauri/src/lib.rs` during Phase
//! 3 of the architecture plan. They used to live alongside the Tauri
//! command wrappers, but:
//!
//! - `WorktreeProvisionerImpl` (in `orchestration_adapters.rs`) needs
//!   to call `create_git_worktree_internal` too — sharing a single
//!   implementation avoids drift.
//! - The future `flowstate-daemon` bin (Phase 6) must provision
//!   worktrees identically to the UI; keeping the helpers in a
//!   crate that has no Tauri dependency lets the daemon link them.
//!
//! The helpers are pure `std::process::Command` shells — no UI, no
//! async runtime coupling. The Tauri crate retains thin `#[tauri::
//! command]` wrappers that call into this module on a blocking
//! thread.

use std::process::Command;

use zenui_provider_api::{hide_console_window_std, path_with_extras, resolve_cli_command};

use serde::{Deserialize, Serialize};

/// Build a `Command` for `git` already configured with:
///   - the absolute path to `git` resolved through the workspace
///     binary resolver (so the user's `binaries.search_paths` and
///     platform fallbacks apply — critical on Windows where the GUI
///     launch's PATH is stripped),
///   - the console-window-hide flag on Windows,
///   - PATH augmented for the child so anything `git` itself forks
///     (ssh for `git fetch`, hooks, the configured editor, ...)
///     also sees the user's extras.
///
/// Used by every git shell-out in this module so the four call sites
/// stay in lockstep.
fn git_cmd() -> Command {
    let mut cmd = Command::new(resolve_cli_command("git"));
    hide_console_window_std(&mut cmd);
    cmd.env("PATH", path_with_extras(&[]));
    cmd
}

/// One entry in `git worktree list --porcelain`. Serialised to the
/// frontend as camelCase so JS/TS callers don't need a conversion
/// helper. Derives `Deserialize` so the eventual Phase 4 HTTP
/// handlers can echo it back round-trip without a separate DTO.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct GitWorktree {
    pub path: String,
    pub head: Option<String>,
    pub branch: Option<String>,
}

/// Resolve a path to its containing git repository root, or `None` if
/// `path` isn't inside a git repo (or `git` itself is unavailable).
/// Used by the worktree list to canonicalize paths reported under a
/// `.git/` dir (submodule gitdirs) back to the actual working tree.
pub fn resolve_git_root_sync(path: &str) -> Option<String> {
    let output = git_cmd()
        .args(["-C", path, "rev-parse", "--show-toplevel"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let root = String::from_utf8(output.stdout).ok()?.trim().to_string();
    if root.is_empty() { None } else { Some(root) }
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

/// Parse `git worktree list --porcelain` output into `Vec<GitWorktree>`.
/// Ignores `bare` records (no working tree to show) and strips
/// `refs/heads/` so the UI can render branch names directly.
pub fn list_git_worktrees_sync(path: String) -> Result<Vec<GitWorktree>, String> {
    let output = git_cmd()
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
            // Tolerate git versions that elide the trailing newline
            // separator between records.
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
    }
    flush(
        &mut worktrees,
        &mut current_path,
        &mut current_head,
        &mut current_branch,
        &mut current_bare,
    );
    Ok(worktrees)
}

/// Create a worktree and return its freshly-listed entry.
///
/// When `checkout_existing == true`, runs
/// `git -C <project_path> worktree add <worktree_path> <branch>` to
/// check out an existing branch into the new worktree. Otherwise
/// runs `git -C <project_path> worktree add -b <branch> <worktree_path>
/// <base_ref>` to create a new branch forked from `base_ref`.
///
/// Called both from the Tauri `create_git_worktree` command and from
/// the orchestration dispatcher's `WorktreeProvisionerImpl`.
pub fn create_git_worktree_internal(
    project_path: &str,
    worktree_path: &str,
    branch: &str,
    base_ref: &str,
    checkout_existing: bool,
) -> Result<GitWorktree, String> {
    if branch.trim().is_empty() {
        return Err("empty branch name".into());
    }
    if worktree_path.trim().is_empty() {
        return Err("empty worktree path".into());
    }
    let output = if checkout_existing {
        git_cmd()
            .args(["-C", project_path, "worktree", "add", worktree_path, branch])
            .output()
            .map_err(|e| format!("failed to run git: {e}"))?
    } else {
        git_cmd()
            .args([
                "-C",
                project_path,
                "worktree",
                "add",
                "-b",
                branch,
                worktree_path,
                base_ref,
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
    let effective_path =
        resolve_git_root_sync(project_path).unwrap_or_else(|| project_path.to_string());
    let all = list_git_worktrees_sync(effective_path)?;
    all.into_iter()
        .find(|w| w.path.trim_end_matches('/') == worktree_path.trim_end_matches('/'))
        .ok_or_else(|| {
            format!("worktree add succeeded but {worktree_path} not found in subsequent list")
        })
}
