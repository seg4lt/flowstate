use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::{Mutex, oneshot};

// The split into sibling modules is largely mechanical; every event
// variant references types defined across `types.rs`, `messages.rs`,
// and helper modules. Reach for the glob-re-export from the crate
// root to keep this file readable rather than maintaining an ever-
// growing hand-curated use list.
use crate::orchestration::{RuntimeCall, RuntimeCallError, RuntimeCallResult};
use crate::*;

/// Events that a provider adapter can push during a turn for streaming display.
#[derive(Debug, Clone)]
pub enum ProviderTurnEvent {
    AssistantTextDelta {
        delta: String,
    },
    ReasoningDelta {
        delta: String,
    },
    ToolCallStarted {
        call_id: String,
        name: String,
        args: Value,
        /// Parent Task/Agent call_id when this tool call originates
        /// from a sub-agent; `None` for main-agent calls.
        parent_call_id: Option<String>,
    },
    ToolCallCompleted {
        call_id: String,
        output: String,
        error: Option<String>,
    },
    Info {
        message: String,
    },
    PermissionRequest {
        request_id: String,
        tool_name: String,
        input: Value,
        suggested_decision: PermissionDecision,
    },
    UserQuestion {
        request_id: String,
        questions: Vec<UserInputQuestion>,
    },
    FileChange {
        call_id: String,
        path: String,
        operation: FileOperation,
        before: Option<String>,
        after: Option<String>,
    },
    SubagentStarted {
        parent_call_id: String,
        agent_id: String,
        agent_type: String,
        prompt: String,
        /// Raw provider-level model this subagent will run on, when
        /// known at spawn time (i.e. the adapter can read a model
        /// override from its static agent catalog). Adapters that
        /// don't distinguish pass `None`; the runtime then falls
        /// back to the session model for display.
        model: Option<String>,
    },
    SubagentEvent {
        agent_id: String,
        event: Value,
    },
    SubagentCompleted {
        agent_id: String,
        output: String,
        error: Option<String>,
    },
    /// Emitted when the provider tells us — usually via the first
    /// assistant message from the subagent — which model the SDK
    /// actually resolved to run the subagent on. Distinct from the
    /// planned value on `SubagentStarted.model` because SDKs can
    /// resolve aliases, honour runtime overrides, or fall back
    /// when the requested model is unavailable. Runtime-core
    /// overwrites `SubagentRecord.model` with this when it fires.
    SubagentModelObserved {
        agent_id: String,
        model: String,
    },
    PlanProposed {
        plan_id: String,
        title: String,
        steps: Vec<PlanStep>,
        raw: String,
    },
    /// Token usage for the current turn, emitted once when the
    /// provider's final result message arrives.
    TurnUsage {
        usage: TokenUsage,
    },
    /// The provider has told us which model it actually resolved to
    /// for this turn. The string the user picked in the dropdown
    /// (or read from Settings defaults) can be an alias — e.g. the
    /// Claude SDK accepts `"sonnet"` and internally resolves it to
    /// a specific pinned version like
    /// `"claude-sonnet-4-5-20250929"`. Runtime-core uses this to
    /// upgrade `session.summary.model` so the UI's model-selector
    /// dropdown highlights the correct entry (its list contains the
    /// pinned version, not the alias).
    ModelResolved {
        model: String,
    },
    /// Rate-limit / plan-usage snapshot for a single bucket. Can fire
    /// multiple times per turn if the provider updates several
    /// buckets at once. Conceptually account-wide — runtime-core
    /// promotes this to RuntimeEvent::RateLimitUpdated without
    /// attaching it to the current TurnRecord.
    RateLimitUpdated {
        info: RateLimitInfo,
    },
    /// The SDK inserted a compaction boundary in the stream — older
    /// turns are being compressed into a summary. Arrives paired
    /// with `CompactSummary` (hook-sourced, may land before or after
    /// this event). Runtime-core merges the pair into one
    /// `ContentBlock::Compact` on the current turn.
    CompactBoundary {
        trigger: CompactTrigger,
        pre_tokens: Option<u64>,
        post_tokens: Option<u64>,
        duration_ms: Option<u64>,
    },
    /// Summary text produced by compaction. Pairs with
    /// `CompactBoundary`; whichever lands first creates the block,
    /// the second fills the gap.
    CompactSummary {
        trigger: CompactTrigger,
        summary: String,
    },
    /// The SDK's memory-recall supervisor surfaced relevant memory
    /// files (or a synthesis paragraph) into the turn's context.
    /// Runtime-core appends one `ContentBlock::MemoryRecall` per
    /// occurrence to the turn's blocks.
    MemoryRecall {
        mode: MemoryRecallMode,
        memories: Vec<MemoryRecallItem>,
    },
    /// Coarse-grained turn-phase signal. Providers emit this when
    /// they enter / exit a non-streaming phase (waiting on the API
    /// to start responding, compressing history). Drives the
    /// working-indicator secondary label so long pauses carry a
    /// label instead of looking stuck. Absence of events keeps the
    /// label empty; runtime-core does not synthesize phases.
    StatusChanged {
        phase: TurnPhase,
    },
    /// The provider is auto-retrying a transient API error. Drives
    /// the "Retrying (2 of 5)…" banner. Attempts are 1-indexed.
    /// `retry_delay_ms` is the SDK's planned backoff before the
    /// next attempt fires. `error_status` is the HTTP status that
    /// triggered the retry (if known); `error` is a human-readable
    /// summary that gets tucked into the banner's tooltip.
    TurnRetrying {
        attempt: u32,
        max_retries: u32,
        retry_delay_ms: u64,
        error_status: Option<u16>,
        error: String,
    },
    /// Provider-predicted next user prompt. Emitted near the end of
    /// a turn when the provider supports it. Frontend stores the
    /// latest suggestion per session and renders it as ghost text
    /// in the empty composer; any keystroke dismisses it.
    PromptSuggestion {
        suggestion: String,
    },
    /// Heartbeat for an in-flight tool call. Emitted periodically
    /// by providers that opt in (`ProviderFeatures.tool_progress`).
    /// Runtime-core stamps `ToolCall::last_progress_at` on receipt
    /// so the frontend can distinguish "still ticking" from "stuck"
    /// per-tool, instead of falling back to the session-wide
    /// silence detector.
    ToolProgress {
        call_id: String,
        /// Tool name from the provider (e.g. "Bash"). Carried for
        /// log clarity and for the stuck-banner copy when this
        /// heartbeat is the one that's gone stale.
        tool_name: String,
        /// Parent Task/Agent call_id when the tool runs inside a
        /// sub-agent; mirrors the field on `ToolCallStarted`.
        parent_call_id: Option<String>,
        /// ISO 8601 timestamp stamped at the bridge when the SDK's
        /// heartbeat arrived. We propagate this verbatim rather
        /// than restamping inside runtime-core so the freshness
        /// clock measures wall time at the source, not arrival
        /// time at our end of the bridge channel.
        occurred_at: String,
    },
    /// Cross-session orchestration call. Adapters emit this when the
    /// underlying agent invokes a flowstate_* capability tool; the
    /// runtime-core drain loop dispatches the call and resolves the
    /// matching oneshot on the sink's `runtime_pending` map. The shape
    /// is provider-agnostic — every adapter bridge translates its
    /// native tool invocation into the same `RuntimeCall`.
    RuntimeCall {
        request_id: String,
        call: crate::RuntimeCall,
    },
}

/// Phase of a turn between streams. Deliberately coarse — only
/// phases that warrant a UI label earn a variant here. Unknown
/// or unmapped provider phases fall through to `Idle` so the
/// working indicator shows no label rather than a misleading one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-bindings", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-bindings", ts(optional_fields))]
#[serde(rename_all = "snake_case")]
pub enum TurnPhase {
    /// No active non-streaming phase. Default; renders no label.
    Idle,
    /// Provider is waiting on the model API to start producing
    /// tokens (Anthropic's `status: "requesting"`, etc.).
    Requesting,
    /// Tokens are actively streaming — the spinner is the right
    /// signal; this phase exists mostly for parity with providers
    /// that distinguish it from `Idle`.
    Streaming,
    /// Provider is compacting older history (auto-compact or user
    /// triggered). The `compact_boundary` / `PostCompact` recap
    /// will land after this phase clears.
    Compacting,
    /// Turn has paused mid-stream waiting on user input
    /// (permission prompt, AskUserQuestion, plan-exit approval).
    /// Renders the "awaiting your answer" label where the normal
    /// time counter would be.
    AwaitingInput,
}

/// Per-session "always allow / always deny" memory keyed by tool name.
/// Lives in `RuntimeCore` and is shared across every `TurnEventSink` for
/// a given session, so a user's "Always" decision in turn N short-circuits
/// every subsequent prompt for the same tool in turn N+1, N+2, ...
///
/// Stored decisions are normalized to `Allow` or `Deny` (never `AllowAlways`
/// or `DenyAlways`) — providers don't care about the "always" suffix, only
/// about the binary outcome.
pub type PermissionPolicy = Arc<Mutex<HashMap<String, PermissionDecision>>>;

/// A handle for adapters to push streaming events during `execute_turn`.
/// Dropping the sink closes the channel, signalling to the runtime that the turn is complete.
#[derive(Clone)]
pub struct TurnEventSink {
    tx: tokio::sync::mpsc::Sender<ProviderTurnEvent>,
    /// Oneshot per outstanding permission request, keyed by the
    /// internal `perm-N` id. The payload is `(decision, mode_override)`
    /// so "approve and switch mode" is delivered atomically with the
    /// decision itself — no side channel, no id-lookup mistakes.
    permission_pending:
        Arc<Mutex<HashMap<String, oneshot::Sender<(PermissionDecision, Option<PermissionMode>)>>>>,
    question_pending: Arc<Mutex<HashMap<String, oneshot::Sender<Vec<UserInputAnswer>>>>>,
    /// Oneshot per outstanding orchestration `RuntimeCall`, keyed by
    /// the internal `run-N` id. Mirrors the permission / question
    /// pattern — the adapter awaits the receiver; the runtime drain
    /// loop dispatches the call and resolves the sender.
    runtime_pending:
        Arc<Mutex<HashMap<String, oneshot::Sender<Result<RuntimeCallResult, RuntimeCallError>>>>>,
    /// Session-scoped persistent permission decisions. Shared across turns.
    policy: PermissionPolicy,
}

impl TurnEventSink {
    /// Create a sink with a fresh, isolated permission policy. Mostly for
    /// tests and one-off uses where there's no shared session state.
    pub fn new(tx: tokio::sync::mpsc::Sender<ProviderTurnEvent>) -> Self {
        Self::with_policy(tx, Arc::new(Mutex::new(HashMap::new())))
    }

    /// Create a sink wired to an existing per-session permission policy. The
    /// runtime calls this once per turn, passing in the policy that lives on
    /// `RuntimeCore` keyed by `session_id`, so always-allow/always-deny
    /// decisions persist for the rest of the session.
    pub fn with_policy(
        tx: tokio::sync::mpsc::Sender<ProviderTurnEvent>,
        policy: PermissionPolicy,
    ) -> Self {
        Self {
            tx,
            permission_pending: Arc::new(Mutex::new(HashMap::new())),
            question_pending: Arc::new(Mutex::new(HashMap::new())),
            runtime_pending: Arc::new(Mutex::new(HashMap::new())),
            policy,
        }
    }

    /// Send a streaming event. Silently drops if the channel is closed.
    pub async fn send(&self, event: ProviderTurnEvent) {
        let _ = self.tx.send(event).await;
    }

    /// Ask the host to decide on a tool invocation. Emits a PermissionRequest event
    /// and awaits the host's answer. Returns `(Deny, None)` if the channel closes.
    ///
    /// The second tuple element is an optional `PermissionMode` the host
    /// wants applied alongside the approval — used by the plan-exit
    /// approve-and-switch flow. Adapters that don't care about mode
    /// switches can simply ignore it; there is no separate side channel
    /// to forget to read.
    ///
    /// Short-circuits the host prompt entirely if a previous turn in this
    /// session already answered `AllowAlways` or `DenyAlways` for the same
    /// tool name. The cached fast path always returns `None` for the mode
    /// override — "always" decisions are tool-scoped, not mode-scoped.
    pub async fn request_permission(
        &self,
        tool_name: String,
        input: Value,
        suggested: PermissionDecision,
    ) -> (PermissionDecision, Option<PermissionMode>) {
        // Fast path: prior "always" decision in this session.
        {
            let policy = self.policy.lock().await;
            if let Some(decision) = policy.get(&tool_name).copied() {
                return (decision, None);
            }
        }

        let request_id = next_request_id("perm");
        let (sender, receiver) = oneshot::channel();
        {
            let mut guard = self.permission_pending.lock().await;
            guard.insert(request_id.clone(), sender);
            tracing::info!(
                request_id = %request_id,
                tool_name = %tool_name,
                pending_count = guard.len(),
                "permission request registered in pending map"
            );
        }
        self.send(ProviderTurnEvent::PermissionRequest {
            request_id: request_id.clone(),
            tool_name: tool_name.clone(),
            input,
            suggested_decision: suggested,
        })
        .await;
        let (decision, mode_override) = match receiver.await {
            Ok(payload) => payload,
            Err(_) => {
                tracing::warn!(
                    request_id = %request_id,
                    "permission oneshot receiver got Err — sender dropped before answer (turn probably ended mid-prompt)"
                );
                let mut guard = self.permission_pending.lock().await;
                guard.remove(&request_id);
                return (PermissionDecision::Deny, None);
            }
        };

        // Persist "always" decisions before normalizing the return value.
        // The provider only sees Allow/Deny — the "always" suffix is purely
        // a host-side hint that we should remember the answer.
        let normalized = match decision {
            PermissionDecision::AllowAlways => {
                self.policy
                    .lock()
                    .await
                    .insert(tool_name, PermissionDecision::Allow);
                PermissionDecision::Allow
            }
            PermissionDecision::DenyAlways => {
                self.policy
                    .lock()
                    .await
                    .insert(tool_name, PermissionDecision::Deny);
                PermissionDecision::Deny
            }
            other => other,
        };
        (normalized, mode_override)
    }

    /// Ask the user one or more structured clarifying questions and await their
    /// answers. Returns None if the channel closes before the user answers.
    pub async fn ask_user(
        &self,
        questions: Vec<UserInputQuestion>,
    ) -> Option<Vec<UserInputAnswer>> {
        let request_id = next_request_id("q");
        let (sender, receiver) = oneshot::channel();
        {
            let mut guard = self.question_pending.lock().await;
            guard.insert(request_id.clone(), sender);
        }
        self.send(ProviderTurnEvent::UserQuestion {
            request_id: request_id.clone(),
            questions,
        })
        .await;
        match receiver.await {
            Ok(answers) => Some(answers),
            Err(_) => {
                let mut guard = self.question_pending.lock().await;
                guard.remove(&request_id);
                None
            }
        }
    }

    /// Host-side: called by the runtime when the user answers a permission request.
    pub async fn resolve_permission(&self, request_id: &str, decision: PermissionDecision) {
        self.resolve_permission_with_mode(request_id, decision, None)
            .await;
    }

    /// Like `resolve_permission`, but also attaches an optional
    /// permission-mode override that travels atomically with the
    /// decision through the oneshot. Used by the plan-exit
    /// approve-and-switch flow so the Claude SDK adapter can include
    /// `updatedPermissions` in the bridge's `PermissionResult` within
    /// the same wake-up that delivers the decision. Logs a warning if
    /// there is no pending sender for the id — that almost always
    /// means a stale or mis-routed answer.
    pub async fn resolve_permission_with_mode(
        &self,
        request_id: &str,
        decision: PermissionDecision,
        mode_override: Option<PermissionMode>,
    ) {
        let mut guard = self.permission_pending.lock().await;
        match guard.remove(request_id) {
            Some(sender) => {
                tracing::info!(
                    request_id,
                    ?decision,
                    has_mode_override = mode_override.is_some(),
                    "resolve_permission firing oneshot"
                );
                let _ = sender.send((decision, mode_override));
            }
            None => {
                tracing::warn!(
                    request_id,
                    "resolve_permission found no pending sender — stale or mis-routed answer"
                );
            }
        }
    }

    /// Unblock every permission and question oneshot that is still
    /// sitting in this sink's pending maps. Dropping the sender side
    /// causes the receiver inside `request_permission` / `ask_user`
    /// to wake with Err and return a synthetic Deny / None, which
    /// unwinds any spawned task still waiting on a user answer.
    ///
    /// The adapter must call this at the very end of `run_turn` so
    /// orphaned permission tasks (e.g. from an interrupt that tore
    /// down the bridge before the user answered every prompt) don't
    /// live forever holding the sink's `permission_pending` map and
    /// leaking memory.
    pub async fn drain_pending(&self) {
        let drained_perms = {
            let mut guard = self.permission_pending.lock().await;
            let n = guard.len();
            guard.clear();
            n
        };
        let drained_qs = {
            let mut guard = self.question_pending.lock().await;
            let n = guard.len();
            guard.clear();
            n
        };
        let drained_runtime = {
            let mut guard = self.runtime_pending.lock().await;
            let n = guard.len();
            guard.clear();
            n
        };
        if drained_perms > 0 || drained_qs > 0 || drained_runtime > 0 {
            tracing::info!(
                drained_permissions = drained_perms,
                drained_questions = drained_qs,
                drained_runtime_calls = drained_runtime,
                "sink drain_pending: released orphaned oneshots"
            );
        }
    }

    /// Adapter-side: ask the runtime to perform a cross-session
    /// orchestration action (spawn a peer, message an existing session,
    /// poll for a reply, ...). Mirrors `request_permission` — registers
    /// a oneshot, emits a `RuntimeCall` event, awaits the dispatcher's
    /// reply. Returns `Err(Cancelled)` if the sink is drained before
    /// the dispatcher answers (e.g. the turn is interrupted).
    pub async fn runtime_call(
        &self,
        call: RuntimeCall,
    ) -> Result<RuntimeCallResult, RuntimeCallError> {
        let request_id = next_request_id("run");
        let (sender, receiver) = oneshot::channel();
        {
            let mut guard = self.runtime_pending.lock().await;
            guard.insert(request_id.clone(), sender);
        }
        self.send(ProviderTurnEvent::RuntimeCall {
            request_id: request_id.clone(),
            call,
        })
        .await;
        match receiver.await {
            Ok(result) => result,
            Err(_) => {
                let mut guard = self.runtime_pending.lock().await;
                guard.remove(&request_id);
                Err(RuntimeCallError::Cancelled)
            }
        }
    }

    /// Host-side: called by the runtime drain loop once the dispatcher
    /// has produced a result for the matching `RuntimeCall` event.
    pub async fn resolve_runtime_call(
        &self,
        request_id: &str,
        result: Result<RuntimeCallResult, RuntimeCallError>,
    ) {
        let mut guard = self.runtime_pending.lock().await;
        match guard.remove(request_id) {
            Some(sender) => {
                let _ = sender.send(result);
            }
            None => {
                tracing::warn!(
                    request_id,
                    "resolve_runtime_call: no pending sender for id — stale dispatch?"
                );
            }
        }
    }

    /// Host-side: called by the runtime when the user answers a question.
    pub async fn resolve_question(&self, request_id: &str, answers: Vec<UserInputAnswer>) {
        let mut guard = self.question_pending.lock().await;
        if let Some(sender) = guard.remove(request_id) {
            let _ = sender.send(answers);
        }
    }

    /// Host-side: called by the runtime when the user dismisses a question
    /// without answering. Dropping the sender causes the awaiting `ask_user`
    /// to return `None`, which each adapter translates into a provider-specific
    /// cancellation signal (JSON-RPC error, deny permission result, sentinel
    /// answer) so the model can proceed instead of hanging forever.
    pub async fn cancel_question(&self, request_id: &str) {
        let mut guard = self.question_pending.lock().await;
        guard.remove(request_id);
    }
}

fn next_request_id(prefix: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or_default();
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}-{nanos:x}-{n:x}")
}
