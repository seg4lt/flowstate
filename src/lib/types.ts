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
export type ReasoningEffort = "minimal" | "low" | "medium" | "high";
export type FileOperation = "write" | "edit" | "delete";
export type SubagentStatus = "running" | "completed" | "failed";
export type PlanStatus = "proposed" | "accepted" | "rejected";

// --- Structs (camelCase fields) ---

export interface ProviderModel {
  value: string;
  label: string;
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
  | { kind: "tool_call"; callId: string };

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

  | { type: "create_project"; path?: string }
  | { type: "delete_project"; project_id: string }
  | { type: "assign_session_to_project"; session_id: string; project_id?: string }
  | { type: "update_session_model"; session_id: string; model: string }
  | { type: "archive_session"; session_id: string }
  | { type: "unarchive_session"; session_id: string }
  | { type: "list_archived_sessions" };

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
  | { type: "attachment"; data: AttachmentData };

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
  | { type: "subagent_started"; session_id: string; turn_id: string; parent_call_id: string; agent_id: string; agent_type: string; prompt: string }
  | { type: "subagent_event"; session_id: string; turn_id: string; agent_id: string; event: unknown }
  | { type: "subagent_completed"; session_id: string; turn_id: string; agent_id: string; output: string; error?: string }
  | { type: "plan_proposed"; session_id: string; turn_id: string; plan_id: string; title: string; steps: PlanStep[]; raw: string }
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
  | { type: "session_project_assigned"; session_id: string; project_id?: string };
