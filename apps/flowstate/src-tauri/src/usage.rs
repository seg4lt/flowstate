// Thin Tauri command + subscriber-wiring layer over the
// `zenui-usage-store` core crate.
//
// The actual analytics ORM (schema, rollups, group-by SQL, timeseries
// zero-fill, tests) lives in `crates/core/usage-store`. This file only
// owns what is genuinely Tauri-runtime glue:
//
//   * `#[tauri::command]` handlers that forward to the store.
//   * `spawn_turn_completed_subscriber` which subscribes to the
//     runtime broadcast and records finalized turns.
//
// Moving the ORM out kept ~1,180 lines of domain logic in the correct
// layer (Phase 4.1 of the architecture audit).

use tauri::State;
use tokio::sync::broadcast::error::RecvError;
use zenui_provider_api::RuntimeEvent;
use zenui_runtime_core::RuntimeCore;
use zenui_usage_store::{
    TopSessionRow, UsageBucket, UsageEvent, UsageGroupBy, UsageRange, UsageStore,
    UsageSummaryPayload, UsageTimeseriesPayload,
};

#[tauri::command]
pub fn get_usage_summary(
    store: State<'_, UsageStore>,
    range: UsageRange,
    group_by: Option<UsageGroupBy>,
) -> Result<UsageSummaryPayload, String> {
    store.summary(range, group_by.unwrap_or_default())
}

#[tauri::command]
pub fn get_usage_timeseries(
    store: State<'_, UsageStore>,
    range: UsageRange,
    bucket: UsageBucket,
    split_by: Option<UsageGroupBy>,
) -> Result<UsageTimeseriesPayload, String> {
    store.timeseries(range, bucket, split_by)
}

#[tauri::command]
pub fn get_top_sessions(
    store: State<'_, UsageStore>,
    range: UsageRange,
    limit: Option<u32>,
) -> Result<Vec<TopSessionRow>, String> {
    store.top_sessions(range, limit.unwrap_or(10))
}

/// Spawn the long-lived subscriber that turns
/// `RuntimeEvent::TurnCompleted` events into rows in the usage store.
/// Runs on the host tokio runtime and exits cleanly when the runtime
/// broadcast is closed.
///
/// Broadcast lag is non-fatal: we log and keep listening. Missing
/// telemetry never affects runtime correctness.
pub fn spawn_turn_completed_subscriber(runtime: &RuntimeCore, writer: UsageStore) {
    let mut rx = runtime.subscribe();
    tauri::async_runtime::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(RuntimeEvent::TurnCompleted { session, turn, .. }) => {
                    let event = UsageEvent::from_turn(&session, &turn);
                    if let Err(e) = writer.record_turn(&event) {
                        tracing::warn!("record turn usage failed: {e}");
                    }
                }
                Ok(_) => {}
                Err(RecvError::Lagged(n)) => {
                    tracing::warn!("usage subscriber lagged by {n} events; continuing");
                }
                Err(RecvError::Closed) => break,
            }
        }
    });
}
