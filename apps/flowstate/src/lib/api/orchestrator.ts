// Typed wrapper around the loopback HTTP `/api/orchestrator/*`
// surface exposed by the Rust `kanban::http` router. Every call
// funnels through the `orchestrator_request` Tauri command which
// proxies to the daemon's loopback HTTP port.
//
// The Rust side is the source of truth for the wire shapes — types
// here mirror `crates/app-layer/src/kanban/model.rs`. When the
// flag is OFF, every endpoint except `/status` and
// `/feature-flag` returns 404; callers should generally not see
// those because the route is gated client-side.

import { invoke } from "@tauri-apps/api/core";

export type TaskState =
  | "Open"
  | "Triage"
  | "Ready"
  | "Code"
  | "AgentReview"
  | "HumanReview"
  | "Merge"
  | "Done"
  | "NeedsHuman"
  | "Cancelled";

export type SessionRole =
  | "triage"
  | "orchestrator"
  | "coder"
  | "reviewer"
  | "memory_seeder"
  | "memory_updater";

export type CommentAuthor =
  | "user"
  | "triage"
  | "orchestrator"
  | "reviewer"
  | "coder"
  | "system";

/** Mirrors `Task` in `kanban::model`. */
export interface Task {
  taskId: string;
  title: string;
  body: string;
  state: TaskState;
  projectId: string | null;
  worktreeProjectId: string | null;
  branch: string | null;
  orchestratorSessionId: string | null;
  needsHumanReason: string | null;
  createdAt: number;
  updatedAt: number;
}

export interface TaskComment {
  commentId: string;
  taskId: string;
  author: CommentAuthor;
  body: string;
  createdAt: number;
}

export interface TaskSession {
  sessionId: string;
  taskId: string;
  role: SessionRole;
  createdAt: number;
  retiredAt: number | null;
}

export interface KeyDirectory {
  path: string;
  note: string;
}

export interface ProjectMemory {
  projectId: string;
  purpose: string | null;
  languages: string[];
  keyDirectories: KeyDirectory[];
  conventions: string[];
  recentTaskThemes: string[];
  seededAt: number | null;
  updatedAt: number;
}

export interface OrchestratorStatus {
  featureEnabled: boolean;
  tickEnabled: boolean;
  tickIntervalMs: number;
  maxParallelTasks: number;
}

// ── internal proxy helper ─────────────────────────────────────────

async function call<T>(
  method: "GET" | "POST" | "PUT" | "DELETE",
  path: string,
  body?: unknown,
): Promise<T> {
  // The Rust-side `orchestrator_request` command stringifies HTTP
  // errors verbatim, including the response body. Callers can
  // `catch` and surface them in toasts.
  return invoke<T>("orchestrator_request", {
    method,
    path,
    body: body ?? null,
  });
}

// ── status + settings ─────────────────────────────────────────────

export function getStatus(): Promise<OrchestratorStatus> {
  return call<OrchestratorStatus>("GET", "/api/orchestrator/status");
}

export function setFeatureFlag(enabled: boolean): Promise<unknown> {
  return call("POST", "/api/orchestrator/feature-flag", { enabled });
}

export function setTickEnabled(enabled: boolean): Promise<unknown> {
  return call("POST", "/api/orchestrator/toggle", { enabled });
}

export function setTickIntervalMs(ms: number): Promise<unknown> {
  return call("POST", "/api/orchestrator/tick-interval", { ms });
}

export function setMaxParallelTasks(n: number): Promise<unknown> {
  return call("POST", "/api/orchestrator/max-parallel-tasks", { n });
}

// ── tasks ─────────────────────────────────────────────────────────

export function listTasks(): Promise<Task[]> {
  return call<Task[]>("GET", "/api/orchestrator/tasks");
}

export function getTask(taskId: string): Promise<Task> {
  return call<Task>("GET", `/api/orchestrator/tasks/${encodeURIComponent(taskId)}`);
}

export function createTask(body: string): Promise<Task> {
  return call<Task>("POST", "/api/orchestrator/tasks", { body });
}

export function deleteTask(taskId: string): Promise<unknown> {
  return call("DELETE", `/api/orchestrator/tasks/${encodeURIComponent(taskId)}`);
}

// ── comments ──────────────────────────────────────────────────────

export function listComments(taskId: string): Promise<TaskComment[]> {
  return call<TaskComment[]>(
    "GET",
    `/api/orchestrator/tasks/${encodeURIComponent(taskId)}/comments`,
  );
}

export function postComment(
  taskId: string,
  body: string,
  author?: CommentAuthor,
): Promise<TaskComment> {
  return call<TaskComment>(
    "POST",
    `/api/orchestrator/tasks/${encodeURIComponent(taskId)}/comments`,
    { body, author },
  );
}

// ── sessions ──────────────────────────────────────────────────────

export function listTaskSessions(taskId: string): Promise<TaskSession[]> {
  return call<TaskSession[]>(
    "GET",
    `/api/orchestrator/tasks/${encodeURIComponent(taskId)}/sessions`,
  );
}

// ── human-gate transitions ────────────────────────────────────────

export function approveHumanReview(taskId: string): Promise<unknown> {
  return call(
    "POST",
    `/api/orchestrator/tasks/${encodeURIComponent(taskId)}/approve`,
  );
}

export function resolveNeedsHuman(
  taskId: string,
  args: { nextState?: TaskState; comment?: string } = {},
): Promise<unknown> {
  return call(
    "POST",
    `/api/orchestrator/tasks/${encodeURIComponent(taskId)}/resolve`,
    {
      next_state: args.nextState,
      comment: args.comment,
    },
  );
}

export function cancelTask(taskId: string): Promise<unknown> {
  return call(
    "POST",
    `/api/orchestrator/tasks/${encodeURIComponent(taskId)}/cancel`,
  );
}

// ── project memory ────────────────────────────────────────────────

export function listMemory(): Promise<ProjectMemory[]> {
  return call<ProjectMemory[]>("GET", "/api/orchestrator/memory");
}

export function getMemory(projectId: string): Promise<ProjectMemory> {
  return call<ProjectMemory>(
    "GET",
    `/api/orchestrator/memory/${encodeURIComponent(projectId)}`,
  );
}

export function putMemory(
  projectId: string,
  memory: Omit<ProjectMemory, "projectId" | "seededAt" | "updatedAt">,
): Promise<unknown> {
  return call(
    "PUT",
    `/api/orchestrator/memory/${encodeURIComponent(projectId)}`,
    memory,
  );
}

// ── columns ───────────────────────────────────────────────────────

/** Ordered states shown as columns on the board. */
export const BOARD_COLUMNS: readonly TaskState[] = [
  "Open",
  "Triage",
  "Ready",
  "Code",
  "AgentReview",
  "HumanReview",
  "Merge",
  "Done",
  "NeedsHuman",
  "Cancelled",
];

export const COLUMN_LABEL: Record<TaskState, string> = {
  Open: "Open",
  Triage: "Triage",
  Ready: "Ready",
  Code: "Coding",
  AgentReview: "Agent Review",
  HumanReview: "Human Review",
  Merge: "Merge",
  Done: "Done",
  NeedsHuman: "Needs Human",
  Cancelled: "Cancelled",
};
