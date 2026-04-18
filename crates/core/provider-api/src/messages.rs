use serde::{Deserialize, Serialize};
use serde_json::Value;

// See `events.rs` for why this module re-uses the crate-root glob.
use crate::*;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-bindings", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-bindings", ts(optional_fields))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RuntimeEvent {
    RuntimeReady {
        message: String,
    },
    /// The daemon has entered graceful shutdown. Clients should show a
    /// banner, finish any in-progress UI interactions, and stop issuing
    /// new turn-starting commands.
    DaemonShuttingDown {
        reason: String,
    },
    SessionStarted {
        session: SessionSummary,
    },
    TurnStarted {
        session_id: String,
        turn: TurnRecord,
    },
    ContentDelta {
        session_id: String,
        turn_id: String,
        delta: String,
        accumulated_output: String,
    },
    ReasoningDelta {
        session_id: String,
        turn_id: String,
        delta: String,
    },
    ToolCallStarted {
        session_id: String,
        turn_id: String,
        call_id: String,
        name: String,
        args: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_call_id: Option<String>,
    },
    ToolCallCompleted {
        session_id: String,
        turn_id: String,
        call_id: String,
        output: String,
        error: Option<String>,
    },
    /// Heartbeat for an in-flight tool call. Mirrors
    /// `ProviderTurnEvent::ToolProgress`. Frontend uses
    /// `occurred_at` to refresh the per-tool stalled-tool pip; the
    /// matching `ToolCall::last_progress_at` field is also updated
    /// in the persisted `TurnRecord` so a reload mid-turn keeps the
    /// pip behavior consistent.
    ToolProgress {
        session_id: String,
        turn_id: String,
        call_id: String,
        tool_name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_call_id: Option<String>,
        occurred_at: String,
    },
    /// File rewind completed. Carries the lists of paths the
    /// runtime restored (had captured `before` snapshots) and
    /// deleted (created in the rewound span). Diff panel keys off
    /// this to refresh; toasts can read the totals to surface a
    /// "Reverted N files" message.
    FilesRewound {
        session_id: String,
        /// `turn_id` of the turn the user rewound to (the anchor
        /// they clicked). Frontend uses this to scroll / focus the
        /// originating user message.
        turn_id: String,
        paths_restored: Vec<String>,
        paths_deleted: Vec<String>,
    },
    TurnCompleted {
        session_id: String,
        session: SessionSummary,
        turn: TurnRecord,
    },
    SessionInterrupted {
        session: SessionSummary,
        message: String,
    },
    SessionDeleted {
        session_id: String,
    },
    PermissionRequested {
        session_id: String,
        turn_id: String,
        request_id: String,
        tool_name: String,
        input: Value,
        suggested: PermissionDecision,
    },
    UserQuestionAsked {
        session_id: String,
        turn_id: String,
        request_id: String,
        questions: Vec<UserInputQuestion>,
    },
    FileChanged {
        session_id: String,
        turn_id: String,
        call_id: String,
        path: String,
        operation: FileOperation,
        before: Option<String>,
        after: Option<String>,
    },
    SubagentStarted {
        session_id: String,
        turn_id: String,
        parent_call_id: String,
        agent_id: String,
        agent_type: String,
        prompt: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
    },
    SubagentEvent {
        session_id: String,
        turn_id: String,
        agent_id: String,
        event: Value,
    },
    SubagentCompleted {
        session_id: String,
        turn_id: String,
        agent_id: String,
        output: String,
        error: Option<String>,
    },
    /// Broadcast when `ProviderTurnEvent::SubagentModelObserved`
    /// fires — the frontend can upgrade its in-memory
    /// `SubagentRecord.model` so the subagent header shows the
    /// SDK-resolved pinned id rather than the planned catalog
    /// value. Persisted via the `SubagentRecord` itself; this event
    /// just nudges the streaming UI.
    SubagentModelObserved {
        session_id: String,
        turn_id: String,
        agent_id: String,
        model: String,
    },
    PlanProposed {
        session_id: String,
        turn_id: String,
        plan_id: String,
        title: String,
        steps: Vec<PlanStep>,
        raw: String,
    },
    /// Compaction started / finished in the provider. Carries the
    /// full merged block state each time an input event lands, so
    /// clients can re-render a single "Conversation recap" divider
    /// without tracking the boundary / summary pair themselves.
    CompactUpdated {
        session_id: String,
        turn_id: String,
        trigger: CompactTrigger,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pre_tokens: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        post_tokens: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        summary: Option<String>,
    },
    /// The SDK's memory-recall supervisor attached memories to the
    /// current turn.
    MemoryRecalled {
        session_id: String,
        turn_id: String,
        mode: MemoryRecallMode,
        memories: Vec<MemoryRecallItem>,
    },
    /// Turn-phase transition. Drives the working-indicator's
    /// secondary label. Runtime-core forwards these verbatim from
    /// the provider; it does not synthesize phases on its own, so
    /// providers that don't emit `StatusChanged` simply never have
    /// a phase label.
    TurnStatusChanged {
        session_id: String,
        turn_id: String,
        phase: TurnPhase,
    },
    /// Incremental token-usage snapshot for the in-flight turn.
    /// Fires on every provider-level usage update — for the Claude
    /// Agent SDK that's once per assistant message (per API call in
    /// the turn's tool loop), plus a final update at result time
    /// carrying cost and duration.
    ///
    /// The numerator fields (`input_tokens`, `cache_read_tokens`,
    /// `cache_write_tokens`) reflect the LATEST API call, not a sum
    /// across the turn. Summing cache reads across a long tool loop
    /// inflates the value far past the context window (each call
    /// re-reads the same cached prompt), which is how we used to
    /// render "51M / 1M" on long turns. `output_tokens` is the
    /// running sum for the turn since each call only reports its
    /// own output slice.
    ///
    /// Clients that render a live context indicator should replace
    /// the in-flight turn's `usage` with this payload and re-derive
    /// the display; the final `TurnCompleted.turn.usage` carries
    /// the same values with `total_cost_usd` / `duration_ms`
    /// populated.
    TurnUsageUpdated {
        session_id: String,
        turn_id: String,
        usage: TokenUsage,
    },
    /// Provider-level auto-retry in progress. Drives the banner
    /// that appears above the composer; cleared on the next
    /// assistant text delta or turn completion.
    TurnRetrying {
        session_id: String,
        turn_id: String,
        attempt: u32,
        max_retries: u32,
        retry_delay_ms: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error_status: Option<u16>,
        error: String,
    },
    /// Provider-predicted next user prompt. Drives the ghost-text
    /// suggestion overlay in the composer. Frontend keeps only the
    /// latest suggestion per session; overwritten on each new
    /// event, cleared on any composer keystroke or turn start.
    PromptSuggested {
        session_id: String,
        turn_id: String,
        suggestion: String,
    },
    Error {
        message: String,
    },
    Info {
        message: String,
    },
    ProviderModelsUpdated {
        provider: ProviderKind,
        models: Vec<ProviderModel>,
    },
    ProviderHealthUpdated {
        status: ProviderStatus,
    },
    /// A provider reported new rate-limit or plan-usage data. Keyed
    /// by bucket id so clients can replace prior values for the
    /// same bucket without losing others. Account-wide; not scoped
    /// to any session even though it rides on a turn's event stream.
    RateLimitUpdated {
        info: RateLimitInfo,
    },
    ProjectCreated {
        project: ProjectRecord,
    },
    ProjectDeleted {
        project_id: String,
        reassigned_session_ids: Vec<String>,
    },
    SessionProjectAssigned {
        session_id: String,
        project_id: Option<String>,
    },
    SessionModelUpdated {
        session_id: String,
        model: String,
    },
    SessionArchived {
        session_id: String,
    },
    SessionUnarchived {
        session: SessionSummary,
    },
    /// Full per-session command catalog — slash commands, sub-agents,
    /// and MCP servers — produced by
    /// [`ProviderAdapter::session_command_catalog`]. Fires on session
    /// start, session load, and explicit refresh. Replaces the prior
    /// payload for this `session_id`; clients key their cache by
    /// `session_id` and do id-equality short-circuits on
    /// `catalog.commands[].id` to avoid unnecessary popup re-renders.
    SessionCommandCatalogUpdated {
        session_id: String,
        catalog: CommandCatalog,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-bindings", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-bindings", ts(optional_fields))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    Ping,
    LoadSnapshot,
    LoadSession {
        session_id: String,
        /// Cap the number of turns returned to the most recent `limit`.
        /// Absent (the default) means "return every turn" — callers that
        /// don't care about long-thread payload size can keep using the
        /// original shape. Transports and UIs that want perceived-fast
        /// thread opens should pass a small positive value (e.g. 50)
        /// and lazy-load older turns on demand.
        #[serde(default)]
        limit: Option<usize>,
    },
    StartSession {
        provider: ProviderKind,
        #[serde(default)]
        model: Option<String>,
        #[serde(default)]
        project_id: Option<String>,
    },
    SendTurn {
        session_id: String,
        input: String,
        /// Pasted images attached to this turn. Each carries the raw
        /// base64 bytes so the runtime can persist them to disk and
        /// forward them to providers that support multimodal input.
        #[serde(default)]
        images: Vec<ImageAttachment>,
        #[serde(default)]
        permission_mode: Option<PermissionMode>,
        #[serde(default)]
        reasoning_effort: Option<ReasoningEffort>,
    },
    /// Fetch the full bytes of a persisted image attachment. The
    /// frontend calls this lazily when the user clicks a chip on a
    /// replayed turn; runtime-core reads the file from
    /// `<data_dir>/attachments/<uuid>.<ext>` and responds with
    /// `ServerMessage::Attachment`.
    GetAttachment {
        attachment_id: String,
    },
    InterruptTurn {
        session_id: String,
    },
    /// Switch the active session's permission mode mid-turn. The runtime
    /// forwards this to the session's adapter; for Claude Agent SDK
    /// sessions the bridge calls `query.setPermissionMode` on the live
    /// SDK Query, so the rest of the in-flight turn runs under the new
    /// mode. Adapters whose backend doesn't support mid-turn switching
    /// silently no-op and the new mode applies to the next turn.
    UpdatePermissionMode {
        session_id: String,
        permission_mode: PermissionMode,
    },
    /// Update per-session settings persisted in
    /// `sessions.provider_state_json.metadata`. Designed as a sparse
    /// envelope — only fields the user actually changed are present;
    /// `None` means "leave existing value untouched". Today the only
    /// field is `compact_custom_instructions`, but the envelope shape
    /// keeps this command stable as future per-session settings land.
    /// Settings take effect on the NEXT turn (the Claude SDK doesn't
    /// expose a mid-turn handle for system-prompt edits); runtime-core
    /// reads the metadata when constructing the next `send_prompt`
    /// call, so a user can edit and immediately fire a turn without
    /// any reload step.
    UpdateSessionSettings {
        session_id: String,
        /// `None` here means "no change to this field". To clear the
        /// value, pass `Some("".to_string())` — the empty string is
        /// the canonical "no instructions, use the default
        /// compaction prompt" signal.
        #[serde(default)]
        compact_custom_instructions: Option<String>,
    },
    /// Revert all on-disk file changes made by the chosen turn and
    /// every subsequent turn back to their pre-turn state. Uses the
    /// snapshots persistence already captures in
    /// `FileChangeRecord.before` — no SDK round-trip required, so
    /// the action works between turns and after a daemon restart.
    /// On success the runtime broadcasts a `FilesRewound` event;
    /// the diff panel listens for it and refreshes.
    RewindFiles {
        session_id: String,
        /// `turn_id` of the turn the user wants to "rewind to".
        /// Every file change recorded by THIS turn and any later
        /// turn is undone; the file's earliest captured `before`
        /// across that span wins. Files whose first record in the
        /// span has `before = None` (newly created in the span) are
        /// deleted instead of restored.
        turn_id: String,
    },
    DeleteSession {
        session_id: String,
    },
    AnswerPermission {
        session_id: String,
        request_id: String,
        decision: PermissionDecision,
        /// Optional permission-mode change to apply alongside the
        /// approval. The Claude SDK adapter forwards this to the
        /// bridge, which sets it on the `PermissionResult`'s
        /// `updatedPermissions` so the SDK applies the mode change
        /// AS PART OF accepting the tool call. This is the canonical
        /// way to swap modes when approving an `ExitPlanMode` —
        /// calling `setPermissionMode` separately doesn't make the
        /// model continue executing within the same turn.
        #[serde(default)]
        permission_mode_override: Option<PermissionMode>,
    },
    AnswerQuestion {
        session_id: String,
        request_id: String,
        answers: Vec<UserInputAnswer>,
    },
    CancelQuestion {
        session_id: String,
        request_id: String,
    },
    AcceptPlan {
        session_id: String,
        plan_id: String,
    },
    RejectPlan {
        session_id: String,
        plan_id: String,
    },
    RefreshModels {
        provider: ProviderKind,
    },
    /// Flip a provider's runtime enabled flag. Persisted to the
    /// `provider_enablement` table and broadcast via
    /// `ProviderHealthUpdated` so every connected client sees the
    /// new state. Disabled providers skip health checks and reject
    /// `SendTurn` — see `runtime-core::handle_client_message`.
    SetProviderEnabled {
        provider: ProviderKind,
        enabled: bool,
    },
    CreateProject {
        #[serde(default)]
        path: Option<String>,
    },
    DeleteProject {
        project_id: String,
    },
    AssignSessionToProject {
        session_id: String,
        #[serde(default)]
        project_id: Option<String>,
    },
    UpdateSessionModel {
        session_id: String,
        model: String,
    },
    ArchiveSession {
        session_id: String,
    },
    UnarchiveSession {
        session_id: String,
    },
    ListArchivedSessions,
    /// Ask the runtime to refetch the command catalog for a session.
    /// Triggered by the frontend when the user opens the slash-command
    /// popup, so disk changes (e.g. a new SKILL.md) appear without
    /// requiring a session reload. Runtime-core dedupes concurrent
    /// refreshes per session id.
    RefreshSessionCommands {
        session_id: String,
    },
    /// Request a per-category context-usage breakdown for the
    /// session. Runtime-core dispatches to the adapter's
    /// `get_context_usage` and responds with
    /// `ServerMessage::ContextUsage`. Fired lazily when the user
    /// opens the context popover — not streamed.
    GetContextUsage {
        session_id: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-bindings", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-bindings", ts(optional_fields))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    Welcome { bootstrap: BootstrapPayload },
    Snapshot { snapshot: AppSnapshot },
    SessionLoaded { session: SessionDetail },
    SessionCreated { session: SessionSummary },
    Pong,
    Ack { message: String },
    Event { event: RuntimeEvent },
    Error { message: String },
    ArchivedSessionsList { sessions: Vec<SessionSummary> },
    /// Response to `ClientMessage::GetAttachment`. Carries the full
    /// bytes of a persisted image.
    Attachment { data: AttachmentData },
    /// Response to `ClientMessage::GetContextUsage`. `breakdown` is
    /// `None` when the provider doesn't support the RPC (default
    /// `Ok(None)` from the adapter), which the frontend treats as
    /// "hide the popover". Errors surface via `ServerMessage::Error`.
    ContextUsage {
        session_id: String,
        breakdown: Option<ContextBreakdown>,
    },
}
