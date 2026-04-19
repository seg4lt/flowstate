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

use std::collections::HashMap;
use std::io::BufRead;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;
use tauri::{Manager, State};
use tauri::ipc::Channel;

use super::diff::GitFileSummary;
use crate::lock::lock_ok;

/// Events streamed from `watch_git_diff_summary` back to the
/// frontend over a Tauri `Channel<T>`. Tagged enum with snake_case
/// kind so the JS side can discriminate with a simple switch.
#[derive(Serialize, Clone)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DiffSummaryEvent {
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
pub struct DiffTaskHandle {
    child: Arc<Mutex<Option<Child>>>,
    cancelled: Arc<AtomicBool>,
}

#[derive(Default)]
pub struct DiffTasks {
    tasks: Mutex<HashMap<u64, DiffTaskHandle>>,
}

/// Cap for per-file line counting during the fast path. Reading a
/// 50 MB untracked generated artifact just to show `+N` in the
/// header would defeat the whole point of Phase 1 being fast.
pub const UNTRACKED_COUNT_MAX_BYTES: u64 = 2 * 1024 * 1024;

pub fn count_file_lines_bounded(abs: &Path) -> u32 {
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
pub fn collect_git_status_files(project_path: &Path, path: &str) -> Vec<GitFileSummary> {
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
pub fn run_watch_diff(
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
    *lock_ok(&child_slot) = Some(child);

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
                if let Some(mut c) = lock_ok(&child_slot).take() {
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
    let child_opt = lock_ok(&child_slot).take();
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
pub async fn watch_git_diff_summary(
    app: tauri::AppHandle,
    path: String,
    token: u64,
    on_event: Channel<DiffSummaryEvent>,
) {
    let cancelled = Arc::new(AtomicBool::new(false));
    let child_slot: Arc<Mutex<Option<Child>>> = Arc::new(Mutex::new(None));

    if let Some(tasks) = app.try_state::<DiffTasks>() {
        lock_ok(&tasks.tasks).insert(
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
        let result = run_watch_diff(
            &path,
            &on_event,
            cancelled_for_thread,
            child_for_thread,
        );
        if let Some(tasks) = app_for_thread.try_state::<DiffTasks>() {
            lock_ok(&tasks.tasks).remove(&token);
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
pub fn stop_git_diff_summary(tasks: State<'_, DiffTasks>, token: u64) {
    let handle = lock_ok(&tasks.tasks).remove(&token);
    if let Some(handle) = handle {
        handle.cancelled.store(true, Ordering::SeqCst);
        if let Some(mut child) = lock_ok(&handle.child).take() {
            let _ = child.kill();
        }
    }
}
