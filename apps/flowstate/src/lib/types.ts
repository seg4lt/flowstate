// Types mirroring rs-agent-sdk/crates/core/provider-api/src/lib.rs
//
// IMPORTANT serde conventions:
// - Structs (SessionSummary, TurnRecord, etc.) use #[serde(rename_all = "camelCase")]
//   → field names are camelCase in JSON
// - Enums (ClientMessage, RuntimeEvent, ServerMessage) use #[serde(tag = "type", rename_all = "snake_case")]
//   → the "type" tag values are snake_case, AND variant field names stay snake_case (no per-variant rename)

// --- Enums ---

export type ProviderKind =
  | "codex"
  | "claude"
  | "github_copilot"
  | "claude_cli"
  | "github_copilot_cli";

export type ProviderStatusLevel = "ready" | "warning" | "error";
export type SessionStatus = "ready" | "running" | "interrupted";
export type TurnStatus = "running" | "completed" | "interrupted" | "failed";
export type ToolCallStatus = "pending" | "completed" | "failed";
export type PermissionDecision = "allow" | "allow_always" | "deny" | "deny_always";
export type PermissionMode = "default" | "accept_edits" | "plan" | "bypass";

/** Coarse turn-phase signal emitted by providers that support the
 *  capability (`ProviderFeatures.statusLabels`). Drives the working-
 *  indicator's secondary label so long non-streaming pauses carry a
 *  label instead of looking stuck. Not all providers emit every
 *  phase — the UI treats missing events as `idle`. */
export type TurnPhase =
  | "idle"
  | "requesting"
  | "streaming"
  | "compacting"
  | "awaiting_input";

/** Snapshot of an in-flight provider-level auto-retry for a session.
 *  Populated from `turn_retrying` events; cleared on the next
 *  assistant text delta or turn completion. Drives the
 *  `api-retry-banner` above the composer. */
export interface RetryState {
  turnId: string;
  attempt: number;
  maxRetries: number;
  retryDelayMs: number;
  errorStatus?: number;
  error: string;
  /** Epoch ms of when the event landed, so the banner's countdown
   *  can render `retryDelayMs - (now - startedAt)`. */
  startedAt: number;
}
export type ReasoningEffort = "minimal" | "low" | "medium" | "high";
export type FileOperation = "write" | "edit" | "delete";
export type SubagentStatus = "running" | "completed" | "failed";
export type PlanStatus = "proposed" | "accepted" | "rejected";

// --- Structs (camelCase fields) ---

export interface ProviderModel {
  value: string;
  label: string;
  /** Authoritative context window in tokens for this model. When
   *  present, the UI prefers this over the SDK-reported
   *  `TokenUsage.contextWindow` (which can drift, e.g. Anthropic's
   *  1M beta auto-negotiation). Omitted when the adapter doesn't
   *  know the ceiling. */
  contextWindow?: number;
  /** Authoritative max output tokens for this model, when known. */
  maxOutputTokens?: number;
}

/** Where a user-authored SKILL.md came from on disk. Drives the
 * project / global badge next to skill entries in the slash popup. */
export type SkillSource = "disk_global" | "disk_project";

/** Discriminator for entries in a `CommandCatalog`. Flattened into
 * `ProviderCommand` on the wire — the Rust side uses
 * `#[serde(flatten)]` so the `kind` field lands at the top level of
 * the command object, and `UserSkill` additionally carries its
 * `source` alongside. */
export type CommandKind =
  | { kind: "builtin" }
  | { kind: "user_skill"; source: SkillSource }
  | { kind: "tui_only" };

/** One slash-command / skill entry for a given session. The frontend
 * renders one popup row per command. `id` is stable across sessions
 * for the same command; the store reducer compares `commands[].id`
 * arrays to short-circuit re-renders when a refresh returns the same
 * set. */
export type ProviderCommand = {
  id: string;
  name: string;
  description: string;
  userInvocable: boolean;
  argHint?: string;
} & CommandKind;

/** A sub-agent exposed by the provider (e.g. Claude SDK's
 * supportedAgents). Rendered in the popup under an "agent" badge. */
export interface ProviderAgent {
  id: string;
  name: string;
  description: string;
}

/** An MCP server the provider is aware of. Carried on the wire but
 * not rendered in the popup in v1 — reserved for future session-header
 * status chips. */
export interface McpServerInfo {
  id: string;
  name: string;
  enabled: boolean;
}

/** Full per-session capability enumeration: commands, sub-agents,
 * MCP servers. Broadcast as `session_command_catalog_updated`. */
export interface CommandCatalog {
  commands: ProviderCommand[];
  agents: ProviderAgent[];
  mcpServers: McpServerInfo[];
}

export interface ProviderStatus {
  kind: ProviderKind;
  label: string;
  installed: boolean;
  authenticated: boolean;
  version: string | null;
  status: ProviderStatusLevel;
  message: string | null;
  models: ProviderModel[];
  /** Cross-provider capability flags. Drives UI gating — hide the
   *  effort selector on providers whose adapter doesn't populate
   *  `thinkingEffort`, hide the rewind action when
   *  `fileCheckpoints` is off, etc. Optional on the wire for
   *  backward-compat with older daemon builds; readers should treat
   *  every flag as `false` when the object is missing. */
  features?: ProviderFeatures;
}

/** Mirrors `zenui_provider_api::ProviderFeatures`. Every flag here
 *  gates a user-visible affordance; the UI reads this object on the
 *  session's current provider and hides controls whose flag is
 *  unset. Adapters that don't opt into a feature leave its flag
 *  `false` / absent, so users never see a button that does nothing.
 *
 *  All fields are optional so the interface survives adding new
 *  flags without breaking TS consumers of older daemon broadcasts. */
export interface ProviderFeatures {
  statusLabels?: boolean;
  toolProgress?: boolean;
  apiRetries?: boolean;
  thinkingEffort?: boolean;
  contextBreakdown?: boolean;
  promptSuggestions?: boolean;
  fileCheckpoints?: boolean;
  compactCustomInstructions?: boolean;
  sessionLifecycleEvents?: boolean;
}

export interface PlanStep {
  title: string;
  detail?: string;
}

export interface PlanRecord {
  planId: string;
  title: string;
  steps: PlanStep[];
  raw: string;
  status: PlanStatus;
}

export interface ToolCall {
  callId: string;
  name: string;
  args: unknown;
  output?: string;
  error?: string;
  status: ToolCallStatus;
  // Parent Task/Agent call_id when this tool call was issued from
  // inside a sub-agent. Undefined for main-agent calls. Used by the
  // turn view to segment the tool-call stream into per-agent groups.
  parentCallId?: string;
  /** ISO 8601 timestamp of when the tool call was issued. Populated
   *  by runtime-core on `ToolCallStarted` for every provider; the UI
   *  renders a live elapsed counter ("Bash · 12s") from this while
   *  the call is still `pending`. Providers whose adapter doesn't
   *  opt into `ProviderFeatures.toolProgress` have the timer hidden
   *  at the UI level even though the field is populated. */
  startedAt?: string;
}

// Mirrors `zenui_provider_api::ContentBlock` — the canonical ordered
// content stream for an assistant turn. Tagged on `kind` (snake_case)
// because the Rust enum uses #[serde(tag = "kind", rename_all = "snake_case")].
// Tool-call blocks reference TurnRecord.toolCalls by callId — that's
// where mutable status/output live; the block itself only carries the
// stream position so interleaved "text → tool → text → tool" turns
// render in the order the provider emitted them.
export type ContentBlock =
  | { kind: "text"; text: string }
  | { kind: "reasoning"; text: string }
  | { kind: "tool_call"; callId: string }
  // Conversation-recap marker — the Claude Agent SDK compressed
  // older turns into a summary. `summary` is absent between receipt
  // of the compact_boundary system message (which has metrics only)
  // and the PostCompact hook firing (which carries the text);
  // runtime-core merges them before emitting.
  | {
      kind: "compact";
      trigger: "auto" | "manual";
      preTokens?: number;
      postTokens?: number;
      durationMs?: number;
      summary?: string;
    }
  // "Recalled from memory" marker — the SDK's memory-recall
  // supervisor attached memory files to the turn's context.
  | {
      kind: "memory_recall";
      mode: "select" | "synthesize";
      memories: MemoryRecallItem[];
    };

export interface MemoryRecallItem {
  path: string;
  scope: "personal" | "team";
  /** Present in 'synthesize' mode; absent in 'select' mode where
   *  renderers lazy-load the file body from `path` on demand. */
  content?: string;
}

/** Mirrors `zenui_provider_api::ContextBreakdown` — the response to
 *  `ClientMessage::GetContextUsage`. Providers that support context
 *  introspection (Claude SDK today) populate this; others return
 *  `null` and the UI hides the popover trigger via the
 *  `features.contextBreakdown` gate. */
export interface ContextBreakdown {
  totalTokens: number;
  maxTokens: number;
  categories: ContextCategory[];
}

export interface ContextCategory {
  name: string;
  tokens: number;
  /** Provider-supplied hex color for a stacked-bar segment (e.g.
   *  Claude SDK's palette). Optional — the UI falls back to a
   *  deterministic hash-based colour when absent. */
  color?: string;
}

export interface FileChangeRecord {
  callId: string;
  path: string;
  operation: FileOperation;
  before?: string;
  after?: string;
}

export interface SubagentRecord {
  agentId: string;
  parentCallId: string;
  agentType: string;
  prompt: string;
  /** Raw provider-level model id this subagent is running on. Set
   *  at spawn time from the provider's static agent catalog when
   *  available, and upgraded to the SDK-observed value on the first
   *  assistant message. Undefined when the provider doesn't
   *  distinguish per-subagent models. */
  model?: string;
  events: unknown[];
  output?: string;
  error?: string;
  status: SubagentStatus;
}

export interface TokenUsage {
  inputTokens: number;
  outputTokens: number;
  /** Tokens written to the provider's prompt cache this turn. */
  cacheWriteTokens?: number;
  /** Tokens read from the provider's prompt cache this turn. */
  cacheReadTokens?: number;
  /** Model's max context window in tokens, when the provider knows it. */
  contextWindow?: number;
  totalCostUsd?: number;
  durationMs?: number;
  model?: string;
}

export type RateLimitStatus = "allowed" | "allowed_warning" | "rejected";

export interface RateLimitInfo {
  /** Stable provider-defined id; used as the store map key. */
  bucket: string;
  /** Human-readable label, provided by the adapter. */
  label: string;
  status: RateLimitStatus;
  /** Fraction 0.0 - 1.0. */
  utilization: number;
  /** Unix ms when the bucket resets. Absent for non-resetting buckets. */
  resetsAt?: number;
  isUsingOverage?: boolean;
}

export interface TurnRecord {
  turnId: string;
  input: string;
  output: string;
  status: TurnStatus;
  createdAt: string;
  updatedAt: string;
  reasoning?: string;
  toolCalls?: ToolCall[];
  fileChanges?: FileChangeRecord[];
  subagents?: SubagentRecord[];
  plan?: PlanRecord;
  permissionMode?: PermissionMode;
  reasoningEffort?: ReasoningEffort;
  blocks?: ContentBlock[];
  /** References to images the user pasted on this turn. The full bytes
   * live on disk and are fetched lazily via `get_attachment` when the
   * user clicks a chip. */
  inputAttachments?: AttachmentRef[];
  /** Token accounting and cost, set once per turn from the provider's
   * final result message. Absent on interrupted/failed turns and on
   * providers that don't surface usage yet. */
  usage?: TokenUsage;
}

/** Pre-send (in-flux) image — lives in ChatInput state until submit.
 * Carries the raw base64 + an object URL for thumbnail rendering. */
export interface AttachedImage {
  /** Local UUID — React key + remove-by-id. */
  id: string;
  /** MIME type, e.g. "image/png". */
  mediaType: string;
  /** Standard base64 (no `data:` prefix). */
  dataBase64: string;
  /** Display name, e.g. "image.png". */
  name: string;
  /** Browser blob URL for rendering thumbnails / lightbox locally,
   * before the bytes hit the server. */
  previewUrl: string;
}

/** Persisted reference returned by the server on session load.
 * Lightweight — no bytes. The bytes are fetched on-demand via
 * `get_attachment` when the user clicks a chip. */
export interface AttachmentRef {
  /** UUID — also the filename (sans extension) on disk. */
  id: string;
  mediaType: string;
  name?: string;
  sizeBytes: number;
}

/** Full attachment payload returned by `get_attachment`. */
export interface AttachmentData {
  mediaType: string;
  dataBase64: string;
  name?: string;
}

// Display-only fields (`name`, `sortOrder`, `title`, `lastTurnPreview`)
// deliberately live on the app-side `user_config.sqlite` store via
// `SessionDisplay` / `ProjectDisplay` in `src/lib/api.ts`, not on these
// SDK types. See `rs-agent-sdk/crates/core/persistence/CLAUDE.md` for
// the boundary rule.
export interface ProjectRecord {
  projectId: string;
  path?: string;
  createdAt: string;
  updatedAt: string;
}

export interface SessionSummary {
  sessionId: string;
  provider: ProviderKind;
  status: SessionStatus;
  createdAt: string;
  updatedAt: string;
  turnCount: number;
  model?: string;
  projectId?: string;
}

export interface ProviderSessionState {
  nativeThreadId?: string;
  metadata?: unknown;
}

export interface SessionDetail {
  summary: SessionSummary;
  turns: TurnRecord[];
  providerState?: ProviderSessionState;
}

export interface AppSnapshot {
  generatedAt: string;
  sessions: SessionDetail[];
  projects: ProjectRecord[];
}

export interface BootstrapPayload {
  appName: string;
  generatedAt: string;
  wsUrl: string;
  providers: ProviderStatus[];
  snapshot: AppSnapshot;
}

export interface UserInputOption {
  id: string;
  label: string;
  description?: string;
}

export interface UserInputQuestion {
  id: string;
  text: string;
  header?: string;
  options: UserInputOption[];
  multiSelect: boolean;
  allowFreeform: boolean;
  isSecret: boolean;
}

export interface UserInputAnswer {
  questionId: string;
  optionIds: string[];
  answer: string;
}

// --- Wire messages (enum variant fields are snake_case) ---

export type ClientMessage =
  | { type: "ping" }
  | { type: "load_snapshot" }
  | { type: "load_session"; session_id: string; limit?: number }
  | { type: "start_session"; provider: ProviderKind; model?: string; project_id?: string }
  | { type: "send_turn"; session_id: string; input: string; images?: { media_type: string; data_base64: string; name?: string }[]; permission_mode?: PermissionMode; reasoning_effort?: ReasoningEffort }
  | { type: "get_attachment"; attachment_id: string }
  | { type: "interrupt_turn"; session_id: string }
  | { type: "update_permission_mode"; session_id: string; permission_mode: PermissionMode }
  | { type: "delete_session"; session_id: string }
  | { type: "answer_permission"; session_id: string; request_id: string; decision: PermissionDecision; permission_mode_override?: PermissionMode }
  | { type: "answer_question"; session_id: string; request_id: string; answers: UserInputAnswer[] }
  | { type: "cancel_question"; session_id: string; request_id: string }
  | { type: "accept_plan"; session_id: string; plan_id: string }
  | { type: "reject_plan"; session_id: string; plan_id: string }
  | { type: "refresh_models"; provider: ProviderKind }
  | { type: "set_provider_enabled"; provider: ProviderKind; enabled: boolean }
  | { type: "create_project"; path?: string }
  | { type: "delete_project"; project_id: string }
  | { type: "assign_session_to_project"; session_id: string; project_id?: string }
  | { type: "update_session_model"; session_id: string; model: string }
  | { type: "archive_session"; session_id: string }
  | { type: "unarchive_session"; session_id: string }
  | { type: "list_archived_sessions" }
  | { type: "refresh_session_commands"; session_id: string }
  | { type: "get_context_usage"; session_id: string };

export type ServerMessage =
  | { type: "welcome"; bootstrap: BootstrapPayload }
  | { type: "snapshot"; snapshot: AppSnapshot }
  | { type: "session_loaded"; session: SessionDetail }
  | { type: "session_created"; session: SessionSummary }
  | { type: "pong" }
  | { type: "ack"; message: string }
  | { type: "event"; event: RuntimeEvent }
  | { type: "error"; message: string }
  | { type: "archived_sessions_list"; sessions: SessionSummary[] }
  | { type: "attachment"; data: AttachmentData }
  | { type: "context_usage"; session_id: string; breakdown: ContextBreakdown | null };

export type RuntimeEvent =
  | { type: "runtime_ready"; message: string }
  | { type: "daemon_shutting_down"; reason: string }
  | { type: "session_started"; session: SessionSummary }
  | { type: "turn_started"; session_id: string; turn: TurnRecord }
  | { type: "content_delta"; session_id: string; turn_id: string; delta: string; accumulated_output: string }
  | { type: "reasoning_delta"; session_id: string; turn_id: string; delta: string }
  | { type: "tool_call_started"; session_id: string; turn_id: string; call_id: string; name: string; args: unknown; parent_call_id?: string }
  | { type: "tool_call_completed"; session_id: string; turn_id: string; call_id: string; output: string; error?: string }
  | { type: "turn_completed"; session_id: string; session: SessionSummary; turn: TurnRecord }
  | { type: "session_interrupted"; session: SessionSummary; message: string }
  | { type: "session_deleted"; session_id: string }
  | { type: "permission_requested"; session_id: string; turn_id: string; request_id: string; tool_name: string; input: unknown; suggested: PermissionDecision }
  | { type: "user_question_asked"; session_id: string; turn_id: string; request_id: string; questions: UserInputQuestion[] }
  | { type: "file_changed"; session_id: string; turn_id: string; call_id: string; path: string; operation: FileOperation; before?: string; after?: string }
  | { type: "subagent_started"; session_id: string; turn_id: string; parent_call_id: string; agent_id: string; agent_type: string; prompt: string; model?: string }
  | { type: "subagent_event"; session_id: string; turn_id: string; agent_id: string; event: unknown }
  | { type: "subagent_completed"; session_id: string; turn_id: string; agent_id: string; output: string; error?: string }
  | { type: "subagent_model_observed"; session_id: string; turn_id: string; agent_id: string; model: string }
  | { type: "plan_proposed"; session_id: string; turn_id: string; plan_id: string; title: string; steps: PlanStep[]; raw: string }
  | { type: "compact_updated"; session_id: string; turn_id: string; trigger: "auto" | "manual"; pre_tokens?: number; post_tokens?: number; duration_ms?: number; summary?: string }
  | { type: "memory_recalled"; session_id: string; turn_id: string; mode: "select" | "synthesize"; memories: MemoryRecallItem[] }
  | { type: "turn_status_changed"; session_id: string; turn_id: string; phase: TurnPhase }
  | { type: "turn_retrying"; session_id: string; turn_id: string; attempt: number; max_retries: number; retry_delay_ms: number; error_status?: number; error: string }
  | { type: "prompt_suggested"; session_id: string; turn_id: string; suggestion: string }
  | { type: "error"; message: string }
  | { type: "info"; message: string }
  | { type: "provider_models_updated"; provider: ProviderKind; models: ProviderModel[] }
  | { type: "provider_health_updated"; status: ProviderStatus }
  | { type: "rate_limit_updated"; info: RateLimitInfo }
  | { type: "session_model_updated"; session_id: string; model: string }
  | { type: "session_archived"; session_id: string }
  | { type: "session_unarchived"; session: SessionSummary }
  | { type: "project_created"; project: ProjectRecord }
  | { type: "project_deleted"; project_id: string; reassigned_session_ids: string[] }
  | { type: "session_project_assigned"; session_id: string; project_id?: string }
  | { type: "session_command_catalog_updated"; session_id: string; catalog: CommandCatalog };
