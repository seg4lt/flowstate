import { invoke } from "@tauri-apps/api/core";

// Per-session and per-project display metadata: titles, names,
// previews, ordering. Persisted in the same `user_config.sqlite`
// file as the kv store, in dedicated tables. The agent SDK
// no longer stores any of this — it only persists fields the
// runtime needs to execute or resume agents. See
// `rs-agent-sdk/crates/core/persistence/CLAUDE.md` for the boundary.

export interface SessionDisplay {
  title: string | null;
  lastTurnPreview: string | null;
}

export interface ProjectDisplay {
  name: string | null;
  sortOrder: number | null;
}

export function setSessionDisplay(
  sessionId: string,
  display: SessionDisplay,
): Promise<void> {
  return invoke<void>("set_session_display", { sessionId, display });
}

export function getSessionDisplay(
  sessionId: string,
): Promise<SessionDisplay | null> {
  return invoke<SessionDisplay | null>("get_session_display", { sessionId });
}

export function listSessionDisplay(): Promise<Record<string, SessionDisplay>> {
  return invoke<Record<string, SessionDisplay>>("list_session_display");
}

export function deleteSessionDisplay(sessionId: string): Promise<void> {
  return invoke<void>("delete_session_display", { sessionId });
}

export function setProjectDisplay(
  projectId: string,
  display: ProjectDisplay,
): Promise<void> {
  return invoke<void>("set_project_display", { projectId, display });
}

export function getProjectDisplay(
  projectId: string,
): Promise<ProjectDisplay | null> {
  return invoke<ProjectDisplay | null>("get_project_display", { projectId });
}

export function listProjectDisplay(): Promise<Record<string, ProjectDisplay>> {
  return invoke<Record<string, ProjectDisplay>>("list_project_display");
}

export function deleteProjectDisplay(projectId: string): Promise<void> {
  return invoke<void>("delete_project_display", { projectId });
}

// Parent/child worktree links. Each worktree has its own SDK
// project so the agent SDK's existing cwd resolution "just works",
// and this table records "this SDK project is a git worktree of
// that SDK project, on branch Z". The flowstate sidebar reads these
// to group worktree threads under the parent project visually —
// the SDK has no concept of worktrees.
export interface ProjectWorktree {
  projectId: string;
  parentProjectId: string;
  branch: string | null;
}

export function setProjectWorktree(
  projectId: string,
  parentProjectId: string,
  branch: string | null,
): Promise<void> {
  return invoke<void>("set_project_worktree", {
    projectId,
    parentProjectId,
    branch,
  });
}

export function getProjectWorktree(
  projectId: string,
): Promise<ProjectWorktree | null> {
  return invoke<ProjectWorktree | null>("get_project_worktree", { projectId });
}

export function listProjectWorktree(): Promise<Record<string, ProjectWorktree>> {
  return invoke<Record<string, ProjectWorktree>>("list_project_worktree");
}

export function deleteProjectWorktree(projectId: string): Promise<void> {
  return invoke<void>("delete_project_worktree", { projectId });
}
