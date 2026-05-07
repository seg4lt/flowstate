//! Cross-thread / cross-project orchestration dispatcher.
//!
//! Owns the live state needed to coordinate cross-session work without
//! bloating `RuntimeCore`'s top-level field list: awaiting-reply
//! oneshots, per-turn orchestration budget, and the cycle-detection
//! graph. The `RuntimeCore` itself provides the actual `dispatch_*`
//! methods (so they can reach adapters, persistence, etc.) — this file
//! is the state container plus a couple of tiny helpers.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use tokio::sync::{Mutex, oneshot};
use zenui_provider_api::{
    PermissionMode, PollOutcome, ReasoningEffort, RuntimeCall, RuntimeCallError, RuntimeCallOrigin,
    RuntimeCallResult, TurnEventSink, TurnStatus,
};

use crate::RuntimeCore;

/// Hard-cap on orchestration calls per originating user turn. Stops an
/// agent from fanning out 500 spawns before anyone notices.
pub const DEFAULT_TURN_BUDGET: u32 = 10;

/// Default per-call timeout (seconds) when the caller doesn't provide
/// one. Long enough to cover agentic turns that take several minutes.
pub const DEFAULT_AWAIT_TIMEOUT_SECS: u64 = 30 * 60;

/// Upper bound we'll honor when a caller *does* provide a timeout.
pub const MAX_AWAIT_TIMEOUT_SECS: u64 = 30 * 60;

/// Max depth of the awaiting-graph chain. A→B→C→D is fine; A→B→C→D→E
/// trips the cycle guard. Keeps runaway delegation bounded even when
/// the graph itself is acyclic.
pub const MAX_AWAIT_DEPTH: usize = 4;

/// A caller awaiting a future reply from `target_session`. When the
/// target's next turn completes with a final assistant message, the
/// oneshot is resolved with `(turn_id, reply_text)`. If the turn
/// finishes Interrupted / Failed, the sender is dropped, which wakes
/// the awaiter with an Err that the dispatcher maps to
/// `RuntimeCallError::Cancelled`.
pub struct PendingReply {
    pub after_turn_id: Option<String>,
    pub sender: oneshot::Sender<(String, String)>,
}

/// A queued async `Send` — delivered on the next turn boundary when
/// the target session is currently running. For v1 we store plain
/// strings; a future revision can widen this to `Vec<ContentBlock>`
/// without changing the caller-visible API.
#[derive(Debug, Clone)]
pub struct QueuedMessage {
    pub from_session_id: String,
    pub message: String,
}

#[derive(Default)]
pub struct OrchestrationState {
    /// Sessions waiting for a reply from another session, keyed by the
    /// target session. A single target can have multiple awaiters.
    pub pending_replies: Mutex<HashMap<String, Vec<PendingReply>>>,
    /// Per-session message inbox for async `Send`. Drained on each
    /// turn-boundary tick (see `runtime-core` completion hook).
    pub mailboxes: Mutex<HashMap<String, VecDeque<QueuedMessage>>>,
    /// awaiter → {targets it's waiting on}. Used for cycle detection:
    /// before registering a new pending reply, walk the graph to make
    /// sure the new target isn't already (directly or transitively)
    /// waiting on the awaiter.
    pub awaiting_graph: Mutex<HashMap<String, HashSet<String>>>,
    /// Per-origin-turn budget counter. Keyed by the originating turn
    /// id (the turn that called a `flowstate_*` tool). Reset naturally
    /// on turn completion — entries are removed at the end of the turn.
    pub budgets: Mutex<HashMap<String, u32>>,
}

impl OrchestrationState {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Return `Err(Cycle)` if `target` is already (directly or
    /// transitively) waiting on `awaiter`. Otherwise record the new
    /// edge `awaiter → target`. Called right before registering a
    /// pending reply.
    pub async fn register_await(
        &self,
        awaiter: &str,
        target: &str,
    ) -> Result<(), RuntimeCallError> {
        let mut graph = self.awaiting_graph.lock().await;

        // Does `target` (or something it awaits) already reach `awaiter`?
        let mut frontier: Vec<String> = vec![target.to_string()];
        let mut seen: HashSet<String> = HashSet::new();
        let mut depth = 0usize;
        while let Some(node) = frontier.pop() {
            if node == awaiter {
                return Err(RuntimeCallError::Cycle {
                    session_id: target.to_string(),
                });
            }
            if !seen.insert(node.clone()) {
                continue;
            }
            depth += 1;
            if depth > MAX_AWAIT_DEPTH {
                return Err(RuntimeCallError::Cycle {
                    session_id: target.to_string(),
                });
            }
            if let Some(next) = graph.get(&node) {
                frontier.extend(next.iter().cloned());
            }
        }

        graph
            .entry(awaiter.to_string())
            .or_default()
            .insert(target.to_string());
        Ok(())
    }

    pub async fn unregister_await(&self, awaiter: &str, target: &str) {
        let mut graph = self.awaiting_graph.lock().await;
        if let Some(set) = graph.get_mut(awaiter) {
            set.remove(target);
            if set.is_empty() {
                graph.remove(awaiter);
            }
        }
    }

    /// Reserve one unit of the turn budget. Returns
    /// `RuntimeCallError::BudgetExceeded` if the turn has already
    /// exhausted its allotment.
    pub async fn reserve_budget(&self, turn_id: &str) -> Result<(), RuntimeCallError> {
        let mut guard = self.budgets.lock().await;
        let counter = guard.entry(turn_id.to_string()).or_insert(0);
        if *counter >= DEFAULT_TURN_BUDGET {
            return Err(RuntimeCallError::BudgetExceeded);
        }
        *counter += 1;
        Ok(())
    }

    /// Release the budget counter for a finished turn. Called from the
    /// send_turn exit path.
    pub async fn release_budget(&self, turn_id: &str) {
        let mut guard = self.budgets.lock().await;
        guard.remove(turn_id);
    }

    /// Register an awaiter for `target_session`'s next turn. Multiple
    /// awaiters are fine — all senders are woken when the target's
    /// turn completes.
    pub async fn register_reply(&self, target_session: &str, pending: PendingReply) {
        let mut guard = self.pending_replies.lock().await;
        guard
            .entry(target_session.to_string())
            .or_default()
            .push(pending);
    }

    /// Called by the turn-completion hook. Removes and returns every
    /// pending reply for this session whose `after_turn_id` is
    /// satisfied by the turn that just finished. "Satisfied" means:
    /// the awaiter had no turn cursor, OR the cursor matches a turn
    /// strictly earlier than the just-finished one. For v1 we pop all
    /// awaiters unconditionally — callers set cursors only when they
    /// want to skip older replies.
    pub async fn drain_replies_for(&self, session_id: &str) -> Vec<PendingReply> {
        let mut guard = self.pending_replies.lock().await;
        guard.remove(session_id).unwrap_or_default()
    }

    pub async fn enqueue_message(&self, target_session: &str, from: &str, message: String) {
        let mut guard = self.mailboxes.lock().await;
        guard
            .entry(target_session.to_string())
            .or_default()
            .push_back(QueuedMessage {
                from_session_id: from.to_string(),
                message,
            });
    }

    pub async fn pop_message(&self, session_id: &str) -> Option<QueuedMessage> {
        let mut guard = self.mailboxes.lock().await;
        let queue = guard.get_mut(session_id)?;
        let m = queue.pop_front();
        if queue.is_empty() {
            guard.remove(session_id);
        }
        m
    }
}

/// Resolve a single pending reply using the just-completed turn. Maps
/// the turn's status onto either an `Ok(turn_id, output)` (on
/// Completed) or a dropped sender (on Interrupted/Failed — awaiter's
/// receiver sees Err, dispatcher translates to Cancelled).
pub fn resolve_pending_reply(
    pending: PendingReply,
    turn_id: &str,
    final_output: &str,
    status: TurnStatus,
) {
    match status {
        TurnStatus::Completed => {
            let _ = pending
                .sender
                .send((turn_id.to_string(), final_output.to_string()));
        }
        // Drop the sender; receiver wakes with Err.
        TurnStatus::Interrupted | TurnStatus::Failed | TurnStatus::Running => {
            drop(pending.sender);
        }
    }
}

/// Utility: clamp a caller-supplied timeout to `[1, MAX_AWAIT_TIMEOUT_SECS]`
/// seconds, falling back to `DEFAULT_AWAIT_TIMEOUT_SECS` if the caller
/// passed `None`.
pub fn clamp_timeout(secs: Option<u64>) -> std::time::Duration {
    let raw = secs.unwrap_or(DEFAULT_AWAIT_TIMEOUT_SECS);
    let clamped = raw.clamp(1, MAX_AWAIT_TIMEOUT_SECS);
    std::time::Duration::from_secs(clamped)
}

/// Derived result when a `Poll` call finds a newer completed turn.
pub fn poll_result_from_turn(turn_id: &str, output: &str) -> RuntimeCallResult {
    RuntimeCallResult::Poll(PollOutcome::Ready {
        reply: output.to_string(),
        turn_id: turn_id.to_string(),
    })
}

/// Spawn a peer session turn on a detached task. Lives in this sibling
/// module (not inline in `lib.rs`) so rustc can check the `Send` bound
/// on the `send_turn` future — Rust's auto-trait leakage for `impl
/// Future` opaque types is not computed inside the type's defining
/// scope, which is exactly the impl block where `send_turn` is declared.
///
/// Fire-and-forget: the calling dispatcher path typically holds a
/// separate oneshot awaiter (or doesn't care about completion for
/// async `Spawn`/`Send`); if `send_turn` fails we just log.
/// Fire a turn on a peer session off-thread. `permission_mode` and
/// `reasoning_effort` control the opening turn; the `send`/`send_and_await`
/// paths pass `PermissionMode::Default` + `None` to preserve historical
/// behavior, while the spawn dispatchers forward the caller-chosen
/// values coming off `RuntimeCall::Spawn{,AndAwait,InWorktree}`.
pub fn spawn_peer_turn(
    rc: Arc<RuntimeCore>,
    session_id: String,
    message: String,
    label: &'static str,
    permission_mode: PermissionMode,
    reasoning_effort: Option<ReasoningEffort>,
) {
    tokio::spawn(async move {
        if let Err(err) = rc
            .send_turn(
                session_id,
                message,
                Vec::new(),
                permission_mode,
                reasoning_effort,
                None,
            )
            .await
        {
            tracing::warn!(label, error = %err, "peer turn failed");
        }
    });
}

/// Spawn a dispatcher task for a `RuntimeCall` event. Runs the
/// dispatch outside the originating session's drain loop so the drain
/// keeps streaming the caller's events; resolves the sink's
/// `runtime_pending` oneshot when the dispatcher returns.
pub fn spawn_dispatch(
    rc: Arc<RuntimeCore>,
    origin: RuntimeCallOrigin,
    call: RuntimeCall,
    request_id: String,
    sink: Option<TurnEventSink>,
) {
    tokio::spawn(async move {
        let result = rc.dispatch_runtime_call(origin, call).await;
        if let Some(sink) = sink {
            sink.resolve_runtime_call(&request_id, result).await;
        }
    });
}
