import * as React from "react";
import { connectStream, sendMessage } from "@/lib/api";
import type {
  ClientMessage,
  PermissionDecision,
  ProviderStatus,
  ProjectRecord,
  RuntimeEvent,
  ServerMessage,
  SessionSummary,
  UserInputQuestion,
} from "@/lib/types";

/** Single permission prompt awaiting the user's answer. */
export interface PendingPermission {
  requestId: string;
  toolName: string;
  input: unknown;
  suggested: PermissionDecision;
}

/** Single AskUserQuestion / ask_user prompt awaiting the user's answer. */
export interface PendingQuestion {
  requestId: string;
  questions: UserInputQuestion[];
}

interface AppState {
  providers: ProviderStatus[];
  sessions: Map<string, SessionSummary>;
  archivedSessions: SessionSummary[];
  projects: ProjectRecord[];
  activeSessionId: string | null;
  /** Sessions whose most recent turn finished while the user was
   *  looking at a different screen / thread. Renders a "Done" badge
   *  in the sidebar so the user can see which threads have new
   *  output to review. Cleared the moment the user activates the
   *  thread, and also cleared whenever a new turn starts on it. */
  doneSessionIds: Set<string>;
  /** Sessions where the agent is actively waiting for the user —
   *  permission prompts, AskUserQuestion calls, ExitPlanMode plan
   *  approvals. Distinct from "running" (which just means a turn is
   *  in flight); this is the subset of running where the model has
   *  paused and won't make progress until the user answers. Cleared
   *  on turn_completed / session_interrupted / session_deleted /
   *  session_archived. */
  awaitingInputSessionIds: Set<string>;
  /** FIFO queue of permission prompts per session. Lives in the
   *  global store (not per-ChatView) so a prompt that arrives while
   *  the user is on a different thread isn't lost — it sits here
   *  until the user opens that thread and answers it. */
  pendingPermissionsBySession: Map<string, PendingPermission[]>;
  /** Single in-flight clarifying question per session. Same rationale
   *  as pendingPermissionsBySession — global so cross-thread events
   *  aren't dropped on the floor. */
  pendingQuestionBySession: Map<string, PendingQuestion>;
  ready: boolean;
}

type AppAction =
  | { type: "server_message"; message: ServerMessage }
  | { type: "set_active_session"; sessionId: string | null }
  /** Pop the head of the per-session permission queue. Used when the
   *  user clicks Allow / Deny — chat-view dispatches this BEFORE
   *  awaiting the answer_permission round-trip so the next queued
   *  prompt becomes visible immediately. */
  | { type: "consume_pending_permission"; sessionId: string; requestId: string }
  /** Clear the per-session pending question. Used when the user
   *  answers OR cancels a question. */
  | { type: "consume_pending_question"; sessionId: string; requestId: string };

/** Recompute whether a session still has any pending input after a
 *  consume action. If both the permissions queue and the question
 *  slot are empty, drop the session from awaitingInputSessionIds so
 *  the sidebar badge clears. */
function recomputeAwaiting(
  awaiting: Set<string>,
  perms: Map<string, PendingPermission[]>,
  questions: Map<string, PendingQuestion>,
  sessionId: string,
): Set<string> {
  const stillPending =
    (perms.get(sessionId)?.length ?? 0) > 0 || questions.has(sessionId);
  if (stillPending) return awaiting;
  if (!awaiting.has(sessionId)) return awaiting;
  const next = new Set(awaiting);
  next.delete(sessionId);
  return next;
}

function appReducer(state: AppState, action: AppAction): AppState {
  switch (action.type) {
    case "server_message":
      return handleServerMessage(state, action.message);
    case "set_active_session": {
      // Opening a thread implicitly clears its "Done" badge — the
      // user is now looking at the output, so we no longer need to
      // shout for their attention.
      let doneSessionIds = state.doneSessionIds;
      if (action.sessionId && doneSessionIds.has(action.sessionId)) {
        doneSessionIds = new Set(doneSessionIds);
        doneSessionIds.delete(action.sessionId);
      }
      return { ...state, activeSessionId: action.sessionId, doneSessionIds };
    }
    case "consume_pending_permission": {
      const list = state.pendingPermissionsBySession.get(action.sessionId);
      if (!list || list.length === 0) return state;
      const filtered = list.filter((p) => p.requestId !== action.requestId);
      if (filtered.length === list.length) return state;
      const pendingPermissionsBySession = new Map(state.pendingPermissionsBySession);
      if (filtered.length === 0) {
        pendingPermissionsBySession.delete(action.sessionId);
      } else {
        pendingPermissionsBySession.set(action.sessionId, filtered);
      }
      const awaitingInputSessionIds = recomputeAwaiting(
        state.awaitingInputSessionIds,
        pendingPermissionsBySession,
        state.pendingQuestionBySession,
        action.sessionId,
      );
      return {
        ...state,
        pendingPermissionsBySession,
        awaitingInputSessionIds,
      };
    }
    case "consume_pending_question": {
      const current = state.pendingQuestionBySession.get(action.sessionId);
      if (!current || current.requestId !== action.requestId) return state;
      const pendingQuestionBySession = new Map(state.pendingQuestionBySession);
      pendingQuestionBySession.delete(action.sessionId);
      const awaitingInputSessionIds = recomputeAwaiting(
        state.awaitingInputSessionIds,
        state.pendingPermissionsBySession,
        pendingQuestionBySession,
        action.sessionId,
      );
      return {
        ...state,
        pendingQuestionBySession,
        awaitingInputSessionIds,
      };
    }
    default:
      return state;
  }
}

function handleServerMessage(
  state: AppState,
  message: ServerMessage,
): AppState {
  switch (message.type) {
    case "welcome": {
      const sessions = new Map<string, SessionSummary>();
      for (const detail of message.bootstrap.snapshot.sessions) {
        sessions.set(detail.summary.sessionId, detail.summary);
      }
      return {
        ...state,
        providers: message.bootstrap.providers,
        sessions,
        projects: message.bootstrap.snapshot.projects,
        ready: true,
      };
    }

    case "snapshot": {
      const sessions = new Map<string, SessionSummary>();
      for (const detail of message.snapshot.sessions) {
        sessions.set(detail.summary.sessionId, detail.summary);
      }
      return {
        ...state,
        sessions,
        projects: message.snapshot.projects,
      };
    }

    case "session_created": {
      const sessions = new Map(state.sessions);
      sessions.set(message.session.sessionId, message.session);
      return { ...state, sessions };
    }

    case "archived_sessions_list": {
      return { ...state, archivedSessions: message.sessions };
    }

    case "event":
      return handleRuntimeEvent(state, message.event);

    default:
      return state;
  }
}

function handleRuntimeEvent(state: AppState, event: RuntimeEvent): AppState {
  switch (event.type) {
    case "session_started": {
      const sessions = new Map(state.sessions);
      sessions.set(event.session.sessionId, event.session);
      return { ...state, sessions };
    }

    case "session_deleted": {
      const sessions = new Map(state.sessions);
      sessions.delete(event.session_id);
      let doneSessionIds = state.doneSessionIds;
      if (doneSessionIds.has(event.session_id)) {
        doneSessionIds = new Set(doneSessionIds);
        doneSessionIds.delete(event.session_id);
      }
      let awaitingInputSessionIds = state.awaitingInputSessionIds;
      if (awaitingInputSessionIds.has(event.session_id)) {
        awaitingInputSessionIds = new Set(awaitingInputSessionIds);
        awaitingInputSessionIds.delete(event.session_id);
      }
      let pendingPermissionsBySession = state.pendingPermissionsBySession;
      if (pendingPermissionsBySession.has(event.session_id)) {
        pendingPermissionsBySession = new Map(pendingPermissionsBySession);
        pendingPermissionsBySession.delete(event.session_id);
      }
      let pendingQuestionBySession = state.pendingQuestionBySession;
      if (pendingQuestionBySession.has(event.session_id)) {
        pendingQuestionBySession = new Map(pendingQuestionBySession);
        pendingQuestionBySession.delete(event.session_id);
      }
      return {
        ...state,
        sessions,
        archivedSessions: state.archivedSessions.filter(
          (s) => s.sessionId !== event.session_id,
        ),
        activeSessionId:
          state.activeSessionId === event.session_id
            ? null
            : state.activeSessionId,
        doneSessionIds,
        awaitingInputSessionIds,
        pendingPermissionsBySession,
        pendingQuestionBySession,
      };
    }

    case "session_interrupted": {
      const sessions = new Map(state.sessions);
      sessions.set(event.session.sessionId, event.session);
      let awaitingInputSessionIds = state.awaitingInputSessionIds;
      if (awaitingInputSessionIds.has(event.session.sessionId)) {
        awaitingInputSessionIds = new Set(awaitingInputSessionIds);
        awaitingInputSessionIds.delete(event.session.sessionId);
      }
      let pendingPermissionsBySession = state.pendingPermissionsBySession;
      if (pendingPermissionsBySession.has(event.session.sessionId)) {
        pendingPermissionsBySession = new Map(pendingPermissionsBySession);
        pendingPermissionsBySession.delete(event.session.sessionId);
      }
      let pendingQuestionBySession = state.pendingQuestionBySession;
      if (pendingQuestionBySession.has(event.session.sessionId)) {
        pendingQuestionBySession = new Map(pendingQuestionBySession);
        pendingQuestionBySession.delete(event.session.sessionId);
      }
      return {
        ...state,
        sessions,
        awaitingInputSessionIds,
        pendingPermissionsBySession,
        pendingQuestionBySession,
      };
    }

    case "turn_started": {
      // The runtime flips session.status to Running server-side in
      // orchestration::start_turn but only broadcasts session_id + turn
      // on TurnStarted (no SessionSummary), so the store would otherwise
      // sit at the previous turn's "ready" status for the entire
      // duration of the new turn. Optimistically mirror the running
      // state here — turn_completed/session_interrupted will overwrite
      // with the authoritative summary when the turn ends.
      const sessions = new Map(state.sessions);
      const s = sessions.get(event.session_id);
      if (s) sessions.set(event.session_id, { ...s, status: "running" });
      // A new turn starts → any stale "Done" badge from the previous
      // turn is no longer meaningful; the thread is busy again.
      let doneSessionIds = state.doneSessionIds;
      if (doneSessionIds.has(event.session_id)) {
        doneSessionIds = new Set(doneSessionIds);
        doneSessionIds.delete(event.session_id);
      }
      return { ...state, sessions, doneSessionIds };
    }

    case "turn_completed": {
      const sessions = new Map(state.sessions);
      sessions.set(event.session.sessionId, event.session);
      // Mark this session as "Done" iff the user isn't currently
      // looking at it. Looking at it means there's nothing to
      // notify about — the user already sees the new output.
      let doneSessionIds = state.doneSessionIds;
      if (event.session.sessionId !== state.activeSessionId) {
        if (!doneSessionIds.has(event.session.sessionId)) {
          doneSessionIds = new Set(doneSessionIds);
          doneSessionIds.add(event.session.sessionId);
        }
      }
      // Turn ended → no input is pending anymore on this session.
      let awaitingInputSessionIds = state.awaitingInputSessionIds;
      if (awaitingInputSessionIds.has(event.session.sessionId)) {
        awaitingInputSessionIds = new Set(awaitingInputSessionIds);
        awaitingInputSessionIds.delete(event.session.sessionId);
      }
      let pendingPermissionsBySession = state.pendingPermissionsBySession;
      if (pendingPermissionsBySession.has(event.session.sessionId)) {
        pendingPermissionsBySession = new Map(pendingPermissionsBySession);
        pendingPermissionsBySession.delete(event.session.sessionId);
      }
      let pendingQuestionBySession = state.pendingQuestionBySession;
      if (pendingQuestionBySession.has(event.session.sessionId)) {
        pendingQuestionBySession = new Map(pendingQuestionBySession);
        pendingQuestionBySession.delete(event.session.sessionId);
      }
      return {
        ...state,
        sessions,
        doneSessionIds,
        awaitingInputSessionIds,
        pendingPermissionsBySession,
        pendingQuestionBySession,
      };
    }

    case "permission_requested": {
      // Capture the prompt globally (keyed by session_id) so it
      // survives the user being on a different thread when it
      // arrives. chat-view reads from pendingPermissionsBySession.
      const existing =
        state.pendingPermissionsBySession.get(event.session_id) ?? [];
      // Dedupe on request_id — daemon-side lag-recovery can replay events.
      if (existing.some((p) => p.requestId === event.request_id)) {
        return state;
      }
      const pendingPermissionsBySession = new Map(state.pendingPermissionsBySession);
      pendingPermissionsBySession.set(event.session_id, [
        ...existing,
        {
          requestId: event.request_id,
          toolName: event.tool_name,
          input: event.input,
          suggested: event.suggested,
        },
      ]);
      const awaitingInputSessionIds = state.awaitingInputSessionIds.has(
        event.session_id,
      )
        ? state.awaitingInputSessionIds
        : (() => {
            const next = new Set(state.awaitingInputSessionIds);
            next.add(event.session_id);
            return next;
          })();
      return {
        ...state,
        pendingPermissionsBySession,
        awaitingInputSessionIds,
      };
    }

    case "user_question_asked": {
      const pendingQuestionBySession = new Map(state.pendingQuestionBySession);
      pendingQuestionBySession.set(event.session_id, {
        requestId: event.request_id,
        questions: event.questions,
      });
      const awaitingInputSessionIds = state.awaitingInputSessionIds.has(
        event.session_id,
      )
        ? state.awaitingInputSessionIds
        : (() => {
            const next = new Set(state.awaitingInputSessionIds);
            next.add(event.session_id);
            return next;
          })();
      return {
        ...state,
        pendingQuestionBySession,
        awaitingInputSessionIds,
      };
    }

    case "plan_proposed": {
      // Plan approval doesn't (yet) round-trip through this store —
      // it's still local UI state in chat-view. Flag the session so
      // the sidebar badge appears, and let chat-view handle the
      // accept/reject flow as before.
      if (state.awaitingInputSessionIds.has(event.session_id)) {
        return state;
      }
      const awaitingInputSessionIds = new Set(state.awaitingInputSessionIds);
      awaitingInputSessionIds.add(event.session_id);
      return { ...state, awaitingInputSessionIds };
    }

    case "project_created": {
      return { ...state, projects: [...state.projects, event.project] };
    }

    case "project_renamed": {
      return {
        ...state,
        projects: state.projects.map((p) =>
          p.projectId === event.project_id
            ? { ...p, name: event.name, updatedAt: event.updated_at }
            : p,
        ),
      };
    }

    case "project_deleted": {
      // Drop the project from the list — sessions retain their
      // (now-dangling) projectId on purpose. The sidebar filters them
      // out by checking projectId against state.projects, and if the
      // user later re-creates a project with the same path the
      // backend un-tombstones the original row (same project_id) and
      // they reappear under it. The reassigned_session_ids field on
      // the wire is always empty now and is kept only for backwards
      // compatibility with old daemon builds.
      const projects = state.projects.filter(
        (p) => p.projectId !== event.project_id,
      );
      return { ...state, projects };
    }

    case "session_project_assigned": {
      const sessions = new Map(state.sessions);
      const s = sessions.get(event.session_id);
      if (s)
        sessions.set(event.session_id, { ...s, projectId: event.project_id });
      return { ...state, sessions };
    }

    case "provider_models_updated": {
      return {
        ...state,
        providers: state.providers.map((p) =>
          p.kind === event.provider ? { ...p, models: event.models } : p,
        ),
      };
    }

    case "provider_health_updated": {
      const exists = state.providers.some((p) => p.kind === event.status.kind);
      return {
        ...state,
        providers: exists
          ? state.providers.map((p) =>
              p.kind === event.status.kind ? event.status : p,
            )
          : [...state.providers, event.status],
      };
    }

    case "session_renamed": {
      const sessions = new Map(state.sessions);
      const s = sessions.get(event.session_id);
      if (s) sessions.set(event.session_id, { ...s, title: event.title });
      return { ...state, sessions };
    }

    case "session_model_updated": {
      const sessions = new Map(state.sessions);
      const s = sessions.get(event.session_id);
      if (s) sessions.set(event.session_id, { ...s, model: event.model });
      return { ...state, sessions };
    }

    case "session_archived": {
      const sessions = new Map(state.sessions);
      const archived = state.sessions.get(event.session_id);
      sessions.delete(event.session_id);
      let doneSessionIds = state.doneSessionIds;
      if (doneSessionIds.has(event.session_id)) {
        doneSessionIds = new Set(doneSessionIds);
        doneSessionIds.delete(event.session_id);
      }
      let awaitingInputSessionIds = state.awaitingInputSessionIds;
      if (awaitingInputSessionIds.has(event.session_id)) {
        awaitingInputSessionIds = new Set(awaitingInputSessionIds);
        awaitingInputSessionIds.delete(event.session_id);
      }
      let pendingPermissionsBySession = state.pendingPermissionsBySession;
      if (pendingPermissionsBySession.has(event.session_id)) {
        pendingPermissionsBySession = new Map(pendingPermissionsBySession);
        pendingPermissionsBySession.delete(event.session_id);
      }
      let pendingQuestionBySession = state.pendingQuestionBySession;
      if (pendingQuestionBySession.has(event.session_id)) {
        pendingQuestionBySession = new Map(pendingQuestionBySession);
        pendingQuestionBySession.delete(event.session_id);
      }
      return {
        ...state,
        sessions,
        archivedSessions: archived
          ? [archived, ...state.archivedSessions]
          : state.archivedSessions,
        activeSessionId:
          state.activeSessionId === event.session_id
            ? null
            : state.activeSessionId,
        doneSessionIds,
        awaitingInputSessionIds,
        pendingPermissionsBySession,
        pendingQuestionBySession,
      };
    }

    case "session_unarchived": {
      const sessions = new Map(state.sessions);
      sessions.set(event.session.sessionId, event.session);
      return {
        ...state,
        sessions,
        archivedSessions: state.archivedSessions.filter(
          (s) => s.sessionId !== event.session.sessionId,
        ),
      };
    }

    default:
      return state;
  }
}

const initialState: AppState = {
  providers: [],
  sessions: new Map(),
  archivedSessions: [],
  projects: [],
  activeSessionId: null,
  doneSessionIds: new Set(),
  awaitingInputSessionIds: new Set(),
  pendingPermissionsBySession: new Map(),
  pendingQuestionBySession: new Map(),
  ready: false,
};

interface AppContextValue {
  state: AppState;
  dispatch: React.Dispatch<AppAction>;
  send: (message: ClientMessage) => Promise<ServerMessage | null>;
}

const AppContext = React.createContext<AppContextValue | null>(null);

export function AppProvider({ children }: { children: React.ReactNode }) {
  const [state, dispatch] = React.useReducer(appReducer, initialState);
  const dispatchRef = React.useRef(dispatch);
  dispatchRef.current = dispatch;

  React.useEffect(() => {
    let active = true;
    connectStream((message) => {
      if (active) {
        dispatchRef.current({ type: "server_message", message });
      }
    });
    return () => {
      active = false;
    };
  }, []);

  // Wrap sendMessage so that any response coming back from a client
  // request is also funneled through the reducer. This is what makes
  // e.g. session_created from start_session land in state.sessions
  // before the navigate fires — without it, only events delivered via
  // connectStream are visible to the store.
  const send = React.useCallback(async (message: ClientMessage) => {
    const res = await sendMessage(message);
    if (res) {
      dispatchRef.current({ type: "server_message", message: res });
    }
    return res;
  }, []);

  const value = React.useMemo(() => ({ state, dispatch, send }), [state, send]);

  return <AppContext.Provider value={value}>{children}</AppContext.Provider>;
}

export function useApp() {
  const ctx = React.useContext(AppContext);
  if (!ctx) throw new Error("useApp must be used within AppProvider");
  return ctx;
}
