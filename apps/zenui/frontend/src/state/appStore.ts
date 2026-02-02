import { useEffect, useReducer, useRef } from "react";
import {
  EMPTY_SNAPSHOT,
  type AppSnapshot,
  type BootstrapPayload,
  type ClientMessage,
  type PendingPermission,
  type PendingQuestion,
  type PermissionMode,
  type ProjectRecord,
  type ProviderKind,
  type ReasoningEffort,
  type RuntimeEvent,
  type SessionDetail,
  type SessionSummary,
  type ToolCall,
  type ToolCallStatus,
  type TurnRecord,
  type FileChangeRecord,
  type SubagentRecord,
  type PlanRecord,
} from "../types";

export type ConnectionStatus =
  | "connected"
  | "connecting"
  | "reconnecting"
  | "disconnected";

export interface ComposerDraft {
  prompt: string;
  provider: ProviderKind;
  model: string | null;
  permissionMode: PermissionMode;
  reasoningEffort: ReasoningEffort;
}

export interface QuestionSelection {
  /** Ids of the currently-selected options (0..n for multi-select, 0..1 for single). */
  optionIds: string[];
  /** Free-text the user has typed; prefer this over option ids when non-empty. */
  freeformText: string;
}

export interface AppState {
  bootstrap: BootstrapPayload | null;
  snapshot: AppSnapshot;
  activeSessionId: string | null;
  activeProjectId: string | null;
  pendingPermissions: PendingPermission[];
  pendingQuestion: PendingQuestion | null;
  /** Per-question draft state keyed by question id. Cleared when pendingQuestion clears. */
  questionSelections: Record<string, QuestionSelection>;
  connectionStatus: ConnectionStatus;
  lastAction: string;
  expandedProjectIds: Record<string, boolean>;
  composer: ComposerDraft;
}

const INITIAL_STATE: AppState = {
  bootstrap: null,
  snapshot: EMPTY_SNAPSHOT,
  activeSessionId: null,
  activeProjectId: null,
  pendingPermissions: [],
  pendingQuestion: null,
  questionSelections: {},
  connectionStatus: "connecting",
  lastAction: "Ready",
  expandedProjectIds: {},
  composer: {
    prompt: "",
    provider: "claude",
    model: null,
    permissionMode: "accept_edits",
    reasoningEffort: "medium",
  },
};

type Listener = () => void;

let state: AppState = INITIAL_STATE;
const listeners = new Set<Listener>();

function notify() {
  for (const listener of listeners) listener();
}

function setState(updater: (prev: AppState) => AppState) {
  const next = updater(state);
  if (next !== state) {
    state = next;
    notify();
  }
}

export const appStore = {
  getState: () => state,
  subscribe: (listener: Listener) => {
    listeners.add(listener);
    return () => {
      listeners.delete(listener);
    };
  },
  setState,
};

/**
 * Subscribes to the store and re-renders on any change. Selectors are allowed
 * to return unstable references (new arrays / objects) — unlike
 * `useSyncExternalStore`, this hook does not require snapshot-stability.
 */
export function useAppStore<T>(selector: (state: AppState) => T): T {
  const [, forceUpdate] = useReducer((count: number) => count + 1, 0);
  const selectorRef = useRef(selector);
  selectorRef.current = selector;

  useEffect(() => {
    return appStore.subscribe(() => forceUpdate());
  }, []);

  return selectorRef.current(appStore.getState());
}

// ---------------------------------------------------------------------------
// Actions
// ---------------------------------------------------------------------------

export const actions = {
  loadBootstrap(bootstrap: BootstrapPayload) {
    setState((prev) => {
      const defaultProvider = bootstrap.providers[0]?.kind ?? prev.composer.provider;
      const defaultModel = bootstrap.providers[0]?.models[0]?.value ?? null;
      return {
        ...prev,
        bootstrap,
        snapshot: bootstrap.snapshot,
        connectionStatus: "connected",
        composer: {
          ...prev.composer,
          provider: defaultProvider,
          model: prev.composer.model ?? defaultModel,
        },
      };
    });
  },
  setSnapshot(snapshot: AppSnapshot) {
    setState((prev) => ({ ...prev, snapshot }));
  },
  /**
   * Replace an existing session's entry with a fully-hydrated detail
   * returned from `ClientMessage::LoadSession`. Bootstrap ships sessions
   * with empty turns, so this is how a session gets its turn history the
   * first time the user opens it.
   */
  mergeSessionDetail(detail: SessionDetail) {
    setState((prev) => ({
      ...prev,
      snapshot: {
        ...prev.snapshot,
        sessions: prev.snapshot.sessions.map((session) =>
          session.summary.sessionId !== detail.summary.sessionId ? session : detail,
        ),
      },
    }));
  },
  setConnectionStatus(status: ConnectionStatus) {
    setState((prev) => ({ ...prev, connectionStatus: status }));
  },
  setLastAction(message: string) {
    setState((prev) => ({ ...prev, lastAction: message }));
  },
  selectSession(sessionId: string | null) {
    setState((prev) => ({ ...prev, activeSessionId: sessionId }));
  },
  setPrompt(value: string) {
    setState((prev) => ({ ...prev, composer: { ...prev.composer, prompt: value } }));
  },
  setProviderAndModel(provider: ProviderKind, model: string | null) {
    setState((prev) => ({
      ...prev,
      composer: { ...prev.composer, provider, model },
    }));
  },
  setPermissionMode(mode: PermissionMode) {
    setState((prev) => ({
      ...prev,
      composer: { ...prev.composer, permissionMode: mode },
    }));
  },
  setReasoningEffort(effort: ReasoningEffort) {
    setState((prev) => ({
      ...prev,
      composer: { ...prev.composer, reasoningEffort: effort },
    }));
  },
  setQuestionOption(questionId: string, optionId: string, multi: boolean) {
    setState((prev) => {
      const current = prev.questionSelections[questionId] ?? {
        optionIds: [],
        freeformText: "",
      };
      let nextIds: string[];
      if (multi) {
        nextIds = current.optionIds.includes(optionId)
          ? current.optionIds.filter((id) => id !== optionId)
          : [...current.optionIds, optionId];
      } else {
        nextIds = current.optionIds[0] === optionId ? [] : [optionId];
      }
      return {
        ...prev,
        questionSelections: {
          ...prev.questionSelections,
          [questionId]: {
            optionIds: nextIds,
            // Picking an option clears any freeform — the two are mutually exclusive.
            freeformText: nextIds.length > 0 ? "" : current.freeformText,
          },
        },
      };
    });
  },
  setQuestionFreeform(questionId: string, text: string) {
    setState((prev) => {
      const current = prev.questionSelections[questionId] ?? {
        optionIds: [],
        freeformText: "",
      };
      return {
        ...prev,
        questionSelections: {
          ...prev.questionSelections,
          [questionId]: {
            // Typing clears any option selection.
            optionIds: text.length > 0 ? [] : current.optionIds,
            freeformText: text,
          },
        },
      };
    });
  },
  clearQuestion() {
    setState((prev) => ({
      ...prev,
      pendingQuestion: null,
      questionSelections: {},
    }));
  },
  toggleProjectExpanded(projectId: string) {
    setState((prev) => ({
      ...prev,
      expandedProjectIds: {
        ...prev.expandedProjectIds,
        [projectId]: !(prev.expandedProjectIds[projectId] ?? true),
      },
    }));
  },
  removePermission(requestId: string) {
    setState((prev) => ({
      ...prev,
      pendingPermissions: prev.pendingPermissions.filter(
        (p) => p.requestId !== requestId,
      ),
    }));
  },
  optimisticSendTurn(sessionId: string, input: string, pendingId: string) {
    const now = new Date().toISOString();
    setState((prev) => ({
      ...prev,
      snapshot: {
        ...prev.snapshot,
        sessions: prev.snapshot.sessions.map((session) =>
          session.summary.sessionId !== sessionId
            ? session
            : {
                summary: {
                  ...session.summary,
                  status: "running",
                  turnCount: session.summary.turnCount + 1,
                  lastTurnPreview: input.slice(0, 80),
                  updatedAt: now,
                },
                turns: [
                  ...session.turns,
                  {
                    turnId: pendingId,
                    input,
                    output: "",
                    status: "running",
                    createdAt: now,
                    updatedAt: now,
                    pendingId,
                  },
                ],
              },
        ),
      },
    }));
  },
  applyEvent(event: RuntimeEvent) {
    setState((prev) => applyRuntimeEvent(prev, event));
  },
};

// ---------------------------------------------------------------------------
// Event reducer
// ---------------------------------------------------------------------------

function applyRuntimeEvent(prev: AppState, event: RuntimeEvent): AppState {
  const matchRunning = (turn: TurnRecord, turnId: string) =>
    turn.turnId === turnId || turn.pendingId !== undefined;

  switch (event.type) {
    case "content_delta": {
      return withSessionTurns(prev, event.session_id, (turns) =>
        turns.map((turn) =>
          !matchRunning(turn, event.turn_id)
            ? turn
            : { ...turn, output: event.accumulated_output },
        ),
      );
    }
    case "reasoning_delta": {
      return withSessionTurns(prev, event.session_id, (turns) =>
        turns.map((turn) =>
          !matchRunning(turn, event.turn_id)
            ? turn
            : { ...turn, reasoning: (turn.reasoning ?? "") + event.delta },
        ),
      );
    }
    case "tool_call_started": {
      return withSessionTurns(prev, event.session_id, (turns) =>
        turns.map((turn) =>
          !matchRunning(turn, event.turn_id)
            ? turn
            : {
                ...turn,
                toolCalls: [
                  ...(turn.toolCalls ?? []),
                  {
                    callId: event.call_id,
                    name: event.name,
                    args: event.args,
                    status: "pending" as ToolCallStatus,
                  } satisfies ToolCall,
                ],
              },
        ),
      );
    }
    case "tool_call_completed": {
      return withSessionTurns(prev, event.session_id, (turns) =>
        turns.map((turn) =>
          !matchRunning(turn, event.turn_id)
            ? turn
            : {
                ...turn,
                toolCalls: (turn.toolCalls ?? []).map((tc) =>
                  tc.callId !== event.call_id
                    ? tc
                    : {
                        ...tc,
                        output: event.output,
                        error: event.error,
                        status: (event.error ? "failed" : "completed") as ToolCallStatus,
                      },
                ),
              },
        ),
      );
    }
    case "file_changed": {
      return withSessionTurns(prev, event.session_id, (turns) =>
        turns.map((turn) => {
          if (!matchRunning(turn, event.turn_id)) return turn;
          const next: FileChangeRecord = {
            callId: event.call_id,
            path: event.path,
            operation: event.operation,
            before: event.before,
            after: event.after,
          };
          const existing = turn.fileChanges ?? [];
          const idx = existing.findIndex((x) => x.callId === event.call_id);
          return {
            ...turn,
            fileChanges:
              idx >= 0
                ? existing.map((x, i) => (i === idx ? next : x))
                : [...existing, next],
          };
        }),
      );
    }
    case "subagent_started": {
      return withSessionTurns(prev, event.session_id, (turns) =>
        turns.map((turn) => {
          if (!matchRunning(turn, event.turn_id)) return turn;
          const existing = turn.subagents ?? [];
          if (existing.some((x) => x.agentId === event.agent_id)) return turn;
          const record: SubagentRecord = {
            agentId: event.agent_id,
            parentCallId: event.parent_call_id,
            agentType: event.agent_type,
            prompt: event.prompt,
            events: [],
            status: "running",
          };
          return { ...turn, subagents: [...existing, record] };
        }),
      );
    }
    case "subagent_event": {
      return withSessionTurns(prev, event.session_id, (turns) =>
        turns.map((turn) => {
          if (!matchRunning(turn, event.turn_id)) return turn;
          const existing = turn.subagents ?? [];
          return {
            ...turn,
            subagents: existing.map((x) =>
              x.agentId !== event.agent_id
                ? x
                : { ...x, events: [...(x.events ?? []), event.event] },
            ),
          };
        }),
      );
    }
    case "subagent_completed": {
      return withSessionTurns(prev, event.session_id, (turns) =>
        turns.map((turn) => {
          if (!matchRunning(turn, event.turn_id)) return turn;
          const existing = turn.subagents ?? [];
          return {
            ...turn,
            subagents: existing.map((x) =>
              x.agentId !== event.agent_id
                ? x
                : {
                    ...x,
                    output: event.output,
                    error: event.error,
                    status: event.error ? "failed" : "completed",
                  },
            ),
          };
        }),
      );
    }
    case "plan_proposed": {
      return withSessionTurns(prev, event.session_id, (turns) =>
        turns.map((turn) => {
          if (!matchRunning(turn, event.turn_id)) return turn;
          const plan: PlanRecord = {
            planId: event.plan_id,
            title: event.title,
            steps: event.steps,
            raw: event.raw,
            status: "proposed",
          };
          return { ...turn, plan };
        }),
      );
    }
    case "turn_started": {
      return {
        ...prev,
        snapshot: {
          ...prev.snapshot,
          sessions: prev.snapshot.sessions.map((session) => {
            if (session.summary.sessionId !== event.session_id) return session;
            const hasPending = session.turns.some((t) => t.pendingId !== undefined);
            if (hasPending) {
              return {
                ...session,
                turns: session.turns.map((t) =>
                  t.pendingId === undefined
                    ? t
                    : { ...t, turnId: event.turn.turnId, pendingId: undefined },
                ),
              };
            }
            return { ...session, turns: [...session.turns, event.turn] };
          }),
        },
      };
    }
    case "turn_completed": {
      return {
        ...prev,
        snapshot: {
          ...prev.snapshot,
          sessions: prev.snapshot.sessions.map((session) => {
            if (session.summary.sessionId !== event.session_id) return session;
            return {
              ...session,
              summary: { ...session.summary, ...event.session },
              turns: session.turns.map((turn) =>
                turn.turnId !== event.turn.turnId
                  ? turn
                  : {
                      ...turn,
                      status: event.turn.status,
                      output: event.turn.output,
                      updatedAt: event.turn.updatedAt,
                      reasoning: event.turn.reasoning ?? turn.reasoning,
                      toolCalls: event.turn.toolCalls ?? turn.toolCalls,
                      fileChanges: event.turn.fileChanges ?? turn.fileChanges,
                      subagents: event.turn.subagents ?? turn.subagents,
                      plan: event.turn.plan ?? turn.plan,
                    },
              ),
            };
          }),
        },
      };
    }
    case "session_started": {
      // If bootstrap already contains the session (optimistic path), skip; else append.
      const exists = prev.snapshot.sessions.some(
        (s) => s.summary.sessionId === event.session.sessionId,
      );
      if (exists) return prev;
      return {
        ...prev,
        snapshot: {
          ...prev.snapshot,
          sessions: [
            ...prev.snapshot.sessions,
            { summary: event.session, turns: [] },
          ],
        },
      };
    }
    case "session_interrupted": {
      return {
        ...prev,
        snapshot: {
          ...prev.snapshot,
          sessions: prev.snapshot.sessions.map((session) =>
            session.summary.sessionId !== event.session.sessionId
              ? session
              : { ...session, summary: event.session },
          ),
        },
      };
    }
    case "session_deleted": {
      const remaining = prev.snapshot.sessions.filter(
        (s) => s.summary.sessionId !== event.session_id,
      );
      return {
        ...prev,
        snapshot: { ...prev.snapshot, sessions: remaining },
        activeSessionId:
          prev.activeSessionId === event.session_id ? null : prev.activeSessionId,
      };
    }
    case "permission_requested": {
      return {
        ...prev,
        pendingPermissions: [
          ...prev.pendingPermissions,
          {
            sessionId: event.session_id,
            turnId: event.turn_id,
            requestId: event.request_id,
            toolName: event.tool_name,
            input: event.input,
            suggested: event.suggested,
          },
        ],
      };
    }
    case "user_question_asked": {
      const selections: Record<string, QuestionSelection> = {};
      for (const q of event.questions) {
        selections[q.id] = { optionIds: [], freeformText: "" };
      }
      return {
        ...prev,
        pendingQuestion: {
          sessionId: event.session_id,
          turnId: event.turn_id,
          requestId: event.request_id,
          questions: event.questions,
        },
        questionSelections: selections,
      };
    }
    case "provider_models_updated": {
      if (!prev.bootstrap) return prev;
      return {
        ...prev,
        bootstrap: {
          ...prev.bootstrap,
          providers: prev.bootstrap.providers.map((p) =>
            p.kind === event.provider ? { ...p, models: event.models } : p,
          ),
        },
        lastAction: `Refreshed ${event.provider} models (${event.models.length})`,
      };
    }
    case "project_created": {
      return {
        ...prev,
        snapshot: {
          ...prev.snapshot,
          projects: [...prev.snapshot.projects, event.project],
        },
        expandedProjectIds: {
          ...prev.expandedProjectIds,
          [event.project.projectId]: true,
        },
      };
    }
    case "project_renamed": {
      return {
        ...prev,
        snapshot: {
          ...prev.snapshot,
          projects: prev.snapshot.projects.map((p) =>
            p.projectId !== event.project_id
              ? p
              : { ...p, name: event.name, updatedAt: event.updated_at },
          ),
        },
      };
    }
    case "project_deleted": {
      const reassigned = new Set(event.reassigned_session_ids);
      return {
        ...prev,
        snapshot: {
          ...prev.snapshot,
          projects: prev.snapshot.projects.filter(
            (p) => p.projectId !== event.project_id,
          ),
          sessions: prev.snapshot.sessions.map((session) =>
            reassigned.has(session.summary.sessionId)
              ? { ...session, summary: { ...session.summary, projectId: null } }
              : session,
          ),
        },
      };
    }
    case "session_project_assigned": {
      return {
        ...prev,
        snapshot: {
          ...prev.snapshot,
          sessions: prev.snapshot.sessions.map((session) =>
            session.summary.sessionId !== event.session_id
              ? session
              : {
                  ...session,
                  summary: { ...session.summary, projectId: event.project_id },
                },
          ),
        },
      };
    }
    case "error": {
      return { ...prev, lastAction: `Error: ${event.message}` };
    }
    case "daemon_shutting_down": {
      return {
        ...prev,
        lastAction: `Daemon shutting down: ${event.reason}`,
      };
    }
    default:
      return prev;
  }
}

function withSessionTurns(
  prev: AppState,
  sessionId: string,
  transform: (turns: TurnRecord[]) => TurnRecord[],
): AppState {
  return {
    ...prev,
    snapshot: {
      ...prev.snapshot,
      sessions: prev.snapshot.sessions.map((session) =>
        session.summary.sessionId !== sessionId
          ? session
          : { ...session, turns: transform(session.turns) },
      ),
    },
  };
}

// ---------------------------------------------------------------------------
// Selectors
// ---------------------------------------------------------------------------

export function selectActiveSession(state: AppState): SessionDetail | null {
  if (!state.activeSessionId) return null;
  return (
    state.snapshot.sessions.find(
      (s) => s.summary.sessionId === state.activeSessionId,
    ) ?? null
  );
}

export interface ProjectGroup {
  project: ProjectRecord | null;
  sessions: SessionDetail[];
}

/**
 * Groups sessions by project, sorted by project.sort_order, with an
 * "Unassigned" bucket (project = null) for sessions without a project_id.
 * Sessions within each group are sorted by updatedAt DESC.
 */
export function selectProjectGroups(state: AppState): ProjectGroup[] {
  const byProjectId = new Map<string | null, SessionDetail[]>();
  for (const session of state.snapshot.sessions) {
    const key = session.summary.projectId ?? null;
    const list = byProjectId.get(key) ?? [];
    list.push(session);
    byProjectId.set(key, list);
  }
  for (const sessions of byProjectId.values()) {
    sessions.sort((a, b) =>
      b.summary.updatedAt.localeCompare(a.summary.updatedAt),
    );
  }
  const projectGroups: ProjectGroup[] = state.snapshot.projects.map((project) => ({
    project,
    sessions: byProjectId.get(project.projectId) ?? [],
  }));
  const unassigned = byProjectId.get(null) ?? [];
  if (unassigned.length > 0 || projectGroups.length === 0) {
    projectGroups.push({ project: null, sessions: unassigned });
  }
  return projectGroups;
}

export function selectProviderStatuses(state: AppState) {
  return state.bootstrap?.providers ?? [];
}

export type { SessionSummary, SessionDetail, TurnRecord };
export type SendClientMessage = (message: ClientMessage) => void;
