use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tauri::Manager;
use tauri::ipc::Channel;
use tauri::State;
use tracing_subscriber::EnvFilter;
use zenui_daemon_core::{
    DaemonConfig, DaemonLifecycle, Transport, bootstrap_core_async, graceful_shutdown,
    transport_tauri,
};
use zenui_runtime_core::ConnectionObserver;
use transport_tauri::TauriTransport;

mod pty;
use pty::{PtyId, PtyManager};

mod user_config;
use user_config::{ProjectDisplay, SessionDisplay, UserConfigStore};

use std::collections::HashMap;

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
#[tauri::command]
fn list_git_branches(path: String) -> Result<GitBranchList, String> {
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
#[tauri::command]
fn list_git_worktrees(path: String) -> Result<Vec<GitWorktree>, String> {
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
            current_path = Some(rest.to_string());
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

/// Switch the working tree in `path` to `branch`. When `create_track`
/// is `Some(remote_ref)`, we run `checkout -b <branch> --track
/// <remote_ref>` to create a new local branch tracking a remote; when
/// it's `None`, a plain `checkout <branch>`. On failure, git's stderr
/// is returned verbatim so the UI can show the user exactly why (dirty
/// tree, merge conflict, nonexistent branch, etc.) rather than a
/// generic "checkout failed" message.
#[tauri::command]
fn git_checkout(
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

// ─────────────────────────────────────────────────────────────────
// /code editor view — file picker + single-file read
// ─────────────────────────────────────────────────────────────────

/// Cap the picker list so we never send a million entries to the
/// frontend for a huge repo. 20k is more than enough for a Cmd+P
/// picker — anyone with more than that would already be using real
/// file search (see fff-search upgrade path in follow-ups).
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
    let abs_canon = abs
        .canonicalize()
        .map_err(|e| format!("file path: {e}"))?;
    if !abs_canon.starts_with(&project_canon) {
        return Err("file is outside the project root".into());
    }
    let meta = std::fs::metadata(&abs_canon)
        .map_err(|e| format!("metadata: {e}"))?;
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
/// exits. We intentionally don't kill the editor when flowzen quits.
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
/// header explains as "top 3 / bottom 3". Expand-to-more is a
/// next-step item.
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
            self.line_budget_remaining =
                self.line_budget_remaining.saturating_sub(1);
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
        Some(
            ob.build()
                .map_err(|e| format!("override build: {e}"))?,
        )
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
fn pty_resize(manager: State<'_, PtyManager>, id: PtyId, cols: u16, rows: u16) -> Result<(), String> {
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
// user_config — flowzen-app-owned key/value store
// ─────────────────────────────────────────────────────────────────
//
// Backed by `~/.flowzen/user_config.sqlite` (its own file, not the
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

/// Resolved cross-platform app data dir for Flowzen — the same
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

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .manage(PtyManager::new())
        .setup(|app| {
            let app_handle = app.handle().clone();

            // Cross-platform per-user data directory. Tauri resolves
            // this to:
            //   - macOS:   ~/Library/Application Support/<bundle.id>/
            //   - Linux:   ~/.local/share/<bundle.id>/
            //   - Windows: %APPDATA%/<bundle.id>/
            // Everything flowzen owns — daemon SQLite + threads dir +
            // the app's own user_config sqlite — lives under here.
            let flowzen_root = app
                .path()
                .app_data_dir()
                .expect("failed to resolve app data dir");
            std::fs::create_dir_all(&flowzen_root)
                .expect("failed to create app data dir");
            std::fs::create_dir_all(flowzen_root.join("threads")).ok();

            // Open the flowzen-app-owned user config store. Lives in
            // its own file at <app_data_dir>/user_config.sqlite — a
            // separate database from the daemon's. SDK and app each
            // own their own SQLite; nothing about app-level UI config
            // belongs in the daemon's schema.
            let user_config_store = UserConfigStore::open(&flowzen_root)
                .expect("failed to open user_config store");
            app.manage(user_config_store);

            let transport = Box::new(TauriTransport::new(app_handle));

            let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);

            // Run the daemon on Tauri's existing tokio runtime so the
            // process has exactly one thread pool. The previous shape
            // (std::thread::spawn + bootstrap_core's own runtime) was
            // a workaround for "cannot start a runtime from within a
            // runtime"; bootstrap_core_async removes that need by
            // letting us share the host runtime.
            tauri::async_runtime::spawn(async move {
                let mut config = DaemonConfig::with_project_root(flowzen_root);
                config.idle_timeout = Duration::MAX;

                let core = bootstrap_core_async(&config)
                    .await
                    .expect("daemon bootstrap failed");

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
            get_git_diff_summary,
            get_git_diff_file,
            list_project_files,
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
