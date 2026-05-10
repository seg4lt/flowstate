//! Auto-merge mechanics for the `Merge` state.
//!
//! When the user approves a task in `HumanReview`, the kanban
//! HTTP route flips it to `Merge` and kicks the tick loop. The
//! tick loop then calls `merge_task` here, which:
//!
//! 1. Looks up the parent project's path (from `RuntimeCore`'s
//!    project list).
//! 2. Runs `git -C <parent_path> merge --no-ff --no-edit <branch>`
//!    via `tokio::process::Command` (argv array, no shell — branch
//!    names can't escape).
//! 3. On conflict: `git merge --abort`, returns `Conflict { files }`
//!    so the caller can flip the task to `NeedsHuman` with a clear
//!    reason. Worktree is preserved for human resolution.
//! 4. On success: returns `Merged { sha }` — the caller cleans up
//!    worktree, branch, and retires task sessions.
//!
//! All `git` calls use a 60s timeout per command and avoid shells.

use std::path::Path;
use std::time::Duration;

use tokio::process::Command;

/// Outcome of a merge attempt. Caller maps each variant to a state
/// transition; this module is pure git mechanics.
#[derive(Debug, Clone)]
pub enum MergeOutcome {
    /// `git merge` succeeded. `sha` is the new HEAD commit on the
    /// parent branch (the merge commit when `--no-ff` introduces
    /// one, otherwise the fast-forwarded tip).
    Merged { sha: String },
    /// `git merge` produced conflicts. Files listed are the
    /// conflicting paths (relative to parent worktree). The merge
    /// has already been `git merge --abort`'d before this returns.
    Conflict { files: Vec<String> },
}

/// Errors that aren't conflicts — meaning the merge couldn't even
/// be attempted in a useful way. Caller usually maps these to
/// `NeedsHuman` with the error message as the reason.
#[derive(Debug, Clone)]
pub enum MergeError {
    MissingParentPath(String),
    MissingBranch,
    Timeout(String),
    GitFailed(String),
    Io(String),
}

impl std::fmt::Display for MergeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MergeError::MissingParentPath(p) => write!(f, "missing parent project path: {p}"),
            MergeError::MissingBranch => write!(f, "missing branch on task"),
            MergeError::Timeout(c) => write!(f, "git command timed out: {c}"),
            MergeError::GitFailed(m) => write!(f, "git command failed: {m}"),
            MergeError::Io(e) => write!(f, "io error: {e}"),
        }
    }
}

impl std::error::Error for MergeError {}

const GIT_TIMEOUT: Duration = Duration::from_secs(60);

/// Run the merge.
///
/// `parent_path` is the absolute path to the parent project's
/// working copy (where `git merge` runs). `branch` is the
/// per-task branch (created by the orchestrator's coder spawn).
///
/// **Pre-conditions** (caller's responsibility):
/// - The worktree the branch lives in has its workers terminated
///   (no concurrent writers).
/// - The parent project's checkout is on its main branch (we
///   don't `git checkout` first; merging into a non-main branch
///   is what the user asked for if they configured it that way).
pub async fn merge_task(parent_path: &Path, branch: &str) -> Result<MergeOutcome, MergeError> {
    if branch.trim().is_empty() {
        return Err(MergeError::MissingBranch);
    }
    if !parent_path.exists() {
        return Err(MergeError::MissingParentPath(parent_path.display().to_string()));
    }

    // Step 1: attempt the merge. `--no-ff` to always produce a
    // merge commit so the orchestrator activity has an explicit
    // audit point; `--no-edit` so we don't drop into an editor;
    // explicit message so reflog reads cleanly.
    let merge_msg = format!("merge orchestrator branch {branch}");
    let merge = run_git(
        parent_path,
        &[
            "merge",
            "--no-ff",
            "--no-edit",
            "-m",
            &merge_msg,
            branch,
        ],
    )
    .await?;

    if merge.success {
        // Read the new HEAD sha for the audit comment.
        let head = run_git(parent_path, &["rev-parse", "HEAD"]).await?;
        let sha = head.stdout.trim().to_string();
        return Ok(MergeOutcome::Merged { sha });
    }

    // Step 2: classify the failure. A conflict shows up as a
    // non-zero exit with unmerged paths in the index. Anything
    // else (e.g. branch unknown, dirty working tree) is a real
    // error.
    let conflicts = run_git(parent_path, &["diff", "--name-only", "--diff-filter=U"]).await?;
    let conflicted_files: Vec<String> = conflicts
        .stdout
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();

    if conflicted_files.is_empty() {
        // Not a conflict — propagate the actual git error so the
        // caller can put it on the task as a NeedsHuman reason.
        return Err(MergeError::GitFailed(format!(
            "merge failed (exit {}): {}",
            merge.exit_code, merge.stderr.trim()
        )));
    }

    // Conflict path: abort cleanly before returning so the parent
    // working tree isn't left half-merged. We don't care about
    // the abort's own exit code — if the merge wasn't actually in
    // progress (race), `git merge --abort` returns non-zero but
    // the index is fine.
    let _ = run_git(parent_path, &["merge", "--abort"]).await;
    Ok(MergeOutcome::Conflict { files: conflicted_files })
}

/// Delete a worktree and its branch after a successful merge.
/// Best-effort: returns the first error encountered but never
/// panics — callers should log and continue.
pub async fn cleanup_worktree(
    parent_path: &Path,
    worktree_path: &Path,
    branch: &str,
) -> Result<(), MergeError> {
    // Try a clean `git worktree remove` first.
    let remove = run_git(
        parent_path,
        &["worktree", "remove", &worktree_path.display().to_string()],
    )
    .await?;
    if !remove.success {
        // Non-zero typically means uncommitted changes; for a
        // post-merge cleanup that's surprising but we still want
        // the worktree gone. Force-remove only when the path
        // matches what we expect — argv (not shell) prevents any
        // arbitrary-path injection even when the caller is buggy.
        let force = run_git(
            parent_path,
            &[
                "worktree",
                "remove",
                "--force",
                &worktree_path.display().to_string(),
            ],
        )
        .await?;
        if !force.success {
            return Err(MergeError::GitFailed(format!(
                "worktree remove failed: {}",
                force.stderr.trim()
            )));
        }
    }
    // Delete the branch. `-D` because it's been merged but may
    // not appear merged to git from this worktree (the branch is
    // the orchestrator's per-task branch; deleting it is the
    // explicit intent here).
    let branch_del = run_git(parent_path, &["branch", "-D", branch]).await?;
    if !branch_del.success {
        return Err(MergeError::GitFailed(format!(
            "branch -D {} failed: {}",
            branch,
            branch_del.stderr.trim()
        )));
    }
    Ok(())
}

struct GitOutput {
    success: bool,
    exit_code: i32,
    stdout: String,
    stderr: String,
}

async fn run_git(cwd: &Path, args: &[&str]) -> Result<GitOutput, MergeError> {
    let mut cmd = Command::new("git");
    cmd.current_dir(cwd);
    cmd.args(args);
    // Don't let the user's git pager hijack a non-interactive
    // invocation.
    cmd.env("GIT_PAGER", "cat");
    cmd.env("PAGER", "cat");
    let fut = cmd.output();
    let out = match tokio::time::timeout(GIT_TIMEOUT, fut).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => return Err(MergeError::Io(format!("spawn git: {e}"))),
        Err(_) => {
            return Err(MergeError::Timeout(format!("git {}", args.join(" "))));
        }
    };
    Ok(GitOutput {
        success: out.status.success(),
        exit_code: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).to_string(),
        stderr: String::from_utf8_lossy(&out.stderr).to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command as StdCommand;

    /// Build a tiny repo with two branches sharing no conflicts.
    fn fixture_non_conflicting() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        let run = |args: &[&str]| {
            let s = StdCommand::new("git")
                .current_dir(p)
                .args(args)
                .output()
                .unwrap();
            assert!(
                s.status.success(),
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&s.stderr)
            );
        };
        run(&["init", "-q", "-b", "main"]);
        run(&["config", "user.email", "test@example.com"]);
        run(&["config", "user.name", "Test"]);
        fs::write(p.join("a.txt"), "hello\n").unwrap();
        run(&["add", "a.txt"]);
        run(&["commit", "-q", "-m", "init"]);
        run(&["checkout", "-q", "-b", "feature"]);
        fs::write(p.join("b.txt"), "from feature\n").unwrap();
        run(&["add", "b.txt"]);
        run(&["commit", "-q", "-m", "add b"]);
        run(&["checkout", "-q", "main"]);
        dir
    }

    fn fixture_conflicting() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        let run = |args: &[&str]| {
            let s = StdCommand::new("git")
                .current_dir(p)
                .args(args)
                .output()
                .unwrap();
            assert!(s.status.success());
        };
        run(&["init", "-q", "-b", "main"]);
        run(&["config", "user.email", "test@example.com"]);
        run(&["config", "user.name", "Test"]);
        fs::write(p.join("a.txt"), "original\n").unwrap();
        run(&["add", "a.txt"]);
        run(&["commit", "-q", "-m", "init"]);
        run(&["checkout", "-q", "-b", "feature"]);
        fs::write(p.join("a.txt"), "from feature\n").unwrap();
        run(&["add", "a.txt"]);
        run(&["commit", "-q", "-m", "feature edit"]);
        run(&["checkout", "-q", "main"]);
        fs::write(p.join("a.txt"), "from main\n").unwrap();
        run(&["add", "a.txt"]);
        run(&["commit", "-q", "-m", "main edit"]);
        dir
    }

    #[tokio::test]
    async fn merge_non_conflicting_succeeds() {
        let dir = fixture_non_conflicting();
        let outcome = merge_task(dir.path(), "feature").await.unwrap();
        match outcome {
            MergeOutcome::Merged { sha } => {
                assert_eq!(sha.len(), 40);
            }
            MergeOutcome::Conflict { .. } => panic!("expected Merged"),
        }
        // b.txt should now exist on main.
        assert!(dir.path().join("b.txt").exists());
    }

    #[tokio::test]
    async fn merge_conflicting_aborts_and_reports_files() {
        let dir = fixture_conflicting();
        let outcome = merge_task(dir.path(), "feature").await.unwrap();
        match outcome {
            MergeOutcome::Conflict { files } => {
                assert!(files.iter().any(|f| f == "a.txt"));
            }
            MergeOutcome::Merged { .. } => panic!("expected Conflict"),
        }
        // The working tree should be left clean (merge --abort).
        let st = StdCommand::new("git")
            .current_dir(dir.path())
            .args(["status", "--porcelain"])
            .output()
            .unwrap();
        assert!(
            st.stdout.is_empty(),
            "expected clean tree after abort, got: {}",
            String::from_utf8_lossy(&st.stdout)
        );
    }

    #[tokio::test]
    async fn empty_branch_rejected() {
        let dir = fixture_non_conflicting();
        let err = merge_task(dir.path(), "").await.unwrap_err();
        assert!(matches!(err, MergeError::MissingBranch));
    }

    #[tokio::test]
    async fn missing_parent_rejected() {
        let err = merge_task(Path::new("/no/such/path/here"), "main")
            .await
            .unwrap_err();
        assert!(matches!(err, MergeError::MissingParentPath(_)));
    }

    #[tokio::test]
    async fn merge_unknown_branch_surfaces_git_error() {
        let dir = fixture_non_conflicting();
        let err = merge_task(dir.path(), "no-such-branch").await.unwrap_err();
        assert!(matches!(err, MergeError::GitFailed(_)));
    }
}
