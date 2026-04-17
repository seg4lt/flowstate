import * as React from "react";
import {
  connectStream,
  deleteProjectDisplay,
  deleteProjectWorktree,
  deleteSessionDisplay,
  listProjectDisplay,
  listProjectWorktree,
  listSessionDisplay,
  sendMessage,
  setProjectDisplay,
  setProjectWorktree,
  setSessionDisplay,
  type ProjectDisplay,
  type ProjectWorktree,
  type SessionDisplay,
} from "@/lib/api";
import type {
  ClientMessage,
  PermissionDecision,
  ProviderStatus,
  ProjectRecord,
  RateLimitInfo,
  RuntimeEvent,
  ServerMessage,
  SessionSummary,
  UserInputQuestion,
} from "@/lib/types";
import { ALL_PROVIDER_KINDS } from "@/lib/defaults-settings";

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
  /** App-side display metadata: titles, names, previews, ordering.
   *  Hydrated on boot from `user_config.sqlite`. The SDK snapshot
   *  above only has ids + runtime state; anything a user sees as a
   *  label lives here. See
   *  `rs-agent-sdk/crates/core/persistence/CLAUDE.md`. */
  sessionDisplay: Map<string, SessionDisplay>;
  projectDisplay: Map<string, ProjectDisplay>;
  /** Parent/child worktree links, keyed by the worktree's SDK
   *  project_id. A row here marks the project as a git worktree of
   *  its `parentProjectId`. Lives in flowstate's user_config, not the
   *  SDK — each worktree has its own SDK project so cwd resolution
   *  works natively; this table is purely for sidebar grouping and
   *  the tooltip/branch-icon indicator. */
  projectWorktrees: Map<string, ProjectWorktree>;
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
  /** Latest rate-limit / plan-usage snapshot per bucket, keyed by
   *  the provider-defined bucket id. Account-wide, not scoped to
   *  any session — providers report these whenever they update.
   *  Flowstate surfaces them in the Context Display popover. */
  rateLimits: Record<string, RateLimitInfo>;
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
  | { type: "consume_pending_question"; sessionId: string; requestId: string }
  /** Bulk-hydrate the display maps from the app-side store on boot. */
  | {
      type: "hydrate_display";
      sessionDisplay: Map<string, SessionDisplay>;
      projectDisplay: Map<string, ProjectDisplay>;
      projectWorktrees: Map<string, ProjectWorktree>;
    }
  /** Local write — updates the store after a Tauri set_*_display call
   *  succeeds. `null` value means clear the row locally (used alongside
   *  delete_*_display on session/project deletion). */
  | {
      type: "set_session_display";
      sessionId: string;
      display: SessionDisplay | null;
    }
  | {
      type: "set_project_display";
      projectId: string;
      display: ProjectDisplay | null;
    }
  | {
      type: "set_project_worktree";
      projectId: string;
      record: ProjectWorktree | null;
    };

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
    case "hydrate_display": {
      return {
        ...state,
        sessionDisplay: action.sessionDisplay,
        projectDisplay: action.projectDisplay,
        projectWorktrees: action.projectWorktrees,
      };
    }
    case "set_session_display": {
      const sessionDisplay = new Map(state.sessionDisplay);
      if (action.display === null) {
        sessionDisplay.delete(action.sessionId);
      } else {
        sessionDisplay.set(action.sessionId, action.display);
      }
      return { ...state, sessionDisplay };
    }
    case "set_project_display": {
      const projectDisplay = new Map(state.projectDisplay);
      if (action.display === null) {
        projectDisplay.delete(action.projectId);
      } else {
        projectDisplay.set(action.projectId, action.display);
      }
      return { ...state, projectDisplay };
    }
    case "set_project_worktree": {
      const projectWorktrees = new Map(state.projectWorktrees);
      if (action.record === null) {
        projectWorktrees.delete(action.projectId);
      } else {
        projectWorktrees.set(action.projectId, action.record);
      }
      return { ...state, projectWorktrees };
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
      // Dedupe by id — the runtime publishes this event AND includes
      // it (indirectly) in the Ack response, so we may receive it
      // twice in rapid succession.
      if (state.projects.some((p) => p.projectId === event.project.projectId)) {
        return state;
      }
      return { ...state, projects: [...state.projects, event.project] };
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

    case "rate_limit_updated": {
      return {
        ...state,
        rateLimits: {
          ...state.rateLimits,
          [event.info.bucket]: event.info,
        },
      };
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
  projectWorktrees: new Map(),
  sessionDisplay: new Map(),
  projectDisplay: new Map(),
  activeSessionId: null,
  doneSessionIds: new Set(),
  awaitingInputSessionIds: new Set(),
  pendingPermissionsBySession: new Map(),
  pendingQuestionBySession: new Map(),
  rateLimits: {},
  ready: false,
};

interface AppContextValue {
  state: AppState;
  dispatch: React.Dispatch<AppAction>;
  send: (message: ClientMessage) => Promise<ServerMessage | null>;
  /** Rename a session locally — app-side store only, no SDK call. */
  renameSession: (sessionId: string, title: string) => Promise<void>;
  /** Rename a project locally — app-side store only, no SDK call. */
  renameProject: (projectId: string, name: string) => Promise<void>;
  /** Create a project via the SDK (path only) and immediately write
   *  the display name into the app-side store. Resolves once both
   *  the SDK row and the app-side display row exist; returns the
   *  new project_id. */
  createProject: (path: string, name: string) => Promise<string>;
  /** Update a session's preview locally (e.g. on first turn). */
  updateSessionPreview: (sessionId: string, preview: string) => Promise<void>;
  /** Clear display rows when a session/project is deleted by the SDK. */
  deleteSessionDisplayLocal: (sessionId: string) => Promise<void>;
  deleteProjectDisplayLocal: (projectId: string) => Promise<void>;
  /** Mark an SDK project as a git worktree of another SDK project.
   *  Used by the branch-switcher when a user opens or creates a
   *  worktree — the worktree gets its own SDK project (so the agent
   *  runs with cwd = worktree path) and this link tells the sidebar
   *  to group it under the parent project visually. */
  linkProjectWorktree: (
    projectId: string,
    parentProjectId: string,
    branch: string | null,
  ) => Promise<void>;
  /** Remove the parent/child link — used when a worktree is deleted.
   *  The SDK project itself may stay (so archived/old threads still
   *  show) unless also removed separately. */
  unlinkProjectWorktree: (projectId: string) => Promise<void>;
}

const AppContext = React.createContext<AppContextValue | null>(null);

export function AppProvider({ children }: { children: React.ReactNode }) {
  const [state, dispatch] = React.useReducer(appReducer, initialState);
  const dispatchRef = React.useRef(dispatch);
  dispatchRef.current = dispatch;
  // Mirror state into a ref so the callbacks below can read the latest
  // display maps without stale closures or useCallback dependency churn.
  const stateRef = React.useRef(state);
  stateRef.current = state;

  React.useEffect(() => {
    let active = true;
    connectStream((message) => {
      if (!active) return;
      dispatchRef.current({ type: "server_message", message });
      // After the daemon signals readiness, ensure all providers are
      // enabled at the SDK level so health checks run for every one.
      // The app-level toggle (ProviderEnabledProvider) controls what
      // the user sees — the daemon should always track everything.
      if (message.type === "welcome") {
        for (const kind of ALL_PROVIDER_KINDS) {
          sendMessage({
            type: "set_provider_enabled",
            provider: kind,
            enabled: true,
          }).catch(() => {/* best effort */});
        }
      }
      // Side-effect cleanup: when the SDK reports a session or
      // project as permanently deleted, drop its app-side display
      // row too. We don't clean on archive — archived rows may be
      // unarchived later and the display should be preserved.
      if (message.type === "event") {
        if (message.event.type === "session_deleted") {
          void deleteSessionDisplay(message.event.session_id);
          dispatchRef.current({
            type: "set_session_display",
            sessionId: message.event.session_id,
            display: null,
          });
        } else if (message.event.type === "project_deleted") {
          void deleteProjectDisplay(message.event.project_id);
          dispatchRef.current({
            type: "set_project_display",
            projectId: message.event.project_id,
            display: null,
          });
        }
      }
    });

    // Hydrate the display maps in parallel with the stream. These live
    // in `user_config.sqlite` (app-owned), not in the SDK's daemon
    // database. The daemon only knows session/project ids + runtime
    // state; anything a user sees as a label is merged in here.
    Promise.all([
      listSessionDisplay(),
      listProjectDisplay(),
      listProjectWorktree(),
    ])
      .then(([sessionRecord, projectRecord, worktreeRecord]) => {
        if (!active) return;
        dispatchRef.current({
          type: "hydrate_display",
          sessionDisplay: new Map(Object.entries(sessionRecord)),
          projectDisplay: new Map(Object.entries(projectRecord)),
          projectWorktrees: new Map(Object.entries(worktreeRecord)),
        });
      })
      .catch((err) => {
        console.error("failed to hydrate display metadata", err);
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

  const renameSession = React.useCallback(
    async (sessionId: string, title: string) => {
      const trimmed = title.trim();
      const existing = stateRef.current.sessionDisplay.get(sessionId);
      const display: SessionDisplay = {
        title: trimmed.length > 0 ? trimmed : null,
        lastTurnPreview: existing?.lastTurnPreview ?? null,
      };
      await setSessionDisplay(sessionId, display);
      dispatchRef.current({
        type: "set_session_display",
        sessionId,
        display,
      });
    },
    [],
  );

  const renameProject = React.useCallback(
    async (projectId: string, name: string) => {
      const trimmed = name.trim();
      const existing = stateRef.current.projectDisplay.get(projectId);
      const display: ProjectDisplay = {
        name: trimmed.length > 0 ? trimmed : null,
        sortOrder: existing?.sortOrder ?? null,
      };
      await setProjectDisplay(projectId, display);
      dispatchRef.current({
        type: "set_project_display",
        projectId,
        display,
      });
    },
    [],
  );

  const createProject = React.useCallback(
    async (path: string, name: string): Promise<string> => {
      // Snapshot what's currently in the store so we can detect the
      // new project_id when it lands. The SDK's response is an Ack; the
      // authoritative `ProjectCreated` event is delivered via the
      // Tauri channel and processed by the reducer. We wait briefly
      // for the event to show up, then write the display name.
      const beforeIds = new Set(
        stateRef.current.projects.map((p) => p.projectId),
      );
      await sendMessage({ type: "create_project", path });

      let projectId: string | null = null;
      for (let i = 0; i < 40; i++) {
        const match = stateRef.current.projects.find(
          (p) => !beforeIds.has(p.projectId) && p.path === path,
        );
        if (match) {
          projectId = match.projectId;
          break;
        }
        await new Promise((resolve) => setTimeout(resolve, 25));
      }
      if (!projectId) {
        throw new Error("create_project: project_created event never arrived");
      }

      const trimmed = name.trim();
      const display: ProjectDisplay = {
        name: trimmed.length > 0 ? trimmed : null,
        sortOrder: null,
      };
      await setProjectDisplay(projectId, display);
      dispatchRef.current({
        type: "set_project_display",
        projectId,
        display,
      });
      return projectId;
    },
    [],
  );

  const updateSessionPreview = React.useCallback(
    async (sessionId: string, preview: string) => {
      const existing = stateRef.current.sessionDisplay.get(sessionId);
      const display: SessionDisplay = {
        title: existing?.title ?? null,
        lastTurnPreview: preview.slice(0, 140),
      };
      await setSessionDisplay(sessionId, display);
      dispatchRef.current({
        type: "set_session_display",
        sessionId,
        display,
      });
    },
    [],
  );

  const deleteSessionDisplayLocal = React.useCallback(
    async (sessionId: string) => {
      await deleteSessionDisplay(sessionId);
      dispatchRef.current({
        type: "set_session_display",
        sessionId,
        display: null,
      });
    },
    [],
  );

  const deleteProjectDisplayLocal = React.useCallback(
    async (projectId: string) => {
      await deleteProjectDisplay(projectId);
      dispatchRef.current({
        type: "set_project_display",
        projectId,
        display: null,
      });
    },
    [],
  );

  const linkProjectWorktree = React.useCallback(
    async (
      projectId: string,
      parentProjectId: string,
      branch: string | null,
    ) => {
      await setProjectWorktree(projectId, parentProjectId, branch);
      dispatchRef.current({
        type: "set_project_worktree",
        projectId,
        record: { projectId, parentProjectId, branch },
      });
    },
    [],
  );

  const unlinkProjectWorktree = React.useCallback(
    async (projectId: string) => {
      await deleteProjectWorktree(projectId);
      dispatchRef.current({
        type: "set_project_worktree",
        projectId,
        record: null,
      });
    },
    [],
  );

  const value = React.useMemo(
    () => ({
      state,
      dispatch,
      send,
      renameSession,
      renameProject,
      createProject,
      updateSessionPreview,
      deleteSessionDisplayLocal,
      deleteProjectDisplayLocal,
      linkProjectWorktree,
      unlinkProjectWorktree,
    }),
    [
      state,
      send,
      renameSession,
      renameProject,
      createProject,
      updateSessionPreview,
      deleteSessionDisplayLocal,
      deleteProjectDisplayLocal,
      linkProjectWorktree,
      unlinkProjectWorktree,
    ],
  );

  return <AppContext.Provider value={value}>{children}</AppContext.Provider>;
}

export function useApp() {
  const ctx = React.useContext(AppContext);
  if (!ctx) throw new Error("useApp must be used within AppProvider");
  return ctx;
}
