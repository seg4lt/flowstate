use std::path::Path;
use std::process::Command;

use serde::Serialize;

/// Cheap "does this filesystem entry exist?" probe. Used by the
/// chat view to flip a worktree thread into read-only mode when
/// the user has removed its folder out from under flowstate — the
/// banner explains why the composer is disabled and the existing
/// archived-readonly infra is reused to enforce it.
#[tauri::command]
pub fn path_exists(path: String) -> bool {
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
pub async fn resolve_git_root(path: String) -> Option<String> {
    tauri::async_runtime::spawn_blocking(move || resolve_git_root_sync(&path))
        .await
        .ok()
        .flatten()
}

pub fn resolve_git_root_sync(path: &str) -> Option<String> {
    let output = Command::new("git")
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
pub fn resolve_worktree_path(path: &str) -> String {
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
pub async fn get_git_branch(path: String) -> Option<String> {
    tauri::async_runtime::spawn_blocking(move || get_git_branch_sync(path))
        .await
        .ok()
        .flatten()
}

pub fn get_git_branch_sync(path: String) -> Option<String> {
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

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GitBranchList {
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
pub async fn list_git_branches(path: String) -> Result<GitBranchList, String> {
    tauri::async_runtime::spawn_blocking(move || list_git_branches_sync(path))
        .await
        .map_err(|e| format!("spawn_blocking join: {e}"))?
}

pub fn list_git_branches_sync(path: String) -> Result<GitBranchList, String> {
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
            format!("git for-each-ref failed (status {:?})", output.status.code())
        } else {
            stderr
        });
    }
    let stdout = String::from_utf8(output.stdout)
        .map_err(|e| format!("git output not utf-8: {e}"))?;

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

/// Create a brand-new local branch based on the current HEAD and
/// switch to it. Separate from `git_checkout` because the call shape
/// is different (plain `checkout -b <name>`, no tracking ref) and
/// because the UI surfaces it as a distinct action — typing a branch
/// name that doesn't match any existing ref in the branch picker.
#[tauri::command]
pub fn git_create_branch(path: String, branch: String) -> Result<(), String> {
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
pub fn git_delete_branch(path: String, branch: String) -> Result<(), String> {
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
