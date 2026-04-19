use std::process::Command;

use serde::Serialize;

use super::branch::{resolve_git_root_sync, resolve_worktree_path};

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GitWorktree {
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
pub async fn list_git_worktrees(path: String) -> Result<Vec<GitWorktree>, String> {
    tauri::async_runtime::spawn_blocking(move || list_git_worktrees_sync(path))
        .await
        .map_err(|e| format!("spawn_blocking join: {e}"))?
}

pub fn list_git_worktrees_sync(path: String) -> Result<Vec<GitWorktree>, String> {
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
    let stdout = String::from_utf8(output.stdout)
        .map_err(|e| format!("git output not utf-8: {e}"))?;

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
            current_branch = Some(
                rest.strip_prefix("refs/heads/").unwrap_or(rest).to_string(),
            );
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
pub fn create_git_worktree(
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
    let effective_path = resolve_git_root_sync(&project_path)
        .unwrap_or(project_path);
    let all = list_git_worktrees_sync(effective_path)?;
    all.into_iter()
        .find(|w| {
            w.path.trim_end_matches('/') == worktree_path.trim_end_matches('/')
        })
        .ok_or_else(|| {
            format!(
                "worktree add succeeded but {worktree_path} not found in subsequent list"
            )
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
pub fn remove_git_worktree(
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
pub fn git_checkout(
    path: String,
    branch: String,
    create_track: Option<String>,
) -> Result<(), String> {
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
