//! Internal helpers for `RuntimeCore` that don't belong on the main
//! impl block: RAII guards that keep observer counters / per-session
//! maps in sync across early `?` returns, a standalone async
//! model-refresh spawner callable from tasks that don't hold `&self`,
//! the in-flight turn snapshot builder, and the cache-staleness check.
//!
//! Extracted from `lib.rs` in the phase 3 god-file split. All items
//! are `pub(crate)` — they're crate-internal scaffolding that the
//! outer `RuntimeCore` impl in `lib.rs` consumes.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};

use chrono::Utc;
use tokio::sync::{Mutex, broadcast};
use zenui_persistence::PersistenceService;
use zenui_provider_api::{
    ContentBlock, FileChangeRecord, PermissionMode, PlanRecord, ProviderAdapter, ProviderKind,
    RuntimeEvent, SubagentRecord, TokenUsage, ToolCall, TurnRecord,
};

use crate::{MODEL_CACHE_TTL_HOURS, TurnLifecycleObserver};

/// Internal result of `RuntimeCore::rewind_files`. Not exported to
/// transports or the wire — `handle_client_message` decomposes it
/// into a `RuntimeEvent::FilesRewound` broadcast plus an Ack. Kept
/// as a struct rather than a tuple so the call site reads
/// self-documentingly when the lists grow more entries (e.g. paths
/// we *intended* to touch but couldn't).
pub(crate) struct RewindOutcome {
    pub(crate) paths_restored: Vec<String>,
    pub(crate) paths_deleted: Vec<String>,
}

/// RAII guard that ticks the `TurnLifecycleObserver` counter around the
/// lifetime of `send_turn`. Drop runs on every exit path (normal return,
/// early `?` return, panic), so the daemon-side counter cannot leak even if
/// an adapter panics or a `.await?` unwinds the task.
pub(crate) struct TurnCounterGuard {
    observer: Option<Arc<dyn TurnLifecycleObserver>>,
    session_id: String,
}

impl TurnCounterGuard {
    pub(crate) fn new(
        observer: Option<Arc<dyn TurnLifecycleObserver>>,
        session_id: String,
    ) -> Self {
        if let Some(obs) = &observer {
            obs.on_turn_start(&session_id);
        }
        Self {
            observer,
            session_id,
        }
    }
}

impl Drop for TurnCounterGuard {
    fn drop(&mut self) {
        if let Some(obs) = &self.observer {
            obs.on_turn_end(&self.session_id);
        }
    }
}

/// RAII guard that removes the session's entry from
/// `in_flight_permission_mode` on every exit path of `send_turn`.
/// Mirrors `TurnCounterGuard`'s shape — a plain `remove()` at the
/// bottom of the function would leak the entry whenever an early
/// `?` return unwinds the task.
pub(crate) struct InFlightPermissionModeGuard {
    pub(crate) map: Arc<RwLock<HashMap<String, PermissionMode>>>,
    pub(crate) session_id: String,
}

impl Drop for InFlightPermissionModeGuard {
    fn drop(&mut self) {
        if let Ok(mut live) = self.map.write() {
            live.remove(&self.session_id);
        }
    }
}

/// Standalone model-refresh spawner, usable from both `RuntimeCore::spawn_model_refresh`
/// and from within already-spawned tasks (like `spawn_health_check`) that don't have `&self`.
pub(crate) fn spawn_model_refresh_detached(
    kind: ProviderKind,
    adapter: Arc<dyn ProviderAdapter>,
    persistence: Arc<PersistenceService>,
    event_tx: broadcast::Sender<RuntimeEvent>,
    in_flight: Arc<Mutex<HashSet<ProviderKind>>>,
) {
    tokio::spawn(async move {
        // Dedupe: skip if another refresh for this provider is already running.
        {
            let mut guard = in_flight.lock().await;
            if guard.contains(&kind) {
                tracing::debug!(?kind, "skipping duplicate model refresh");
                return;
            }
            guard.insert(kind);
        }

        let result = adapter.fetch_models().await;

        // Always release the in-flight slot, regardless of outcome.
        {
            let mut guard = in_flight.lock().await;
            guard.remove(&kind);
        }

        match result {
            Ok(models) if !models.is_empty() => {
                tracing::info!(
                    ?kind,
                    count = models.len(),
                    "fetched provider models, persisting and broadcasting"
                );
                persistence.set_cached_models(kind, &models).await;
                // Keep provider_health_cache.status_json in sync with
                // the fresh model list. The bootstrap path now prefers
                // provider_model_cache, but any other reader that
                // touches only the health cache (or a future one) must
                // not observe a stale list.
                if let Some((_, mut status)) = persistence.get_cached_health(kind).await {
                    status.models = models.clone();
                    persistence.set_cached_health(kind, &status).await;
                }
                let _ = event_tx.send(RuntimeEvent::ProviderModelsUpdated {
                    provider: kind,
                    models,
                });
            }
            Ok(_) => {
                tracing::debug!(?kind, "fetch_models returned empty list");
            }
            Err(e) => {
                tracing::warn!(?kind, "fetch_models failed: {e}");
            }
        }
    });
}

/// Build a `TurnRecord` snapshot from the accumulator's local state and
/// stash it under `session_id` in the live in-flight map. Called after
/// every event in the drain loop, so the map always reflects the
/// latest known state of the running turn — that's what `live_session_detail`
/// hands back to a client recovering from broadcast lag.
#[allow(clippy::too_many_arguments)]
pub(crate) fn write_in_flight_snapshot(
    in_flight_turns: &Arc<RwLock<HashMap<String, TurnRecord>>>,
    session_id: &str,
    base: &TurnRecord,
    accumulated: &str,
    reasoning_text: &str,
    tool_calls: &[ToolCall],
    file_changes: &[FileChangeRecord],
    subagents: &[SubagentRecord],
    plan: &Option<PlanRecord>,
    blocks: &[ContentBlock],
    usage: &Option<TokenUsage>,
) {
    let mut snap = base.clone();
    snap.output = accumulated.to_string();
    snap.reasoning = if reasoning_text.is_empty() {
        None
    } else {
        Some(reasoning_text.to_string())
    };
    snap.tool_calls = tool_calls.to_vec();
    snap.file_changes = file_changes.to_vec();
    snap.subagents = subagents.to_vec();
    snap.plan = plan.clone();
    snap.blocks = blocks.to_vec();
    snap.usage = usage.clone();
    if let Ok(mut map) = in_flight_turns.write() {
        map.insert(session_id.to_string(), snap);
    }
}

/// Returns true if the ISO-8601 `fetched_at` timestamp is older than the model
/// cache TTL. Unparseable timestamps are treated as stale so we'll re-fetch.
pub(crate) fn is_cache_stale(fetched_at: &str) -> bool {
    match chrono::DateTime::parse_from_rfc3339(fetched_at) {
        Ok(parsed) => {
            let age = Utc::now().signed_duration_since(parsed.with_timezone(&Utc));
            age > chrono::Duration::hours(MODEL_CACHE_TTL_HOURS)
        }
        Err(_) => true,
    }
}
