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
    /// Emitted after a successful checkpoint capture at turn end.
    /// Frontends track the `(session_id, turn_id)` pairs they've seen
    /// via this event so the "Revert since here" affordance only lights
    /// up on turns that actually have a restorable checkpoint. Consumers
    /// that don't care about checkpoints can ignore the event.
    CheckpointCaptured {
        session_id: String,
        turn_id: String,
    },
    /// Emitted after `SetCheckpointsEnabled` mutates the settings so
    /// every connected client refreshes in lockstep — the daemon is
    /// the single source of truth and the store-sourced cache on each
    /// client would otherwise drift across app windows.
    CheckpointEnablementChanged {
        settings: CheckpointSettings,
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
    /// Cross-session link event. Emitted whenever the orchestration
    /// dispatcher creates a new session on behalf of an agent, or
    /// delivers a message from one session to another. The frontend
    /// uses this to render a "spawned by agent" badge on the child row
    /// in the sidebar and a "waiting on peer" indicator on the parent's
    /// in-flight tool call. `reason` is a short machine tag (`spawn`
    /// / `send`) so the UI can style the two cases differently without
    /// string-matching.
    SessionLinked {
        from_session_id: String,
        to_session_id: String,
        reason: SessionLinkReason,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "ts-bindings", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
pub enum SessionLinkReason {
    /// Child session was just created by the parent's agent.
    Spawn,
    /// Parent's agent sent a message to an existing child session.
    Send,
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
        /// Per-turn thinking-mode dial orthogonal to
        /// `reasoning_effort`. Only honoured by the Claude Agent SDK
        /// adapter today; others ignore. Absent = adapter default
        /// (`ThinkingMode::Always` for Claude SDK).
        #[serde(default)]
        thinking_mode: Option<ThinkingMode>,
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
    /// Atomic "steer": cooperatively interrupt the in-flight turn (if
    /// any) and, once the bridge confirms the turn has unwound, dispatch
    /// `input` as the next turn on the same session. Collapses the
    /// previous two-RPC interrupt→send dance into a single daemon-side
    /// operation so the frontend can't race itself against the bridge's
    /// `turnInProgress` guard.
    ///
    /// Payload mirrors `SendTurn` exactly — the eventual dispatch after
    /// the interrupt unwinds is a normal `send_turn` under the hood.
    SteerTurn {
        session_id: String,
        input: String,
        #[serde(default)]
        images: Vec<ImageAttachment>,
        #[serde(default)]
        permission_mode: Option<PermissionMode>,
        #[serde(default)]
        reasoning_effort: Option<ReasoningEffort>,
        #[serde(default)]
        thinking_mode: Option<ThinkingMode>,
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
        /// Optional free-form feedback surfaced to the model when the
        /// user denies a tool call. Threaded through to the Claude SDK
        /// adapter as the `message` field of `{behavior:'deny', message}`
        /// on the `PermissionResult`, which the model sees as the
        /// tool_result denial reason and can iterate on within the same
        /// turn. Primarily intended for `ExitPlanMode` rejections where
        /// the user wants to steer the plan without restarting the turn.
        #[serde(default)]
        reason: Option<String>,
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
    /// Rewind the session's workspace to its state just before
    /// `turn_id`. Uses the content-addressed checkpoint store that
    /// captured a snapshot at each turn's end.
    ///
    /// The call is safe (non-destructive) when `dry_run: true` (the
    /// runtime reports the set of paths it WOULD touch without writing
    /// anything) or when `confirm_conflicts: false` AND the rewind
    /// would clobber files this session has seen modified elsewhere
    /// since (the runtime returns `NeedsConfirmation`). Clients MUST
    /// surface the conflict list to the user before re-issuing with
    /// `confirm_conflicts: true`.
    RewindFiles {
        session_id: String,
        turn_id: String,
        #[serde(default)]
        dry_run: bool,
        #[serde(default)]
        confirm_conflicts: bool,
    },
    /// Flip the global checkpoint-enablement flag. The runtime
    /// enforces the new value at capture time; disabled skips
    /// capture entirely, and `RewindFiles` surfaces `Disabled` so
    /// the UI can nudge the user back to the settings toggle.
    SetCheckpointsEnabled { enabled: bool },
    /// Read the current checkpoint-settings snapshot. The same data
    /// ships on the `BootstrapPayload`, so the frontend only needs
    /// this for explicit re-syncs (e.g. after a settings dialog reopens
    /// and the app wants to confirm its cache is still accurate).
    GetCheckpointSettings,
}

/// The checkpoint-settings snapshot the daemon ships on
/// `BootstrapPayload` and in response to `GetCheckpointSettings`.
/// One field today; wrapped in a struct so additive changes
/// (telemetry opt-outs, per-provider overrides, etc.) land without
/// breaking the wire contract.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-bindings", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct CheckpointSettings {
    pub global_enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-bindings", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-bindings", ts(optional_fields))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    Welcome {
        bootstrap: BootstrapPayload,
    },
    Snapshot {
        snapshot: AppSnapshot,
    },
    SessionLoaded {
        session: SessionDetail,
    },
    SessionCreated {
        session: SessionSummary,
    },
    Pong,
    Ack {
        message: String,
    },
    Event {
        event: RuntimeEvent,
    },
    Error {
        message: String,
    },
    ArchivedSessionsList {
        sessions: Vec<SessionSummary>,
    },
    /// Response to `ClientMessage::GetAttachment`. Carries the full
    /// bytes of a persisted image.
    Attachment {
        data: AttachmentData,
    },
    /// Response to `ClientMessage::GetContextUsage`. `breakdown` is
    /// `None` when the provider doesn't support the RPC (default
    /// `Ok(None)` from the adapter), which the frontend treats as
    /// "hide the popover". Errors surface via `ServerMessage::Error`.
    ContextUsage {
        session_id: String,
        breakdown: Option<ContextBreakdown>,
    },
    /// Response to `ClientMessage::RewindFiles`. Always echoes back
    /// the `session_id` and `turn_id` from the request so the frontend
    /// can match responses to pending requests without a dedicated
    /// request-id channel. See [`RewindOutcomeWire`] for the three
    /// possible shapes.
    ///
    /// Infrastructure failures (missing session, IO error) surface via
    /// `ServerMessage::Error` instead — `RewindFilesResult` always
    /// reflects a clean semantic outcome, never an exception.
    RewindFilesResult {
        session_id: String,
        turn_id: String,
        outcome: RewindOutcomeWire,
    },
    /// Response to `ClientMessage::GetCheckpointSettings` and to
    /// `SetCheckpointsEnabled`. Carries the full current snapshot so
    /// the UI doesn't have to recompute inheritance.
    CheckpointSettingsSnapshot {
        settings: CheckpointSettings,
    },
}

/// Result of a `RewindFiles` call, in one of three mutually-exclusive
/// shapes. The frontend does an exhaustive `switch(outcome.kind)` so
/// TypeScript catches missing cases at compile time.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-bindings", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-bindings", ts(optional_fields))]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum RewindOutcomeWire {
    /// Rewind applied successfully. When `dry_run: true` was on the
    /// request, no disk writes happened — this variant reports the
    /// paths that WOULD change. In both cases the three path lists
    /// classify what the rewind touched:
    ///
    /// - `paths_restored`: files overwritten with their captured
    ///   pre-turn content.
    /// - `paths_deleted`: files that were created during the rewound
    ///   span, deleted here so they no longer exist.
    /// - `paths_skipped`: files this session touched but for which no
    ///   pre-turn hash was captured (first-touch with nothing prior).
    ///   These are left on disk as-is.
    Applied {
        paths_restored: Vec<String>,
        paths_deleted: Vec<String>,
        paths_skipped: Vec<String>,
        dry_run: bool,
    },
    /// One or more files have been modified outside this session
    /// since it last observed them. The rewind would clobber those
    /// changes; the client must prompt the user and re-issue the
    /// request with `confirm_conflicts: true` to proceed.
    NeedsConfirmation { conflicts: Vec<RewindConflictWire> },
    /// No rewind was possible — the client UI should hide or disable
    /// the affordance. `reason` distinguishes the three cases so the
    /// UX can tell the user WHY.
    Unavailable { reason: RewindUnavailableReason },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-bindings", derive(ts_rs::TS))]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum RewindUnavailableReason {
    /// The target turn has no captured checkpoint. Usually means the
    /// session predates the checkpoint feature OR the capture failed
    /// silently at the time (e.g. IO error on the blob store).
    NoCheckpoint,
    /// The session has no `cwd` — nothing to snapshot. Pure runtime-
    /// only threads (no attached project) never get checkpointing.
    NoWorkspace,
    /// The requested session id didn't resolve to a live session.
    /// Could be a race with session delete, or a stale client.
    SessionNotFound,
    /// Checkpoints are disabled for this session's scope (global or
    /// per-project setting, see PR 5.5). Surfacing the reason lets the
    /// UI nudge the user to the settings toggle.
    Disabled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-bindings", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-bindings", ts(optional_fields))]
#[serde(rename_all = "camelCase")]
pub struct RewindConflictWire {
    /// Path relative to the session's workspace root.
    pub path: String,
    /// Hash this session expected the file to have (its last observed
    /// `post_hash`). `None` means the session expected the file not
    /// to exist.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_last_seen_hash: Option<String>,
    /// Hash of the current on-disk content. `None` means the file is
    /// missing from disk right now.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_current_hash: Option<String>,
}
