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
//!   * an `AtomicBool` the reader polls for flow-control pause
//!
//! The flow-control model is the xterm.js `write(data, cb)` watermark
//! pattern: the frontend calls `pty_pause` when its pending-ack count
//! crosses the high watermark and `pty_resume` when it falls back
//! below the low watermark. The reader thread checks the flag between
//! reads and sleeps in a tight loop while paused — simple, and the
//! pty buffer itself provides the real backpressure to the child
//! process when the reader stops draining it.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use tauri::ipc::Channel;

pub type PtyId = u64;

struct PtySession {
    writer: Mutex<Box<dyn Write + Send>>,
    master: Mutex<Box<dyn MasterPty + Send>>,
    child: Mutex<Box<dyn portable_pty::Child + Send + Sync>>,
    paused: Arc<AtomicBool>,
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
        on_data: Channel<Vec<u8>>,
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
        let paused = Arc::new(AtomicBool::new(false));

        let session = Arc::new(PtySession {
            writer: Mutex::new(writer),
            master: Mutex::new(pair.master),
            child: Mutex::new(child),
            paused: paused.clone(),
        });

        self.sessions.lock().unwrap().insert(id, session);

        let paused_for_thread = paused;
        std::thread::Builder::new()
            .name(format!("pty-reader-{id}"))
            .spawn(move || {
                reader_loop(reader, on_data, paused_for_thread);
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
        session.paused.store(true, Ordering::Relaxed);
        Ok(())
    }

    pub fn resume(&self, id: PtyId) -> Result<(), String> {
        let session = self.get(id)?;
        session.paused.store(false, Ordering::Relaxed);
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
    channel: Channel<Vec<u8>>,
    paused: Arc<AtomicBool>,
) {
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        // Lightweight busy-sleep while paused. The pty buffer will
        // fill and SIGSTOP-equivalent-block the child on its own
        // write() once we stop draining, which is the actual
        // backpressure — this sleep just keeps us from burning CPU.
        while paused.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(10));
        }

        match reader.read(&mut buf) {
            Ok(0) => break, // child exited, slave fd closed
            Ok(n) => {
                let chunk = buf[..n].to_vec();
                if channel.send(chunk).is_err() {
                    // Frontend dropped the channel — terminal was
                    // disposed or the webview reloaded. Stop reading.
                    break;
                }
            }
            Err(e) => {
                tracing::warn!("pty reader error: {e}");
                break;
            }
        }
    }
}
