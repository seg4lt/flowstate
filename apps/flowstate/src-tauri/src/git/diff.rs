use std::path::Path;
use std::process::Command;

use serde::Serialize;

/// Lightweight per-file entry returned by `get_git_diff_summary`.
/// Just path + line stats — no file contents. Designed so the diff
/// panel can show the full file list immediately without paying the
/// IPC + render cost of every file's before/after content. The
/// expensive content fetch happens lazily, one file at a time,
/// through `get_git_diff_file` when the user expands a row.
#[derive(Serialize, Clone)]
pub struct GitFileSummary {
    pub path: String,
    pub status: String,
    pub additions: u32,
    pub deletions: u32,
}

/// Full before/after for a single file, returned by
/// `get_git_diff_file`. `before` is HEAD content (empty for newly
/// added or untracked files); `after` is on-disk content (empty
/// for deleted files). Capped at GIT_DIFF_MAX_FILE_BYTES.
#[derive(Serialize)]
pub struct GitFileContents {
    before: String,
    after: String,
}

/// Maximum file size we'll inline into a diff payload. Keeps the
/// Tauri-bridge JSON message bounded so a session that touches a
/// 50 MB generated artifact doesn't lock up the frontend.
pub const GIT_DIFF_MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;

pub fn read_file_capped(abs: &Path) -> String {
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

pub fn git_show_head(repo: &str, file: &str) -> String {
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
///
/// Async wrapper: pushes all subprocess waits through
/// `spawn_blocking`, and inside the blocking task runs the two
/// independent git reads (tracked numstat + untracked ls-files)
/// concurrently via `std::thread::scope`. Both are read-only
/// queries that don't touch `.git/index.lock`, so they truly
/// overlap rather than serialise inside git.
#[tauri::command]
pub async fn get_git_diff_summary(path: String) -> Vec<GitFileSummary> {
    tauri::async_runtime::spawn_blocking(move || get_git_diff_summary_sync(path))
        .await
        .unwrap_or_default()
}

pub fn get_git_diff_summary_sync(path: String) -> Vec<GitFileSummary> {
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
pub fn run_git_diff_numstat(path: &str) -> Vec<GitFileSummary> {
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
pub fn run_git_ls_files_others(project_path: &Path, path: &str) -> Vec<GitFileSummary> {
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
pub async fn get_git_diff_file(path: String, file: String) -> GitFileContents {
    tauri::async_runtime::spawn_blocking(move || get_git_diff_file_sync(path, file))
        .await
        .unwrap_or_else(|_| GitFileContents {
            before: String::new(),
            after: String::new(),
        })
}

pub fn get_git_diff_file_sync(path: String, file: String) -> GitFileContents {
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
