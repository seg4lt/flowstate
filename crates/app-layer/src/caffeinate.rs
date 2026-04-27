//! macOS-only "keep display awake while a turn is in flight" controller.
//!
//! When the `system.caffeinate` user_config key is `"true"`, this controller
//! spawns one (and only one) `caffeinate -d -t <SAFETY_TIMEOUT>` child for
//! the lifetime of any in-flight agent turn, kills it as soon as all turns
//! finish, and lets the user force-kill it from the settings UI.
//!
//! Why a timeout? Two reasons:
//!   1. **Crash safety net** — if the host process is SIGKILL'd, every
//!      child we spawned reparents to launchd and keeps running. The
//!      `-t` flag ensures `caffeinate` self-terminates within at most
//!      `SAFETY_TIMEOUT` seconds even if our `Drop` never runs.
//!   2. **Bounded resource usage** — we never want a forgotten caffeinate
//!      sitting around indefinitely.
//!
//! Renewal is event-driven, not polled: the spawn path immediately
//! launches a tokio watcher task that `child.wait().await`s. When the
//! caffeinate child exits (timeout reached, SIGTERM from us, or any
//! other reason), the watcher wakes — exactly once, no busy-loop — and
//! asks the controller whether a fresh caffeinate is still needed. If
//! `in_flight > 0 && enabled && !user_killed` it spawns a new one.
//!
//! Single-instance coordination uses an internal mutex on a small
//! `Inner` struct that holds the active child's pid + a oneshot kill
//! channel + a generation counter. Force-kill / on_turn_end (1→0
//! transition) bumps the generation so a stale watcher wake-up cannot
//! resurrect a child the controller has already retired.
//!
//! All clean-shutdown paths converge on `kill_on_drop(true)` set on
//! the spawned `tokio::process::Child`: when the watcher task is
//! dropped (controller goes out of scope, runtime tears down), the
//! Child drop sends SIGKILL to caffeinate and reaps the zombie.

#![cfg(target_os = "macos")]

use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};

use serde::Serialize;
use tokio::process::Command;
use tokio::runtime::Handle;
use tokio::sync::oneshot;
use zenui_runtime_core::TurnLifecycleObserver;

use crate::user_config::UserConfigStore;

/// User-facing config key. Stored as `"true"` / `"false"` to match the
/// rest of `defaults-settings.ts` (every existing toggle uses the
/// same encoding).
pub const CONFIG_KEY: &str = "system.caffeinate";

/// Hard upper bound on how long any single caffeinate invocation runs.
/// Acts as a safety net for crash-induced orphans (see module comment).
/// Five minutes is short enough that a forgotten process is bounded but
/// long enough that respawn churn is negligible during real long turns.
const SAFETY_TIMEOUT_SECS: u64 = 300;

/// Wire-format snapshot of caffeinate state for the settings UI.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CaffeinateStatus {
    pub enabled: bool,
    pub running: bool,
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
    /// `TurnLifecycleObserver` trait methods are sync, but we need to
    /// `runtime.spawn(...)` the watcher task that awaits child exit.
    runtime: Handle,
    /// Self-weak — installed via `Arc::new_cyclic` so watcher tasks
    /// hold a `Weak<Self>` and don't keep the controller alive.
    self_weak: Weak<CaffeinateController>,
}

struct Inner {
    /// PID of the currently running caffeinate child, if any. The
    /// `Child` itself is owned by the spawned watcher task; we keep
    /// just the pid here so `status()` can report it without
    /// borrowing into the task.
    pid: Option<u32>,
    /// Sender to ask the watcher to kill its child and exit early.
    /// `Some` while a child is alive (and a watcher owns it),
    /// `None` otherwise. Taking it (or bumping `generation`) is the
    /// only way to retire the current watcher.
    kill_tx: Option<oneshot::Sender<()>>,
    /// Bumped on every spawn AND every retirement. The watcher
    /// captures its generation at spawn time and hands it back via
    /// [`CaffeinateController::on_watcher_exit`]; if the value no
    /// longer matches, the watcher's wake-up is stale and we
    /// ignore it. Prevents a "kill + immediate respawn" race from
    /// being undone by the old watcher's natural-exit handler.
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

    /// User clicked "Force kill" in settings. Kills the running
    /// caffeinate immediately. The setting itself stays on; the
    /// `user_killed` flag suppresses respawn until the in-flight
    /// counter returns to 0, at which point a fresh turn will spawn
    /// caffeinate normally.
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
        if self.in_flight.load(Ordering::SeqCst) > 0
            && !self.user_killed.load(Ordering::SeqCst)
        {
            self.spawn_locked(&mut inner);
        }
    }

    /// Spawn a fresh caffeinate child if and only if none is running.
    /// Called with the inner mutex held.
    fn spawn_locked(&self, inner: &mut Inner) {
        if inner.kill_tx.is_some() {
            return; // already running
        }
        let mut cmd = Command::new("caffeinate");
        cmd.arg("-d") // prevent display sleep
            .arg("-t")
            .arg(SAFETY_TIMEOUT_SECS.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            // If the watcher task is dropped (e.g. runtime shutdown
            // after a panic), the Child's Drop sends SIGKILL — our
            // last line of defense against orphaned caffeinate
            // processes on clean tear-down paths.
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
            // reached, or someone external killed it) OR we receive
            // a retirement signal and SIGTERM the child ourselves.
            let killed_by_us = tokio::select! {
                wait_res = child.wait() => {
                    if let Err(err) = wait_res {
                        tracing::warn!(%err, "caffeinate wait failed");
                    }
                    false
                }
                _ = rx => {
                    // The Sender side either sent `()` or was dropped.
                    // Both mean "retire this watcher". Kill + reap.
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

    /// Tear down the current watcher (if any). Safe to call when no
    /// watcher exists. Called with the inner mutex held. Bumps
    /// `generation` so the watcher's eventual `on_watcher_exit`
    /// callback is recognised as stale and ignored.
    fn retire_locked(&self, inner: &mut Inner) {
        if let Some(tx) = inner.kill_tx.take() {
            // Watcher will receive the signal (or notice the channel
            // closed) and clean up its child. Send may fail if the
            // watcher already exited concurrently — that's fine.
            let _ = tx.send(());
        }
        inner.pid = None;
        inner.generation = inner.generation.wrapping_add(1);
    }

    /// Watcher callback. Called once the spawned task's child has
    /// exited (and been reaped) for any reason. We may need to
    /// respawn for the natural-timeout case if turns are still in
    /// flight.
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
        // canonical "no child running" state. Bumping would only
        // matter if a stale callback might race; but we already
        // proved no newer one exists by the equality check above.

        if killed_by_us {
            // Retirement was requested explicitly (force_kill,
            // on_turn_end 1→0, refresh-while-disabled, or Drop).
            // Don't second-guess the caller.
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
        // The next 0→1 transition will spawn caffeinate normally.
        self.user_killed.store(false, Ordering::SeqCst);
    }
}

impl Drop for CaffeinateController {
    fn drop(&mut self) {
        // Tell any live watcher to kill its child and exit. The
        // watcher's `Child` has `kill_on_drop(true)` so even if the
        // tokio runtime is tearing down (and the watcher task is
        // dropped before it can process this signal), the Child's
        // Drop sends SIGKILL. Either path leaves no orphaned
        // caffeinate process behind.
        if let Ok(mut inner) = self.inner.lock() {
            if let Some(tx) = inner.kill_tx.take() {
                let _ = tx.send(());
            }
            inner.pid = None;
        }
    }
}
