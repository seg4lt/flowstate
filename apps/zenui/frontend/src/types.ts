export type ProviderKind = "codex" | "claude" | "github_copilot" | "claude_cli" | "github_copilot_cli";
export type ProviderStatusLevel = "ready" | "warning" | "error";
export type SessionStatus = "ready" | "running" | "interrupted";
export type TurnStatus = "running" | "completed" | "interrupted" | "failed";
export type ToolCallStatus = "pending" | "completed" | "failed";
export type PermissionMode = "default" | "accept_edits" | "plan" | "bypass";
export type PermissionDecision = "allow" | "allow_always" | "deny" | "deny_always";
export type FileOperation = "write" | "edit" | "delete";
export type SubagentStatus = "running" | "completed" | "failed";
export type PlanStatus = "proposed" | "accepted" | "rejected";
export type ReasoningEffort = "minimal" | "low" | "medium" | "high";

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

export interface SubagentRecord {
  agentId: string;
  parentCallId: string;
  agentType: string;
  prompt: string;
  events?: unknown[];
  output?: string;
  error?: string;
  status: SubagentStatus;
}

export interface PendingPermission {
  sessionId: string;
  turnId: string;
  requestId: string;
  toolName: string;
  input: unknown;
  suggested: PermissionDecision;
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

export interface PendingQuestion {
  sessionId: string;
  turnId: string;
  requestId: string;
  questions: UserInputQuestion[];
}

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

export interface TurnRecord {
  turnId: string;
  input: string;
  output: string;
  status: TurnStatus;
  reasoning?: string;
  toolCalls?: ToolCall[];
  fileChanges?: FileChangeRecord[];
  subagents?: SubagentRecord[];
  plan?: PlanRecord;
  permissionMode?: PermissionMode;
  reasoningEffort?: ReasoningEffort;
  createdAt: string;
  updatedAt: string;
  /** Local-only: set on optimistic-echo turns, cleared when server version replaces it. */
  pendingId?: string;
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
  projectId?: string | null;
}

export interface SessionDetail {
  summary: SessionSummary;
  turns: TurnRecord[];
}

export interface ProjectRecord {
  projectId: string;
  name: string;
  createdAt: string;
  updatedAt: string;
  sortOrder: number;
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

// RuntimeEvent fields use snake_case to match Rust serde serialization.
export type RuntimeEvent =
  | { type: "runtime_ready"; message: string }
  | { type: "daemon_shutting_down"; reason: string }
  | { type: "session_started"; session: SessionSummary }
  | { type: "turn_started"; session_id: string; turn: TurnRecord }
  | {
      type: "content_delta";
      session_id: string;
      turn_id: string;
      delta: string;
      accumulated_output: string;
    }
  | { type: "reasoning_delta"; session_id: string; turn_id: string; delta: string }
  | {
      type: "tool_call_started";
      session_id: string;
      turn_id: string;
      call_id: string;
      name: string;
      args: unknown;
    }
  | {
      type: "tool_call_completed";
      session_id: string;
      turn_id: string;
      call_id: string;
      output: string;
      error?: string;
    }
  | { type: "turn_completed"; session_id: string; session: SessionSummary; turn: TurnRecord }
  | { type: "session_interrupted"; session: SessionSummary; message: string }
  | { type: "session_deleted"; session_id: string }
  | {
      type: "permission_requested";
      session_id: string;
      turn_id: string;
      request_id: string;
      tool_name: string;
      input: unknown;
      suggested: PermissionDecision;
    }
  | {
      type: "user_question_asked";
      session_id: string;
      turn_id: string;
      request_id: string;
      questions: UserInputQuestion[];
    }
  | {
      type: "file_changed";
      session_id: string;
      turn_id: string;
      call_id: string;
      path: string;
      operation: FileOperation;
      before?: string;
      after?: string;
    }
  | {
      type: "subagent_started";
      session_id: string;
      turn_id: string;
      parent_call_id: string;
      agent_id: string;
      agent_type: string;
      prompt: string;
    }
  | {
      type: "subagent_event";
      session_id: string;
      turn_id: string;
      agent_id: string;
      event: unknown;
    }
  | {
      type: "subagent_completed";
      session_id: string;
      turn_id: string;
      agent_id: string;
      output: string;
      error?: string;
    }
  | {
      type: "plan_proposed";
      session_id: string;
      turn_id: string;
      plan_id: string;
      title: string;
      steps: PlanStep[];
      raw: string;
    }
  | { type: "error"; message: string }
  | { type: "info"; message: string }
  | {
      type: "provider_models_updated";
      provider: ProviderKind;
      models: ProviderModel[];
    }
  | { type: "provider_health_updated"; status: ProviderStatus }
  | { type: "project_created"; project: ProjectRecord }
  | { type: "project_renamed"; project_id: string; name: string; updated_at: string }
  | {
      type: "project_deleted";
      project_id: string;
      reassigned_session_ids: string[];
    }
  | {
      type: "session_project_assigned";
      session_id: string;
      project_id: string | null;
    };

export type ServerMessage =
  | { type: "welcome"; bootstrap: BootstrapPayload }
  | { type: "snapshot"; snapshot: AppSnapshot }
  | { type: "session_loaded"; session: SessionDetail }
  | { type: "session_created"; session: SessionSummary }
  | { type: "pong" }
  | { type: "ack"; message: string }
  | { type: "event"; event: RuntimeEvent }
  | { type: "error"; message: string };

export type ClientMessage =
  | { type: "ping" }
  | { type: "load_snapshot" }
  | { type: "load_session"; session_id: string }
  | {
      type: "start_session";
      provider: ProviderKind;
      title: string | null;
      model?: string | null;
      project_id?: string | null;
    }
  | {
      type: "send_turn";
      session_id: string;
      input: string;
      permission_mode?: PermissionMode;
      reasoning_effort?: ReasoningEffort;
    }
  | { type: "interrupt_turn"; session_id: string }
  | { type: "delete_session"; session_id: string }
  | {
      type: "answer_permission";
      session_id: string;
      request_id: string;
      decision: PermissionDecision;
    }
  | {
      type: "answer_question";
      session_id: string;
      request_id: string;
      answers: UserInputAnswer[];
    }
  | {
      type: "cancel_question";
      session_id: string;
      request_id: string;
    }
  | { type: "accept_plan"; session_id: string; plan_id: string }
  | { type: "reject_plan"; session_id: string; plan_id: string }
  | { type: "refresh_models"; provider: ProviderKind }
  | { type: "create_project"; name: string }
  | { type: "rename_project"; project_id: string; name: string }
  | { type: "delete_project"; project_id: string }
  | {
      type: "assign_session_to_project";
      session_id: string;
      project_id: string | null;
    };

export const EMPTY_SNAPSHOT: AppSnapshot = {
  generatedAt: new Date(0).toISOString(),
  sessions: [],
  projects: [],
};

export const PROVIDER_COLORS: Record<ProviderKind, string> = {
  codex: "bg-emerald-500",
  claude: "bg-amber-500",
  github_copilot: "bg-blue-500",
  claude_cli: "bg-violet-500",
  github_copilot_cli: "bg-sky-500",
};

export const PROVIDER_LABELS: Record<ProviderKind, string> = {
  codex: "Codex",
  claude: "Claude",
  github_copilot: "GitHub Copilot",
  claude_cli: "Claude (CLI)",
  github_copilot_cli: "GitHub Copilot (CLI)",
};

export const PERMISSION_MODE_LABELS: Record<PermissionMode, string> = {
  accept_edits: "Auto-accept edits",
  default: "Ask before edits",
  plan: "Plan mode",
  bypass: "Full access",
};

export const REASONING_EFFORT_LABELS: Record<ReasoningEffort, string> = {
  minimal: "Minimal",
  low: "Low",
  medium: "Medium",
  high: "High",
};
