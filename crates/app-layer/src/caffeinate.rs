//! Cross-platform "keep display awake while a turn is in flight"
//! controller.
//!
//! When the `system.caffeinate` user_config key is `"true"`, this
//! controller arms the OS sleep-prevention hook for the lifetime of
//! any in-flight agent turn, releases it as soon as all turns finish,
//! and lets the user force-kill it from the settings UI.
//!
//! # Platform implementations
//!
//! - **macOS**: spawns one `caffeinate -d -t <SAFETY_TIMEOUT>` child
//!   per active "session" (the 0→1 in-flight transition). When the
//!   child naturally times out, a tokio watcher task wakes and
//!   respawns iff turns are still in flight. `kill_on_drop(true)` on
//!   the `tokio::process::Child` is the last-resort cleanup.
//!
//! - **Windows**: calls `SetThreadExecutionState` with
//!   `ES_CONTINUOUS | ES_SYSTEM_REQUIRED | ES_DISPLAY_REQUIRED` — the
//!   process-wide flag stays armed until cleared with
//!   `ES_CONTINUOUS` alone. No subprocess, no watcher, no timeout
//!   path. Keeping the controller's state model uniform with macOS
//!   makes the [`TurnLifecycleObserver`] impl identical across both
//!   platforms.
//!
//! Why a timeout on macOS? Two reasons:
//!   1. **Crash safety net** — if the host process is SIGKILL'd, the
//!      caffeinate child reparents to launchd and keeps running. The
//!      `-t` flag ensures self-termination within `SAFETY_TIMEOUT`.
//!   2. **Bounded resource usage** — we never want a forgotten
//!      caffeinate sitting around indefinitely.
//!
//! Windows doesn't have the same orphaning concern: if flowstate
//! dies, the process-wide thread-execution state dies with it
//! automatically.
//!
//! # Single-instance coordination
//!
//! Inner state lives behind a `Mutex<Inner>`. macOS uses every field
//! (pid + kill_tx + generation) for watcher coordination; Windows
//! uses `kill_tx.is_some()` as a plain "armed?" flag and otherwise
//! ignores pid + generation. Force-kill / on_turn_end (1→0) bumps
//! the generation so a stale macOS watcher wake-up cannot resurrect
//! a child the controller has already retired.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};

use serde::Serialize;
#[cfg(target_os = "macos")]
use std::process::Stdio;
#[cfg(target_os = "macos")]
use tokio::process::Command;
use tokio::runtime::Handle;
use tokio::sync::oneshot;
use zenui_runtime_core::TurnLifecycleObserver;

use crate::user_config::UserConfigStore;

/// User-facing config key. Stored as `"true"` / `"false"` to match the
/// rest of `defaults-settings.ts` (every existing toggle uses the
/// same encoding).
pub const CONFIG_KEY: &str = "system.caffeinate";

/// Hard upper bound on how long any single caffeinate invocation runs
/// (macOS only — Windows uses a permanent flag with no timeout).
/// Acts as a safety net for crash-induced orphans (see module
/// comment). Five minutes is short enough that a forgotten process
/// is bounded but long enough that respawn churn is negligible
/// during real long turns.
#[cfg(target_os = "macos")]
const SAFETY_TIMEOUT_SECS: u64 = 300;

/// Wire-format snapshot of caffeinate state for the settings UI.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CaffeinateStatus {
    pub enabled: bool,
    pub running: bool,
    /// macOS-only — pid of the active `caffeinate` child. Always
    /// `None` on Windows because there is no subprocess.
    pub pid: Option<u32>,
}

/// Controller plus `TurnLifecycleObserver` impl. Construct via
/// [`CaffeinateController::new`] which returns an `Arc<Self>` because
/// the watcher tasks need a `Weak` back-reference.
pub struct CaffeinateController {
    /// Mirrors the daemon-wide in-flight turn count. We track our own
    /// counter (rather than calling back into `DaemonLifecycle`) so
    /// the controller has zero knowledge of daemon-core internals.
    in_flight: AtomicUsize,
    inner: Mutex<Inner>,
    user_config: UserConfigStore,
    /// Set true by `force_kill`. Suppresses respawn until the next
    /// 1→0 turn transition, at which point it's cleared.
    user_killed: AtomicBool,
    /// Tokio handle captured at construction time. The
    /// `TurnLifecycleObserver` trait methods are sync, but on macOS
    /// we need to `runtime.spawn(...)` the watcher task that awaits
    /// child exit. Unused on Windows but kept in the struct so the
    /// constructor signature is platform-uniform.
    #[allow(dead_code)] // unused on Windows; kept for API parity
    runtime: Handle,
    /// Self-weak — installed via `Arc::new_cyclic` so watcher tasks
    /// hold a `Weak<Self>` and don't keep the controller alive.
    /// Unused on Windows (no watcher tasks) but kept for API parity.
    #[allow(dead_code)] // unused on Windows; kept for API parity
    self_weak: Weak<CaffeinateController>,
}

struct Inner {
    /// PID of the currently running caffeinate child, if any (macOS).
    /// Always `None` on Windows.
    pid: Option<u32>,
    /// macOS: sender to ask the watcher to kill its child and exit
    /// early. `Some` while a child is alive (and a watcher owns it),
    /// `None` otherwise.
    ///
    /// Windows: an opaque "are we armed?" flag — the receiver is
    /// dropped immediately after construction; we only ever check
    /// `is_some()`. `SetThreadExecutionState` is the actual sleep
    /// prevention; this field is a per-controller bookkeeping bit.
    kill_tx: Option<oneshot::Sender<()>>,
    /// macOS: bumped on every spawn AND every retirement. The
    /// watcher captures its generation at spawn time and hands it
    /// back via [`CaffeinateController::on_watcher_exit`]; if the
    /// value no longer matches, the watcher's wake-up is stale.
    /// Prevents a "kill + immediate respawn" race from being undone
    /// by the old watcher's natural-exit handler.
    ///
    /// Unused on Windows (no watcher) but kept so the struct shape
    /// is identical across platforms.
    #[allow(dead_code)] // unused on Windows; kept for API parity
    generation: u64,
}

impl CaffeinateController {
    pub fn new(user_config: UserConfigStore, runtime: Handle) -> Arc<Self> {
        Arc::new_cyclic(|me| Self {
            in_flight: AtomicUsize::new(0),
            inner: Mutex::new(Inner {
                pid: None,
                kill_tx: None,
                generation: 0,
            }),
            user_config,
            user_killed: AtomicBool::new(false),
            runtime,
            self_weak: me.clone(),
        })
    }

    /// Whether the user toggle is currently on. Reads `UserConfigStore`
    /// each call (microsecond SQLite read) so a setting flip is picked
    /// up without any explicit notification plumbing.
    fn is_enabled(&self) -> bool {
        matches!(self.user_config.get(CONFIG_KEY), Ok(Some(v)) if v == "true")
    }

    /// Snapshot for the settings UI.
    pub fn status(&self) -> CaffeinateStatus {
        let inner = match self.inner.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        CaffeinateStatus {
            enabled: self.is_enabled(),
            running: inner.kill_tx.is_some(),
            pid: inner.pid,
        }
    }

    /// User clicked "Force kill" in settings. Releases the active
    /// caffeinate / SetThreadExecutionState immediately. The setting
    /// itself stays on; the `user_killed` flag suppresses respawn
    /// until the in-flight counter returns to 0, at which point a
    /// fresh turn will arm caffeinate normally.
    pub fn force_kill(&self) {
        self.user_killed.store(true, Ordering::SeqCst);
        let mut inner = match self.inner.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        self.retire_locked(&mut inner);
    }

    /// Called from the settings page after the toggle is flipped, so
    /// the controller picks up the change immediately rather than
    /// waiting for the next turn boundary. Idempotent.
    pub fn refresh(&self) {
        let mut inner = match self.inner.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        if !self.is_enabled() {
            self.retire_locked(&mut inner);
            return;
        }
        if self.in_flight.load(Ordering::SeqCst) > 0 && !self.user_killed.load(Ordering::SeqCst) {
            self.spawn_locked(&mut inner);
        }
    }

    /// Arm sleep prevention if and only if it's not already armed.
    /// Called with the inner mutex held.
    fn spawn_locked(&self, inner: &mut Inner) {
        if inner.kill_tx.is_some() {
            return; // already armed
        }

        #[cfg(target_os = "macos")]
        {
            // Resolve `caffeinate` through the workspace binary
            // resolver so user-configured extras + platform fallbacks
            // apply (`/usr/bin/caffeinate` is the standard location,
            // and the resolver's macOS fallbacks include it).
            let mut cmd = Command::new(zenui_provider_api::resolve_cli_command("caffeinate"));
            cmd.arg("-d") // prevent display sleep
                .arg("-t")
                .arg(SAFETY_TIMEOUT_SECS.to_string())
                .env("PATH", zenui_provider_api::path_with_extras(&[]))
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                // If the watcher task is dropped (e.g. runtime
                // shutdown after a panic), the Child's Drop sends
                // SIGKILL — our last line of defense against
                // orphaned caffeinate processes.
                .kill_on_drop(true);
            let mut child = match cmd.spawn() {
                Ok(c) => c,
                Err(err) => {
                    tracing::warn!(%err, "caffeinate spawn failed; display sleep prevention disabled");
                    return;
                }
            };
            let pid = child.id();
            let (tx, rx) = oneshot::channel::<()>();
            let generation = inner.generation.wrapping_add(1);
            inner.generation = generation;
            inner.pid = pid;
            inner.kill_tx = Some(tx);
            tracing::debug!(?pid, generation, "caffeinate spawned");

            let weak = self.self_weak.clone();
            self.runtime.spawn(async move {
                // Either caffeinate exits on its own (SAFETY_TIMEOUT
                // reached, or someone external killed it) OR we
                // receive a retirement signal and SIGTERM the child
                // ourselves.
                let killed_by_us = tokio::select! {
                    wait_res = child.wait() => {
                        if let Err(err) = wait_res {
                            tracing::warn!(%err, "caffeinate wait failed");
                        }
                        false
                    }
                    _ = rx => {
                        // The Sender side either sent `()` or was
                        // dropped. Both mean "retire this watcher".
                        // Kill + reap.
                        let _ = child.start_kill();
                        let _ = child.wait().await;
                        true
                    }
                };
                if let Some(ctrl) = weak.upgrade() {
                    ctrl.on_watcher_exit(generation, killed_by_us);
                }
            });
        }

        #[cfg(target_os = "windows")]
        {
            // Win32: ES_CONTINUOUS *combined with* ES_SYSTEM_REQUIRED
            // and ES_DISPLAY_REQUIRED tells the kernel "keep the
            // system awake AND keep the display on, until I clear
            // it explicitly". `ES_CONTINUOUS` alone (no other flags)
            // is the cleared state — that's what `retire_locked`
            // does on this platform.
            //
            // The return value is the *previous* execution state.
            // 0 means the call failed; we log + bail without arming
            // the bookkeeping flag.
            //
            // SAFETY: documented Win32 FFI; flag constants are
            // u32 bitfield values. No pointers, no buffers.
            unsafe {
                let prev = windows_sys::Win32::System::Power::SetThreadExecutionState(
                    windows_sys::Win32::System::Power::ES_CONTINUOUS
                        | windows_sys::Win32::System::Power::ES_SYSTEM_REQUIRED
                        | windows_sys::Win32::System::Power::ES_DISPLAY_REQUIRED,
                );
                if prev == 0 {
                    tracing::warn!(
                        "SetThreadExecutionState failed; sleep prevention disabled this turn"
                    );
                    return;
                }
            }
            // Bookkeeping: kill_tx.is_some() is what `status.running`
            // and the spawn-guard check at the top of this method
            // read. Receiver is dropped immediately — we never
            // signal Windows-side retirement through the channel.
            let (tx, _rx) = oneshot::channel::<()>();
            inner.kill_tx = Some(tx);
            inner.pid = None;
            tracing::debug!("SetThreadExecutionState armed (system+display required)");
        }

        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        {
            // Other platforms: the controller still tracks turn
            // counts (so `status()` is consistent) but has no
            // backing OS hook. Setting kill_tx prevents a spurious
            // re-arm from the next 0→1 transition.
            let (tx, _rx) = oneshot::channel::<()>();
            inner.kill_tx = Some(tx);
            inner.pid = None;
        }
    }

    /// Tear down the active sleep-prevention hook (if any). Safe to
    /// call when none exists. Called with the inner mutex held.
    /// Bumps `generation` so any in-flight macOS watcher's eventual
    /// `on_watcher_exit` callback is recognised as stale and ignored.
    fn retire_locked(&self, inner: &mut Inner) {
        if let Some(tx) = inner.kill_tx.take() {
            // macOS watcher will receive the signal (or notice the
            // channel closed) and clean up its child. Send may fail
            // if the watcher already exited concurrently — fine.
            // Windows: receiver was dropped at spawn time; this
            // send is a no-op.
            let _ = tx.send(());
        }

        #[cfg(target_os = "windows")]
        {
            // Clear the execution-state flags. ES_CONTINUOUS without
            // ES_SYSTEM_REQUIRED / ES_DISPLAY_REQUIRED is the
            // documented "back to normal" state.
            //
            // SAFETY: documented Win32 FFI.
            unsafe {
                windows_sys::Win32::System::Power::SetThreadExecutionState(
                    windows_sys::Win32::System::Power::ES_CONTINUOUS,
                );
            }
        }

        inner.pid = None;
        inner.generation = inner.generation.wrapping_add(1);
    }

    /// Watcher callback. Called once the spawned task's child has
    /// exited (and been reaped) for any reason. We may need to
    /// respawn for the natural-timeout case if turns are still in
    /// flight. macOS-only — Windows has no watcher because
    /// `SetThreadExecutionState` doesn't time out.
    #[cfg(target_os = "macos")]
    fn on_watcher_exit(&self, generation: u64, killed_by_us: bool) {
        let mut inner = match self.inner.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        if inner.generation != generation {
            // Already superseded by a newer spawn or retirement —
            // do nothing. Specifically, we do NOT clear pid/kill_tx
            // here because they belong to the new generation.
            return;
        }
        // We owned the slot. Clear it.
        inner.pid = None;
        inner.kill_tx = None;
        // Don't bump generation — the value we just observed is the
        // canonical "no child running" state.

        if killed_by_us {
            // Retirement was requested explicitly (force_kill,
            // on_turn_end 1→0, refresh-while-disabled, or Drop).
            return;
        }
        // Natural exit: caffeinate -t timeout fired (or someone
        // SIGKILLed it externally). Respawn iff still needed.
        let want_respawn = self.in_flight.load(Ordering::SeqCst) > 0
            && self.is_enabled()
            && !self.user_killed.load(Ordering::SeqCst);
        if want_respawn {
            tracing::debug!("caffeinate timeout reached, respawning");
            self.spawn_locked(&mut inner);
        }
    }
}

impl TurnLifecycleObserver for CaffeinateController {
    fn on_turn_start(&self, _session_id: &str) {
        let prev = self.in_flight.fetch_add(1, Ordering::SeqCst);
        if prev != 0 {
            return; // not a 0→1 transition
        }
        if !self.is_enabled() || self.user_killed.load(Ordering::SeqCst) {
            return;
        }
        let mut inner = match self.inner.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        self.spawn_locked(&mut inner);
    }

    fn on_turn_end(&self, _session_id: &str) {
        let prev = self.in_flight.fetch_sub(1, Ordering::SeqCst);
        if prev != 1 {
            return; // not a 1→0 transition
        }
        let mut inner = match self.inner.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        self.retire_locked(&mut inner);
        // Reset the user_killed gate now that we're back to quiescent.
        // The next 0→1 transition will arm caffeinate normally.
        self.user_killed.store(false, Ordering::SeqCst);
    }
}

impl Drop for CaffeinateController {
    fn drop(&mut self) {
        if let Ok(mut inner) = self.inner.lock() {
            if let Some(tx) = inner.kill_tx.take() {
                let _ = tx.send(());
            }
            inner.pid = None;
        }

        #[cfg(target_os = "windows")]
        {
            // Belt-and-braces: even if the controller is being
            // dropped with no live "armed" flag, calling
            // SetThreadExecutionState(ES_CONTINUOUS) is idempotent
            // and ensures we don't leave the desktop in a weird
            // "stays awake forever" state if a previous arm
            // somehow failed to retire. Cheap.
            //
            // SAFETY: documented Win32 FFI.
            unsafe {
                windows_sys::Win32::System::Power::SetThreadExecutionState(
                    windows_sys::Win32::System::Power::ES_CONTINUOUS,
                );
            }
        }
    }
}
