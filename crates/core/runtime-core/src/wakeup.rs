//! Scheduler for Claude Code's `ScheduleWakeup` tool.
//!
//! Unlike a typical flowstate `RuntimeCall`, wakeups are not initiated
//! from flowstate's MCP surface. Claude Code (the CLI spawned inside
//! the SDK bridge) ships `ScheduleWakeup` as a built-in harness tool,
//! and the model calls it directly. Flowstate's job is to OBSERVE
//! those tool calls in the streaming output and shoulder the fire
//! path ourselves, because Claude Code's internal timer dies the
//! moment flowstate's [`ProcessCache`](zenui_provider_api::ProcessCache)
//! reaps the bridge (which happens 30 minutes after the last turn
//! ends).
//!
//! ## Flow
//!
//! 1. The adapter's streaming events emit a `ToolCallStarted` for
//!    `name == "ScheduleWakeup"`.
//! 2. `RuntimeCore::observe_schedule_wakeup_tool_call` is invoked in
//!    the drain loop; it parses `delaySeconds` + `prompt` + `reason`
//!    from the tool args, persists a row via
//!    [`zenui_persistence::PersistenceService::insert_wakeup`], and
//!    arms the scheduler.
//! 3. When the tokio timer fires, [`WakeupScheduler`] marks the row
//!    `fired`, publishes `RuntimeEvent::WakeupFired`, and calls
//!    [`crate::orchestration::spawn_peer_turn`] with the observed
//!    prompt on the same session — self-delivery. The respawned
//!    bridge resumes Claude Code via `native_thread_id`, and the
//!    model receives the prompt as a normal user turn.
//!
//! The persisted row survives restart: on daemon boot,
//! [`WakeupScheduler::reload_pending`] re-arms every `pending` row,
//! and any whose `fire_at` has already passed fire on the next
//! scheduler tick.
//!
//! ## Design choices
//!
//! - **Single task + min-heap + Notify.** One background task owns a
//!   `BinaryHeap<(Instant, wakeup_id, session_id, prompt)>`. A
//!   [`tokio::sync::Notify`] wakes it on arm/cancel so a new sooner-
//!   due wakeup can preempt an in-progress sleep. Scales to hundreds
//!   of rows without spawning a task per wakeup; cancel is trivial.
//!
//! - **Tokio `Instant` keys, not wall-clock.** Heap entries are keyed
//!   by `tokio::time::Instant` so `tokio::time::pause()`-based tests
//!   can fast-forward with `tokio::time::advance`. Wall-clock
//!   `fire_at_unix` is what we persist so restarts survive, and we
//!   convert at arm time via `Instant::now() + (fire_at_unix -
//!   now_unix).max(0)`.
//!
//! - **Claude Code may double-fire on short delays.** If `delaySeconds
//!   < BRIDGE_IDLE_TIMEOUT_SECS` (now 30 minutes), Claude Code's
//!   in-CLI timer will fire before our `ProcessCache` reaps the
//!   bridge. That produces assistant output the Rust side doesn't
//!   read (no active `run_turn` after the turn that scheduled the
//!   wakeup ended), so it sits in the stdout pipe buffer. Callers
//!   that want to avoid the deadlock invalidate the bridge at the end
//!   of the turn that observed the wakeup — see the observe-hook
//!   wiring in `runtime-core/src/lib.rs`.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::sync::{Arc, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;
use tokio::sync::{Mutex, Notify, mpsc};
use tokio::time::{Instant, sleep_until};
use zenui_persistence::{PersistenceService, ScheduledWakeupRow};
use zenui_provider_api::{PermissionMode, RuntimeEvent};

use crate::RuntimeCore;

/// Matches the Claude Code tool spec documented at
/// https://code.claude.com (the runtime clamps to this range
/// regardless of what the agent supplied).
pub const WAKEUP_MIN_DELAY_SECS: u64 = 60;
pub const WAKEUP_MAX_DELAY_SECS: u64 = 3600;

/// Hard cap on pending wakeups per session — a runaway agent that
/// schedules in a tight loop is bounded before it can fill the table.
pub const WAKEUP_MAX_PENDING_PER_SESSION: i64 = 32;

/// Clamp `delay_secs` into the spec's supported range. Applied
/// defensively; Claude Code enforces this too but we don't trust the
/// tool-call args to be clean.
pub fn clamp_wakeup_delay(delay_secs: u64) -> u64 {
    delay_secs.clamp(WAKEUP_MIN_DELAY_SECS, WAKEUP_MAX_DELAY_SECS)
}

/// Best-effort "seconds since Unix epoch" — saturating to 0 on the
/// theoretical case of a system clock before 1970.
pub fn now_unix_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Parse Claude Code's `ScheduleWakeup` tool arguments into a
/// normalized `(delay_secs, prompt, reason)` tuple. Accepts both
/// camelCase (Claude Code's spec: `delaySeconds`) and snake_case
/// (`delay_seconds`) for resilience. Returns `None` when required
/// fields are missing so the observer can skip malformed calls
/// without crashing the drain loop.
pub fn parse_schedule_wakeup_args(args: &Value) -> Option<(u64, String, Option<String>)> {
    let obj = args.as_object()?;
    let delay_secs = obj
        .get("delaySeconds")
        .or_else(|| obj.get("delay_seconds"))
        .or_else(|| obj.get("delay_secs"))
        .and_then(Value::as_u64)?;
    let prompt = obj.get("prompt").and_then(Value::as_str)?.to_string();
    if prompt.trim().is_empty() {
        return None;
    }
    let reason = obj
        .get("reason")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string);
    Some((delay_secs, prompt, reason))
}

/// A single armed wakeup in the heap. Ordered by `deadline` ascending
/// via `Reverse` so `BinaryHeap::peek` gives the next due.
#[derive(Debug, Clone, Eq, PartialEq)]
struct HeapEntry {
    deadline: Instant,
    wakeup_id: String,
    session_id: String,
    prompt: String,
}

impl Ord for HeapEntry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.deadline
            .cmp(&other.deadline)
            .then_with(|| self.wakeup_id.cmp(&other.wakeup_id))
    }
}
impl PartialOrd for HeapEntry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl HeapEntry {
    fn from_row(row: ScheduledWakeupRow) -> Self {
        let delta = row.fire_at_unix.saturating_sub(now_unix_secs()).max(0) as u64;
        Self {
            deadline: Instant::now() + Duration::from_secs(delta),
            wakeup_id: row.wakeup_id,
            session_id: row.session_id,
            prompt: row.prompt,
        }
    }
}

#[derive(Debug)]
enum SchedulerCmd {
    Arm(HeapEntry),
    Cancel(String),
}

/// Payload handed to the [`WakeupFireHandler`] when a timer pops.
#[derive(Debug, Clone)]
pub struct FiredWakeup {
    pub wakeup_id: String,
    pub session_id: String,
    pub prompt: String,
}

/// Lazy fire-path hook. `WakeupScheduler` calls this when a timer
/// pops; the production impl publishes `WakeupFired` and dispatches
/// the turn. Tests inject stubs that record fires without a full
/// `RuntimeCore`.
#[async_trait::async_trait]
pub trait WakeupFireHandler: Send + Sync + 'static {
    async fn on_wakeup_fired(&self, fired: FiredWakeup);
}

/// Production fire handler. Publishes `RuntimeEvent::WakeupFired` and
/// calls [`crate::orchestration::spawn_peer_turn`] with self-delivery
/// semantics so the prompt lands as a user turn on the originating
/// session.
pub struct RuntimeCoreFireHandler {
    pub runtime: Weak<RuntimeCore>,
}

#[async_trait::async_trait]
impl WakeupFireHandler for RuntimeCoreFireHandler {
    async fn on_wakeup_fired(&self, fired: FiredWakeup) {
        let Some(rc) = self.runtime.upgrade() else {
            tracing::warn!(
                session_id = %fired.session_id,
                wakeup_id = %fired.wakeup_id,
                "wakeup fired but RuntimeCore is gone; dropping"
            );
            return;
        };
        publish_wakeup_fired(&rc, &fired.session_id, &fired.wakeup_id);
        // Wakeups re-deliver a prompt to the same session — they
        // don't carry mode/effort overrides, so run the turn with
        // the session's strictest permission mode and no effort
        // override (historical behavior pre-spawn-config refactor).
        crate::orchestration::spawn_peer_turn(
            rc,
            fired.session_id,
            fired.prompt,
            "wakeup",
            PermissionMode::Default,
            None,
        );
    }
}

/// Handle to the scheduler task. Cheap to clone; hands off every
/// operation to the owning task via mpsc.
#[derive(Clone)]
pub struct WakeupScheduler {
    tx: mpsc::UnboundedSender<SchedulerCmd>,
}

impl WakeupScheduler {
    pub fn spawn(
        persistence: Arc<PersistenceService>,
        fire_handler: Arc<dyn WakeupFireHandler>,
    ) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let inner = Arc::new(SchedulerInner {
            heap: Mutex::new(BinaryHeap::new()),
            wake_notify: Notify::new(),
            persistence,
            fire_handler,
        });

        let task_inner = Arc::clone(&inner);
        tokio::spawn(async move {
            scheduler_loop(task_inner, rx).await;
        });

        Self { tx }
    }

    /// Arm a freshly-persisted wakeup. The caller has already written
    /// the row; this just gets a timer ticking.
    pub fn arm(&self, row: &ScheduledWakeupRow) {
        let _ = self
            .tx
            .send(SchedulerCmd::Arm(HeapEntry::from_row(row.clone())));
    }

    /// Cancel a previously-armed wakeup. Safe for unknown ids (no-op).
    pub fn cancel(&self, wakeup_id: &str) {
        let _ = self.tx.send(SchedulerCmd::Cancel(wakeup_id.to_string()));
    }

    /// Load every `pending` row from persistence into the heap. Any
    /// row whose `fire_at_unix` already passed fires on the next loop
    /// tick. Call once from `RuntimeCore::init_wakeup_scheduler`.
    pub async fn reload_pending(&self, persistence: &PersistenceService) {
        let rows = persistence.list_pending_wakeups().await;
        let count = rows.len();
        for row in rows {
            let _ = self.tx.send(SchedulerCmd::Arm(HeapEntry::from_row(row)));
        }
        if count > 0 {
            tracing::info!(count, "rehydrated pending wakeups from persistence");
        }
    }
}

struct SchedulerInner {
    heap: Mutex<BinaryHeap<Reverse<HeapEntry>>>,
    wake_notify: Notify,
    persistence: Arc<PersistenceService>,
    fire_handler: Arc<dyn WakeupFireHandler>,
}

async fn scheduler_loop(inner: Arc<SchedulerInner>, mut rx: mpsc::UnboundedReceiver<SchedulerCmd>) {
    let mut cancelled_ids = std::collections::HashSet::<String>::new();

    loop {
        let next_deadline: Option<Instant> = {
            let heap = inner.heap.lock().await;
            heap.peek().map(|Reverse(entry)| entry.deadline)
        };

        tokio::select! {
            maybe_cmd = rx.recv() => {
                match maybe_cmd {
                    Some(SchedulerCmd::Arm(entry)) => {
                        cancelled_ids.remove(&entry.wakeup_id);
                        let mut heap = inner.heap.lock().await;
                        heap.push(Reverse(entry));
                        drop(heap);
                        inner.wake_notify.notify_one();
                    }
                    Some(SchedulerCmd::Cancel(id)) => {
                        cancelled_ids.insert(id);
                        inner.wake_notify.notify_one();
                    }
                    None => return,
                }
            }
            _ = async {
                match next_deadline {
                    Some(deadline) => sleep_until(deadline).await,
                    None => inner.wake_notify.notified().await,
                }
            } => {
                loop {
                    let due = {
                        let mut heap = inner.heap.lock().await;
                        let now = Instant::now();
                        match heap.peek() {
                            Some(Reverse(e)) if e.deadline <= now => {
                                heap.pop().map(|Reverse(e)| e)
                            }
                            _ => None,
                        }
                    };
                    let Some(entry) = due else { break };
                    if cancelled_ids.remove(&entry.wakeup_id) {
                        continue;
                    }
                    let flipped = inner
                        .persistence
                        .mark_wakeup_fired(&entry.wakeup_id)
                        .await;
                    if !flipped {
                        continue;
                    }
                    inner
                        .fire_handler
                        .on_wakeup_fired(FiredWakeup {
                            wakeup_id: entry.wakeup_id.clone(),
                            session_id: entry.session_id.clone(),
                            prompt: entry.prompt.clone(),
                        })
                        .await;
                }
            }
        }
    }
}

/// Publish helper for `RuntimeEvent::WakeupScheduled`. Kept next to
/// its sibling so future fields stay local.
pub fn publish_wakeup_scheduled(
    rc: &RuntimeCore,
    session_id: &str,
    wakeup_id: &str,
    fire_at_unix: i64,
    reason: Option<&str>,
) {
    rc.publish(RuntimeEvent::WakeupScheduled {
        session_id: session_id.to_string(),
        wakeup_id: wakeup_id.to_string(),
        fire_at_unix,
        reason: reason.map(str::to_string),
    });
}

pub fn publish_wakeup_fired(rc: &RuntimeCore, session_id: &str, wakeup_id: &str) {
    rc.publish(RuntimeEvent::WakeupFired {
        session_id: session_id.to_string(),
        wakeup_id: wakeup_id.to_string(),
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Mutex as StdMutex;
    use zenui_persistence::ScheduledWakeupStatus;

    #[derive(Default)]
    struct RecordingHandler {
        fires: StdMutex<Vec<FiredWakeup>>,
    }
    impl RecordingHandler {
        fn fires(&self) -> Vec<FiredWakeup> {
            self.fires.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl WakeupFireHandler for RecordingHandler {
        async fn on_wakeup_fired(&self, fired: FiredWakeup) {
            self.fires.lock().unwrap().push(fired);
        }
    }

    fn seed_session(persistence: &PersistenceService, session_id: &str) {
        persistence
            .insert_session_row_for_tests(session_id)
            .expect("seed session");
    }

    fn row(wakeup_id: &str, session_id: &str, fire_at: i64) -> ScheduledWakeupRow {
        ScheduledWakeupRow {
            wakeup_id: wakeup_id.to_string(),
            session_id: session_id.to_string(),
            origin_turn_id: Some("t-1".to_string()),
            fire_at_unix: fire_at,
            prompt: format!("wake {wakeup_id}"),
            reason: None,
            status: ScheduledWakeupStatus::Pending,
            created_at_unix: 0,
        }
    }

    #[test]
    fn clamp_applies_bounds() {
        assert_eq!(clamp_wakeup_delay(10), WAKEUP_MIN_DELAY_SECS);
        assert_eq!(clamp_wakeup_delay(5_000), WAKEUP_MAX_DELAY_SECS);
        assert_eq!(clamp_wakeup_delay(600), 600);
    }

    #[test]
    fn parse_args_extracts_camel_case() {
        let args = json!({
            "delaySeconds": 300,
            "prompt": "poll the build",
            "reason": "loop probe",
        });
        let (delay, prompt, reason) = parse_schedule_wakeup_args(&args).unwrap();
        assert_eq!(delay, 300);
        assert_eq!(prompt, "poll the build");
        assert_eq!(reason.as_deref(), Some("loop probe"));
    }

    #[test]
    fn parse_args_rejects_empty_prompt() {
        let args = json!({ "delaySeconds": 120, "prompt": "   " });
        assert!(parse_schedule_wakeup_args(&args).is_none());
    }

    #[test]
    fn parse_args_rejects_missing_delay() {
        let args = json!({ "prompt": "x" });
        assert!(parse_schedule_wakeup_args(&args).is_none());
    }

    #[test]
    fn parse_args_accepts_snake_case_fallback() {
        // Defensive: some harness variants might serialise as snake.
        let args = json!({ "delay_seconds": 120, "prompt": "x" });
        let (delay, prompt, _) = parse_schedule_wakeup_args(&args).unwrap();
        assert_eq!(delay, 120);
        assert_eq!(prompt, "x");
    }

    #[tokio::test(start_paused = true)]
    async fn reload_fires_past_due_and_future_in_order() {
        let persistence = Arc::new(PersistenceService::in_memory().unwrap());
        seed_session(&persistence, "s-1");
        let now = now_unix_secs();
        persistence
            .insert_wakeup(row("w-past", "s-1", now - 100))
            .await
            .unwrap();
        persistence
            .insert_wakeup(row("w-near", "s-1", now + 60))
            .await
            .unwrap();
        persistence
            .insert_wakeup(row("w-far", "s-1", now + 600))
            .await
            .unwrap();

        let handler = Arc::new(RecordingHandler::default());
        let scheduler = WakeupScheduler::spawn(
            Arc::clone(&persistence),
            Arc::clone(&handler) as Arc<dyn WakeupFireHandler>,
        );
        scheduler.reload_pending(&persistence).await;

        tokio::time::advance(Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(70)).await;
        tokio::task::yield_now().await;

        let fires = handler.fires();
        assert_eq!(fires.len(), 2, "expected past + near, got {fires:?}");
        assert_eq!(fires[0].wakeup_id, "w-past");
        assert_eq!(fires[0].prompt, "wake w-past");
        assert_eq!(fires[1].wakeup_id, "w-near");
    }

    #[tokio::test(start_paused = true)]
    async fn arm_preempts_sleep_for_earlier_wakeup() {
        let persistence = Arc::new(PersistenceService::in_memory().unwrap());
        seed_session(&persistence, "s-1");
        let handler = Arc::new(RecordingHandler::default());
        let scheduler = WakeupScheduler::spawn(
            Arc::clone(&persistence),
            Arc::clone(&handler) as Arc<dyn WakeupFireHandler>,
        );

        let now = now_unix_secs();
        persistence
            .insert_wakeup(row("w-far", "s-1", now + 600))
            .await
            .unwrap();
        scheduler.arm(&persistence.list_pending_wakeups().await[0]);
        tokio::task::yield_now().await;

        persistence
            .insert_wakeup(row("w-near", "s-1", now + 70))
            .await
            .unwrap();
        let near = persistence
            .list_pending_wakeups()
            .await
            .into_iter()
            .find(|r| r.wakeup_id == "w-near")
            .unwrap();
        scheduler.arm(&near);

        tokio::time::advance(Duration::from_secs(80)).await;
        tokio::task::yield_now().await;

        let fires = handler.fires();
        assert_eq!(fires.len(), 1);
        assert_eq!(fires[0].prompt, "wake w-near");
    }
}
