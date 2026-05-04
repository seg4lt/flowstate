//! Integrated terminal backend.
//!
//! One `PtyManager` is managed as Tauri state. It owns every live
//! PTY session keyed by a monotonic `u64` id. Each session has:
//!   * a dedicated blocking reader thread that streams raw bytes to
//!     the frontend via a per-session `Channel<Vec<u8>>` passed in
//!     at open time
//!   * a parked writer behind a mutex for `pty_write`
//!   * the master handle for `pty_resize`
//!   * the child handle for `pty_kill`
//!   * a `PauseGate` (`Mutex<bool>` + `Condvar`) the reader blocks
//!     on for flow-control pause
//!
//! The flow-control model is the xterm.js `write(data, cb)` watermark
//! pattern: the frontend calls `pty_pause` when its pending-ack count
//! crosses the high watermark and `pty_resume` when it falls back
//! below the low watermark. The reader thread blocks on a `Condvar`
//! between reads while paused — the pty buffer itself provides the
//! real backpressure to the child process when the reader stops
//! draining it, and a stuck-paused terminal stays genuinely 0 Hz
//! instead of waking the scheduler 100×/s.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use serde::Serialize;
use tauri::ipc::Channel;

pub type PtyId = u64;

/// Events streamed from a PTY session to the frontend over a single
/// per-session `Channel<PtyEvent>`. Multiplexing data and lifecycle
/// on the same channel preserves ordering — a final burst of output
/// followed by EOF arrives in source order — and avoids the second
/// IPC pipe a parallel exit channel would require.
#[derive(Serialize, Clone)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PtyEvent {
    /// Raw bytes from the master end of the pty.
    Data { bytes: Vec<u8> },
    /// The reader hit EOF or a read error — the child shell is gone.
    /// `code` is `None` because we deliberately do NOT block in
    /// `wait()` from the reader thread (would race with `kill_all` on
    /// the child mutex). Frontend treats this as "tab should close".
    Exit { code: Option<i32> },
}

/// Reader-thread pause gate. `paused` is the flag the reader checks
/// between blocking reads; `cvar` lets `pty_resume` wake the reader
/// immediately instead of waiting out the next spin-sleep tick.
///
/// Why a `Condvar` instead of the pre-existing 10 ms spin loop: the
/// old `while paused { sleep(10ms) }` parks the reader thread, so it
/// doesn't burn CPU directly — but it produces 100 wake-ups/s per
/// paused terminal, and any frontend that leaves a terminal
/// `paused = true` (panel hidden without resume, popout torn down
/// mid-flow-control, resize race) keeps that drum-beat going forever.
/// The Condvar version blocks the thread until `pty_resume` actually
/// fires, so a stuck-paused terminal is genuinely 0 Hz instead of
/// "looks idle but ticking the scheduler."
struct PauseGate {
    paused: Mutex<bool>,
    cvar: Condvar,
}

impl PauseGate {
    fn new() -> Self {
        Self {
            paused: Mutex::new(false),
            cvar: Condvar::new(),
        }
    }

    fn set(&self, value: bool) {
        let mut guard = self.paused.lock().unwrap();
        if *guard == value {
            return;
        }
        *guard = value;
        // Always notify on transition. Resume → reader wakes from
        // `wait_while`. Pause → no waiter to notify, but the cheap
        // notify_all keeps the API symmetric.
        self.cvar.notify_all();
    }

    /// Block while the gate is held. Returns immediately when the
    /// gate is open. Spurious wakes are absorbed by the predicate.
    fn wait_while_paused(&self) {
        let guard = self.paused.lock().unwrap();
        let _unused = self.cvar.wait_while(guard, |paused| *paused).unwrap();
    }
}

struct PtySession {
    writer: Mutex<Box<dyn Write + Send>>,
    master: Mutex<Box<dyn MasterPty + Send>>,
    child: Mutex<Box<dyn portable_pty::Child + Send + Sync>>,
    pause_gate: Arc<PauseGate>,
}

pub struct PtyManager {
    next_id: AtomicU64,
    sessions: Mutex<HashMap<PtyId, Arc<PtySession>>>,
}

impl Default for PtyManager {
    fn default() -> Self {
        Self::new()
    }
}

impl PtyManager {
    pub fn new() -> Self {
        Self {
            next_id: AtomicU64::new(1),
            sessions: Mutex::new(HashMap::new()),
        }
    }

    pub fn open(
        &self,
        cols: u16,
        rows: u16,
        cwd: Option<String>,
        shell: Option<String>,
        on_event: Channel<PtyEvent>,
    ) -> Result<PtyId, String> {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| format!("openpty: {e}"))?;

        let shell_path = shell.unwrap_or_else(default_shell);
        let mut cmd = CommandBuilder::new(&shell_path);
        // Default cwd to the user's home dir when the frontend
        // doesn't pass one (folder-less session, or project path
        // couldn't be resolved). Falls back to `/` as an absolute
        // last resort so the child never inherits Tauri's bundle
        // location.
        let effective_cwd = cwd
            .filter(|s| !s.is_empty())
            .or_else(|| dirs::home_dir().map(|p| p.to_string_lossy().into_owned()))
            .unwrap_or_else(|| "/".to_string());
        cmd.cwd(&effective_cwd);
        // Inherit the parent env so the user's PATH, editor prefs,
        // language settings etc. are all live inside the terminal.
        for (k, v) in std::env::vars() {
            cmd.env(k, v);
        }
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");

        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| format!("spawn `{shell_path}`: {e}"))?;
        // Drop our copy of the slave so the master reader sees EOF
        // as soon as the child exits.
        drop(pair.slave);

        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| format!("clone reader: {e}"))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|e| format!("take writer: {e}"))?;

        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let pause_gate = Arc::new(PauseGate::new());

        let session = Arc::new(PtySession {
            writer: Mutex::new(writer),
            master: Mutex::new(pair.master),
            child: Mutex::new(child),
            pause_gate: pause_gate.clone(),
        });

        self.sessions.lock().unwrap().insert(id, session);

        let gate_for_thread = pause_gate;
        std::thread::Builder::new()
            .name(format!("pty-reader-{id}"))
            .spawn(move || {
                reader_loop(reader, on_event, gate_for_thread);
            })
            .map_err(|e| format!("spawn reader thread: {e}"))?;

        Ok(id)
    }

    pub fn write(&self, id: PtyId, data: &[u8]) -> Result<(), String> {
        let session = self.get(id)?;
        let mut w = session.writer.lock().unwrap();
        w.write_all(data).map_err(|e| format!("write: {e}"))?;
        w.flush().map_err(|e| format!("flush: {e}"))
    }

    pub fn resize(&self, id: PtyId, cols: u16, rows: u16) -> Result<(), String> {
        let session = self.get(id)?;
        let master = session.master.lock().unwrap();
        master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| format!("resize: {e}"))
    }

    pub fn pause(&self, id: PtyId) -> Result<(), String> {
        let session = self.get(id)?;
        session.pause_gate.set(true);
        Ok(())
    }

    pub fn resume(&self, id: PtyId) -> Result<(), String> {
        let session = self.get(id)?;
        session.pause_gate.set(false);
        Ok(())
    }

    pub fn kill(&self, id: PtyId) -> Result<(), String> {
        let removed = self.sessions.lock().unwrap().remove(&id);
        if let Some(session) = removed {
            // Best-effort SIGKILL. The reader thread will hit EOF and
            // exit on its own once the child's slave fd closes.
            let _ = session.child.lock().unwrap().kill();
        }
        Ok(())
    }

    /// Kill every live session. Called on window-destroyed so we
    /// don't leave orphan shells behind when the user quits.
    pub fn kill_all(&self) {
        let sessions: Vec<_> = {
            let mut map = self.sessions.lock().unwrap();
            map.drain().map(|(_, s)| s).collect()
        };
        for session in sessions {
            let _ = session.child.lock().unwrap().kill();
        }
    }

    fn get(&self, id: PtyId) -> Result<Arc<PtySession>, String> {
        self.sessions
            .lock()
            .unwrap()
            .get(&id)
            .cloned()
            .ok_or_else(|| format!("unknown pty `{id}`"))
    }
}

fn default_shell() -> String {
    #[cfg(windows)]
    {
        // Prefer PowerShell 7+ if it's installed (via `pwsh.exe` on
        // PATH); fall back to cmd.exe which is always present.
        if which_in_path("pwsh.exe") {
            "pwsh.exe".into()
        } else {
            "cmd.exe".into()
        }
    }
    #[cfg(unix)]
    {
        std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".into())
    }
}

#[cfg(windows)]
fn which_in_path(binary: &str) -> bool {
    if let Ok(path) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path) {
            if dir.join(binary).is_file() {
                return true;
            }
        }
    }
    false
}

fn reader_loop(
    mut reader: Box<dyn Read + Send>,
    channel: Channel<PtyEvent>,
    pause_gate: Arc<PauseGate>,
) {
    let mut buf = vec![0u8; 16 * 1024];
    // Track whether we exited via EOF (child died on its own — user
    // typed `exit`, or signal) versus a frontend-side channel drop
    // (terminal was disposed or webview reloaded). Only the former
    // should fire an `Exit` event; in the latter the frontend can't
    // hear us anyway.
    let mut child_gone = false;
    loop {
        // Block until the gate is open. The pty buffer fills and
        // SIGSTOP-equivalent-blocks the child on its own write() once
        // we stop draining, which is the real backpressure; this just
        // keeps the reader thread genuinely parked while paused
        // (vs. the prior 10 ms spin that woke the scheduler 100×/s
        // for as long as the gate stayed closed).
        pause_gate.wait_while_paused();

        match reader.read(&mut buf) {
            Ok(0) => {
                // Slave fd closed — the child exited (clean `exit`,
                // signal, or kill from another thread). Notify the
                // frontend so it can auto-close the tab.
                child_gone = true;
                break;
            }
            Ok(n) => {
                let chunk = buf[..n].to_vec();
                if channel.send(PtyEvent::Data { bytes: chunk }).is_err() {
                    // Frontend dropped the channel — terminal was
                    // disposed or the webview reloaded. Stop reading.
                    break;
                }
            }
            Err(e) => {
                // EIO on the master after the slave closes is normal
                // on some platforms — treat it as "child gone" so the
                // tab still auto-closes on `exit`. Other errors get
                // logged for diagnosis but reach the same outcome.
                tracing::warn!("pty reader error: {e}");
                child_gone = true;
                break;
            }
        }
    }
    if child_gone {
        // Best-effort. We deliberately do NOT block in `wait()` to
        // harvest the exit status — the child mutex may be contended
        // by `kill_all` during shutdown, and the frontend doesn't use
        // the code today. Send `None` and call it done.
        let _ = channel.send(PtyEvent::Exit { code: None });
    }
}
