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
  events: unknown[];
  output?: string;
  error?: string;
  status: SubagentStatus;
}

export interface TurnRecord {
  turnId: string;
  input: string;
  output: string;
  status: TurnStatus;
  createdAt: string;
  updatedAt: string;
  reasoning?: string;
  toolCalls: ToolCall[];
  fileChanges: FileChangeRecord[];
  subagents: SubagentRecord[];
  plan?: PlanRecord;
  permissionMode?: PermissionMode;
  reasoningEffort?: ReasoningEffort;
}

export interface ProjectRecord {
  projectId: string;
  name: string;
  createdAt: string;
  updatedAt: string;
  sortOrder: number;
}

export interface SessionSummary {
  sessionId: string;
  provider: ProviderKind;
  title: string;
  status: SessionStatus;
  createdAt: string;
  updatedAt: string;
  lastTurnPreview: string | null;
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
  | { type: "load_session"; session_id: string }
  | { type: "start_session"; provider: ProviderKind; title?: string; model?: string; project_id?: string }
  | { type: "send_turn"; session_id: string; input: string; permission_mode?: PermissionMode; reasoning_effort?: ReasoningEffort }
  | { type: "interrupt_turn"; session_id: string }
  | { type: "delete_session"; session_id: string }
  | { type: "answer_permission"; session_id: string; request_id: string; decision: PermissionDecision }
  | { type: "answer_question"; session_id: string; request_id: string; answers: UserInputAnswer[] }
  | { type: "cancel_question"; session_id: string; request_id: string }
  | { type: "accept_plan"; session_id: string; plan_id: string }
  | { type: "reject_plan"; session_id: string; plan_id: string }
  | { type: "refresh_models"; provider: ProviderKind }
  | { type: "create_project"; name: string }
  | { type: "rename_project"; project_id: string; name: string }
  | { type: "delete_project"; project_id: string }
  | { type: "assign_session_to_project"; session_id: string; project_id?: string };

export type ServerMessage =
  | { type: "welcome"; bootstrap: BootstrapPayload }
  | { type: "snapshot"; snapshot: AppSnapshot }
  | { type: "session_loaded"; session: SessionDetail }
  | { type: "session_created"; session: SessionSummary }
  | { type: "pong" }
  | { type: "ack"; message: string }
  | { type: "event"; event: RuntimeEvent }
  | { type: "error"; message: string };

export type RuntimeEvent =
  | { type: "runtime_ready"; message: string }
  | { type: "daemon_shutting_down"; reason: string }
  | { type: "session_started"; session: SessionSummary }
  | { type: "turn_started"; session_id: string; turn: TurnRecord }
  | { type: "content_delta"; session_id: string; turn_id: string; delta: string; accumulated_output: string }
  | { type: "reasoning_delta"; session_id: string; turn_id: string; delta: string }
  | { type: "tool_call_started"; session_id: string; turn_id: string; call_id: string; name: string; args: unknown }
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
  | { type: "project_created"; project: ProjectRecord }
  | { type: "project_renamed"; project_id: string; name: string; updated_at: string }
  | { type: "project_deleted"; project_id: string; reassigned_session_ids: string[] }
  | { type: "session_project_assigned"; session_id: string; project_id?: string };
