//! Scheduler for Claude Code's `CronCreate` tool.
//!
//! Sibling of [`crate::wakeup`] — same observe-and-own contract, but
//! for **recurring** schedules. The model emits `CronCreate` (via
//! `/loop <interval> ...` or `/schedule ...`) and Claude Code's harness
//! schedules the cron internally; flowstate doesn't trust that timer
//! because:
//!
//! - It dies along with the bridge subprocess when `ProcessCache` reaps
//!   the process after 30 minutes of idle.
//! - Even when the bridge is alive, fired-cron output would land on a
//!   stdout pipe nobody is reading (no active `run_turn`).
//!
//! So we OBSERVE the tool call in the streaming output, persist a row,
//! and own the fire path with a tokio timer that survives bridge reaps.
//!
//! ## Flow
//!
//! 1. The adapter's streaming events emit a `ToolCallStarted` for
//!    `name == "CronCreate"`.
//! 2. `RuntimeCore::observe_cron_create_tool_call` is invoked in the
//!    drain loop; it parses `cron` + `prompt` + `reason` from the tool
//!    args, persists a row via [`PersistenceService::insert_cron`], and
//!    arms the scheduler.
//! 3. When the tokio timer fires, [`CronScheduler`] stamps
//!    `last_fired_at_unix`, publishes `RuntimeEvent::CronFired`, calls
//!    [`crate::orchestration::spawn_peer_turn`] with the prompt, and
//!    **re-arms** itself by computing the next fire instant from the
//!    same cron expression. (Wakeups are one-shot; crons aren't.)
//!
//! ## Differences from [`crate::wakeup`]
//!
//! - **No "fired" terminal status.** Cron rows are `Active` until
//!   explicitly cancelled. Each tick stamps `last_fired_at_unix` and
//!   re-pushes a fresh heap entry computed from `cron_expr`.
//! - **No bridge invalidation at turn-end.** Wakeups invalidate so
//!   Claude Code's in-CLI timer can't double-fire; for crons the
//!   recurring nature would force a cold-start respawn on every fire,
//!   which is too expensive. We rely on `spawn_peer_turn`'s lazy
//!   bridge re-spawn at fire time instead.
//! - **`cron` crate parsing.** Cron expressions can be 5-, 6-, or
//!   7-field. We try as-given first, then prepend `"0 "` (assume the
//!   5-field POSIX form, fire at second 0) for resilience.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::str::FromStr;
use std::sync::{Arc, Weak};
use std::time::Duration;

use chrono::{TimeZone, Utc};
use serde_json::Value;
use tokio::sync::{Mutex, Notify, mpsc};
use tokio::time::{Instant, sleep_until};
use zenui_persistence::{PersistenceService, ScheduledCronRow};
use zenui_provider_api::{PermissionMode, RuntimeEvent, TurnSource};

use crate::RuntimeCore;
use crate::wakeup::now_unix_secs;

/// Hard cap on active crons per session — a runaway agent that
/// schedules in a tight loop is bounded before it can fill the table.
pub const CRON_MAX_ACTIVE_PER_SESSION: i64 = 16;

/// Floor for "next fire from now" sleeps. The `cron` crate can
/// occasionally return a `next` whose `unix_secs == now_unix_secs()`
/// when the tick is in-progress — sleeping zero would re-fire in a
/// tight loop. One second is the natural granularity for cron and
/// also matches the behavior of crond.
const CRON_MIN_REARM_DELAY_SECS: u64 = 1;

/// Parse a cron expression as-given, falling back to a 5-field
/// interpretation by prepending `"0 "` (fire at second 0). Returns
/// `None` for unparseable input — the observer logs and drops the
/// row rather than crashing the drain loop.
pub fn parse_cron_expr(expr: &str) -> Option<cron::Schedule> {
    let trimmed = expr.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(s) = cron::Schedule::from_str(trimmed) {
        return Some(s);
    }
    // POSIX 5-field fallback. `"*/5 * * * *"` -> `"0 */5 * * * *"`.
    let with_seconds = format!("0 {trimmed}");
    cron::Schedule::from_str(&with_seconds).ok()
}

/// Compute the next fire instant strictly after `after_unix` (a
/// unix-seconds reference, typically `last_fired_at_unix` or
/// `now_unix_secs()`). Returns `None` if the schedule has no future
/// matches (e.g. a one-off "fire only in 1999"). Tested at fixed
/// virtual times via the wrapping helpers in tests.
pub fn next_fire_after_unix(schedule: &cron::Schedule, after_unix: i64) -> Option<i64> {
    let after = Utc
        .timestamp_opt(after_unix.max(0), 0)
        .single()
        .unwrap_or_else(Utc::now);
    schedule
        .after(&after)
        .next()
        .map(|dt| dt.timestamp())
}

/// Parse Claude Code's `CronCreate` tool arguments into a normalised
/// `(cron_expr, prompt, reason)` tuple. Field names match the tool
/// spec; we accept both camelCase (`cronExpression`, `cron`) and
/// snake_case (`cron_expression`) for resilience because the SDK has
/// shifted naming conventions before. Returns `None` when required
/// fields are missing.
pub fn parse_cron_create_args(args: &Value) -> Option<(String, String, Option<String>)> {
    let obj = args.as_object()?;
    let cron_expr = obj
        .get("cron")
        .or_else(|| obj.get("cronExpression"))
        .or_else(|| obj.get("cron_expression"))
        .or_else(|| obj.get("schedule"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())?
        .to_string();
    let prompt = obj
        .get("prompt")
        .and_then(Value::as_str)?
        .to_string();
    if prompt.trim().is_empty() {
        return None;
    }
    let reason = obj
        .get("reason")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string);
    // Validate the expression up front — returning `None` here lets
    // the observer log + skip without ever persisting an unfireable
    // row.
    parse_cron_expr(&cron_expr)?;
    Some((cron_expr, prompt, reason))
}

/// Parse a `CronDelete` tool call's `cron_id` argument. Same
/// camel/snake fallback policy as `parse_cron_create_args`.
pub fn parse_cron_delete_args(args: &Value) -> Option<String> {
    let obj = args.as_object()?;
    obj.get("cronId")
        .or_else(|| obj.get("cron_id"))
        .or_else(|| obj.get("id"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// A single armed cron in the heap. Ordered by `deadline` ascending
/// via `Reverse` so `BinaryHeap::peek` gives the next due. We carry
/// the parsed `Schedule` so the post-fire re-arm doesn't pay another
/// parse cost (and stays consistent with the original args even if the
/// row's text representation is later edited externally).
#[derive(Debug, Clone)]
struct HeapEntry {
    deadline: Instant,
    cron_id: String,
    session_id: String,
    prompt: String,
    /// Pre-parsed schedule. The string form lives on the persisted
    /// row; the heap doesn't need it because re-arm uses this parsed
    /// `Schedule` directly.
    schedule: cron::Schedule,
}

impl PartialEq for HeapEntry {
    fn eq(&self, other: &Self) -> bool {
        self.deadline == other.deadline && self.cron_id == other.cron_id
    }
}
impl Eq for HeapEntry {}
impl Ord for HeapEntry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.deadline
            .cmp(&other.deadline)
            .then_with(|| self.cron_id.cmp(&other.cron_id))
    }
}
impl PartialOrd for HeapEntry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl HeapEntry {
    /// Build the next heap entry for a row, computing its `deadline`
    /// from `cron_expr` strictly after `after_unix`. Returns `None` if
    /// the expression can't parse or has no future fire.
    fn next_for_row(row: &ScheduledCronRow, after_unix: i64) -> Option<Self> {
        let schedule = parse_cron_expr(&row.cron_expr)?;
        let next_unix = next_fire_after_unix(&schedule, after_unix)?;
        let now_unix = now_unix_secs();
        let raw_delta = (next_unix - now_unix).max(0) as u64;
        let delta = raw_delta.max(CRON_MIN_REARM_DELAY_SECS);
        Some(Self {
            deadline: Instant::now() + Duration::from_secs(delta),
            cron_id: row.cron_id.clone(),
            session_id: row.session_id.clone(),
            prompt: row.prompt.clone(),
            schedule,
        })
    }

    /// Re-arm helper used after a fire. Same as `next_for_row` but
    /// doesn't re-parse the expression.
    fn rearm(self, after_unix: i64) -> Option<Self> {
        let next_unix = next_fire_after_unix(&self.schedule, after_unix)?;
        let now_unix = now_unix_secs();
        let raw_delta = (next_unix - now_unix).max(0) as u64;
        let delta = raw_delta.max(CRON_MIN_REARM_DELAY_SECS);
        Some(Self {
            deadline: Instant::now() + Duration::from_secs(delta),
            ..self
        })
    }
}

#[derive(Debug)]
enum SchedulerCmd {
    Arm(HeapEntry),
    Cancel(String),
}

/// Payload handed to the [`CronFireHandler`] when a timer pops.
#[derive(Debug, Clone)]
pub struct FiredCron {
    pub cron_id: String,
    pub session_id: String,
    pub prompt: String,
    pub fire_at_unix: i64,
}

/// Lazy fire-path hook. `CronScheduler` calls this when a timer pops;
/// the production impl publishes `CronFired` and dispatches the turn.
/// Tests inject stubs that record fires without a full `RuntimeCore`.
#[async_trait::async_trait]
pub trait CronFireHandler: Send + Sync + 'static {
    async fn on_cron_fired(&self, fired: FiredCron);
}

/// Production fire handler. Publishes `RuntimeEvent::CronFired` and
/// calls [`crate::orchestration::spawn_peer_turn`] with self-delivery
/// semantics so the prompt lands as a user turn on the originating
/// session.
pub struct RuntimeCoreFireHandler {
    pub runtime: Weak<RuntimeCore>,
}

#[async_trait::async_trait]
impl CronFireHandler for RuntimeCoreFireHandler {
    async fn on_cron_fired(&self, fired: FiredCron) {
        let Some(rc) = self.runtime.upgrade() else {
            tracing::warn!(
                session_id = %fired.session_id,
                cron_id = %fired.cron_id,
                "cron fired but RuntimeCore is gone; dropping"
            );
            return;
        };
        publish_cron_fired(&rc, &fired.session_id, &fired.cron_id, fired.fire_at_unix);
        // Same self-delivery shape as wakeups (`wakeup.rs`): no
        // mode/effort overrides, the session's own permissions and
        // model apply.
        crate::orchestration::spawn_peer_turn(
            rc,
            fired.session_id,
            fired.prompt,
            TurnSource::Cron,
            PermissionMode::Default,
            None,
        );
    }
}

/// Handle to the scheduler task. Cheap to clone; hands off every
/// operation to the owning task via mpsc.
#[derive(Clone)]
pub struct CronScheduler {
    tx: mpsc::UnboundedSender<SchedulerCmd>,
}

impl CronScheduler {
    pub fn spawn(
        persistence: Arc<PersistenceService>,
        fire_handler: Arc<dyn CronFireHandler>,
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

    /// Arm a freshly-persisted cron. The caller has already written
    /// the row; this just gets a timer ticking. Skips silently if
    /// the cron expression has no future fire.
    pub fn arm(&self, row: &ScheduledCronRow) {
        let after = row.last_fired_at_unix.unwrap_or_else(now_unix_secs);
        if let Some(entry) = HeapEntry::next_for_row(row, after) {
            let _ = self.tx.send(SchedulerCmd::Arm(entry));
        } else {
            tracing::warn!(
                cron_id = %row.cron_id,
                cron_expr = %row.cron_expr,
                "cron has no future fire; not arming"
            );
        }
    }

    /// Cancel a previously-armed cron. Safe for unknown ids (no-op).
    pub fn cancel(&self, cron_id: &str) {
        let _ = self.tx.send(SchedulerCmd::Cancel(cron_id.to_string()));
    }

    /// Load every `active` row from persistence into the heap. Each
    /// row's next fire is computed from the cron expression after the
    /// row's `last_fired_at_unix` (or now, if it never fired). Call
    /// once from `RuntimeCore::init_cron_scheduler`.
    pub async fn reload_active(&self, persistence: &PersistenceService) {
        let rows = persistence.list_active_crons().await;
        let mut count = 0usize;
        for row in rows {
            let after = row.last_fired_at_unix.unwrap_or_else(now_unix_secs);
            if let Some(entry) = HeapEntry::next_for_row(&row, after) {
                let _ = self.tx.send(SchedulerCmd::Arm(entry));
                count += 1;
            } else {
                tracing::warn!(
                    cron_id = %row.cron_id,
                    cron_expr = %row.cron_expr,
                    "rehydrated cron has no future fire; skipping"
                );
            }
        }
        if count > 0 {
            tracing::info!(count, "rehydrated active crons from persistence");
        }
    }
}

struct SchedulerInner {
    heap: Mutex<BinaryHeap<Reverse<HeapEntry>>>,
    wake_notify: Notify,
    persistence: Arc<PersistenceService>,
    fire_handler: Arc<dyn CronFireHandler>,
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
                        // A fresh arm clears any prior cancel — the
                        // operator may legitimately resurrect an id
                        // by deleting and immediately re-creating
                        // the same schedule.
                        cancelled_ids.remove(&entry.cron_id);
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
                    if cancelled_ids.remove(&entry.cron_id) {
                        // Drop the entry without re-arming; CronDelete
                        // is terminal for this id.
                        continue;
                    }
                    let fire_at_unix = now_unix_secs();
                    let flipped = inner
                        .persistence
                        .mark_cron_fired(&entry.cron_id, fire_at_unix)
                        .await;
                    if !flipped {
                        // Row was cancelled out from under us (or the
                        // session was deleted, FK-cascading the row
                        // away). Don't dispatch and don't re-arm.
                        continue;
                    }
                    inner
                        .fire_handler
                        .on_cron_fired(FiredCron {
                            cron_id: entry.cron_id.clone(),
                            session_id: entry.session_id.clone(),
                            prompt: entry.prompt.clone(),
                            fire_at_unix,
                        })
                        .await;
                    // Re-arm strictly AFTER fire_at_unix so a
                    // sub-second tick (e.g. `* * * * * *`) doesn't
                    // double-fire on the same second. Clone the id
                    // for the no-future log path because `rearm`
                    // consumes `self` — rare branch, cheap clone.
                    let log_cron_id = entry.cron_id.clone();
                    if let Some(next_entry) = entry.rearm(fire_at_unix) {
                        let mut heap = inner.heap.lock().await;
                        heap.push(Reverse(next_entry));
                        drop(heap);
                        inner.wake_notify.notify_one();
                    } else {
                        tracing::info!(
                            cron_id = %log_cron_id,
                            "cron has no further fires after this tick; not re-arming"
                        );
                    }
                }
            }
        }
    }
}

/// Publish helper for `RuntimeEvent::CronScheduled`. Kept next to its
/// sibling so future fields stay local.
pub fn publish_cron_scheduled(
    rc: &RuntimeCore,
    session_id: &str,
    cron_id: &str,
    cron_expr: &str,
    reason: Option<&str>,
) {
    rc.publish(RuntimeEvent::CronScheduled {
        session_id: session_id.to_string(),
        cron_id: cron_id.to_string(),
        cron_expr: cron_expr.to_string(),
        reason: reason.map(str::to_string),
    });
}

pub fn publish_cron_fired(rc: &RuntimeCore, session_id: &str, cron_id: &str, fire_at_unix: i64) {
    rc.publish(RuntimeEvent::CronFired {
        session_id: session_id.to_string(),
        cron_id: cron_id.to_string(),
        fire_at_unix,
    });
}

pub fn publish_cron_cancelled(rc: &RuntimeCore, session_id: &str, cron_id: &str) {
    rc.publish(RuntimeEvent::CronCancelled {
        session_id: session_id.to_string(),
        cron_id: cron_id.to_string(),
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Mutex as StdMutex;
    use zenui_persistence::ScheduledCronStatus;

    #[derive(Default)]
    struct RecordingHandler {
        fires: StdMutex<Vec<FiredCron>>,
    }
    impl RecordingHandler {
        fn fires(&self) -> Vec<FiredCron> {
            self.fires.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl CronFireHandler for RecordingHandler {
        async fn on_cron_fired(&self, fired: FiredCron) {
            self.fires.lock().unwrap().push(fired);
        }
    }

    fn seed_session(persistence: &PersistenceService, session_id: &str) {
        persistence
            .insert_session_row_for_tests(session_id)
            .expect("seed session");
    }

    fn row(cron_id: &str, session_id: &str, expr: &str) -> ScheduledCronRow {
        ScheduledCronRow {
            cron_id: cron_id.to_string(),
            session_id: session_id.to_string(),
            origin_turn_id: Some("t-1".to_string()),
            cron_expr: expr.to_string(),
            prompt: format!("tick {cron_id}"),
            reason: None,
            status: ScheduledCronStatus::Active,
            last_fired_at_unix: None,
            created_at_unix: 0,
        }
    }

    #[test]
    fn parse_expr_accepts_5_field_posix() {
        assert!(parse_cron_expr("*/5 * * * *").is_some());
    }

    #[test]
    fn parse_expr_accepts_6_field_with_seconds() {
        assert!(parse_cron_expr("0 */5 * * * *").is_some());
    }

    #[test]
    fn parse_expr_rejects_garbage() {
        assert!(parse_cron_expr("not a cron").is_none());
        assert!(parse_cron_expr("").is_none());
    }

    #[test]
    fn parse_create_args_canonical_field_names() {
        let args = json!({
            "cron": "*/5 * * * *",
            "prompt": "poll the build",
            "reason": "loop probe",
        });
        let (expr, prompt, reason) = parse_cron_create_args(&args).unwrap();
        assert_eq!(expr, "*/5 * * * *");
        assert_eq!(prompt, "poll the build");
        assert_eq!(reason.as_deref(), Some("loop probe"));
    }

    #[test]
    fn parse_create_args_accepts_alternate_field_names() {
        // Defensive fallbacks if the SDK shifts naming.
        let snake = json!({
            "cron_expression": "*/5 * * * *",
            "prompt": "x",
        });
        let camel = json!({
            "cronExpression": "*/5 * * * *",
            "prompt": "x",
        });
        let schedule = json!({
            "schedule": "*/5 * * * *",
            "prompt": "x",
        });
        assert!(parse_cron_create_args(&snake).is_some());
        assert!(parse_cron_create_args(&camel).is_some());
        assert!(parse_cron_create_args(&schedule).is_some());
    }

    #[test]
    fn parse_create_args_rejects_empty_prompt() {
        let args = json!({ "cron": "*/5 * * * *", "prompt": "   " });
        assert!(parse_cron_create_args(&args).is_none());
    }

    #[test]
    fn parse_create_args_rejects_unparseable_cron() {
        let args = json!({ "cron": "garbage", "prompt": "x" });
        assert!(parse_cron_create_args(&args).is_none());
    }

    #[test]
    fn parse_delete_args_extracts_id() {
        assert_eq!(
            parse_cron_delete_args(&json!({ "cronId": "abc" })).as_deref(),
            Some("abc")
        );
        assert_eq!(
            parse_cron_delete_args(&json!({ "cron_id": "def" })).as_deref(),
            Some("def")
        );
        assert_eq!(
            parse_cron_delete_args(&json!({ "id": "ghi" })).as_deref(),
            Some("ghi")
        );
        assert!(parse_cron_delete_args(&json!({})).is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn recurring_cron_re_arms_after_fire() {
        let persistence = Arc::new(PersistenceService::in_memory().unwrap());
        seed_session(&persistence, "s-1");
        let handler = Arc::new(RecordingHandler::default());
        let scheduler = CronScheduler::spawn(
            Arc::clone(&persistence),
            Arc::clone(&handler) as Arc<dyn CronFireHandler>,
        );

        // `* * * * * *` = every second. With virtual time we can
        // advance two full ticks and observe two fires from one row.
        let r = row("c-1", "s-1", "* * * * * *");
        persistence.insert_cron(r.clone()).await.unwrap();
        scheduler.arm(&r);

        // First fire.
        tokio::time::advance(Duration::from_secs(2)).await;
        tokio::task::yield_now().await;
        // Second fire (re-armed automatically).
        tokio::time::advance(Duration::from_secs(2)).await;
        tokio::task::yield_now().await;

        let fires = handler.fires();
        assert!(
            fires.len() >= 2,
            "expected at least 2 fires from a per-second cron, got {}: {fires:?}",
            fires.len()
        );
        assert_eq!(fires[0].cron_id, "c-1");
        assert_eq!(fires[0].prompt, "tick c-1");
    }

    #[tokio::test(start_paused = true)]
    async fn cancel_prevents_future_fires() {
        let persistence = Arc::new(PersistenceService::in_memory().unwrap());
        seed_session(&persistence, "s-1");
        let handler = Arc::new(RecordingHandler::default());
        let scheduler = CronScheduler::spawn(
            Arc::clone(&persistence),
            Arc::clone(&handler) as Arc<dyn CronFireHandler>,
        );

        let r = row("c-keep", "s-1", "* * * * * *");
        persistence.insert_cron(r.clone()).await.unwrap();
        scheduler.arm(&r);
        let r2 = row("c-cancel", "s-1", "* * * * * *");
        persistence.insert_cron(r2.clone()).await.unwrap();
        scheduler.arm(&r2);

        // Cancel the second row before any fire, in both persistence
        // and the scheduler.
        persistence.cancel_cron("c-cancel").await;
        scheduler.cancel("c-cancel");

        tokio::time::advance(Duration::from_secs(3)).await;
        tokio::task::yield_now().await;

        let fires = handler.fires();
        assert!(
            fires.iter().all(|f| f.cron_id == "c-keep"),
            "cancelled cron should not have fired: {fires:?}"
        );
        assert!(
            !fires.is_empty(),
            "uncancelled cron should still have fired"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn reload_active_arms_pending_rows_on_restart() {
        let persistence = Arc::new(PersistenceService::in_memory().unwrap());
        seed_session(&persistence, "s-1");
        // Persist a row but DON'T arm — simulate a daemon restart.
        persistence
            .insert_cron(row("c-restart", "s-1", "* * * * * *"))
            .await
            .unwrap();

        let handler = Arc::new(RecordingHandler::default());
        let scheduler = CronScheduler::spawn(
            Arc::clone(&persistence),
            Arc::clone(&handler) as Arc<dyn CronFireHandler>,
        );
        scheduler.reload_active(&persistence).await;

        tokio::time::advance(Duration::from_secs(2)).await;
        tokio::task::yield_now().await;

        let fires = handler.fires();
        assert!(
            !fires.is_empty(),
            "reload_active should re-arm persisted rows"
        );
        assert_eq!(fires[0].cron_id, "c-restart");
    }
}
