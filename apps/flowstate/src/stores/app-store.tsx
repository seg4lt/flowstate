import * as React from "react";
import {
  connectStream,
  deleteProjectDisplay,
  deleteProjectWorktree,
  deleteSessionDisplay,
  getProjectWorktree,
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
import { useDockBadge } from "@/hooks/use-dock-badge";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import type {
  CheckpointSettings,
  ClientMessage,
  CommandCatalog,
  PermissionDecision,
  PermissionMode,
  ProviderStatus,
  ProjectRecord,
  RateLimitInfo,
  RuntimeEvent,
  ServerMessage,
  SessionLinkReason,
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

/** One failed runtime-provisioning phase, mirrored from the Rust
 *  `flowstate_app_layer::provision::ProvisionFailure` payload. Lives
 *  in app state so the sidebar Settings icon can render a red dot
 *  and the Settings page can render per-phase Retry banners after
 *  the splash dismisses (or on a warm reload that missed the live
 *  events). */
export interface ProvisionFailure {
  /** Wire-format phase id: "node" | "claude-sdk" | "copilot-sdk". */
  phase: string;
  /** Full multi-line anyhow error string. UI typically shows the
   *  first line and exposes the rest behind a disclosure. */
  error: string;
}

interface AppState {
  providers: ProviderStatus[];
  sessions: Map<string, SessionSummary>;
  archivedSessions: SessionSummary[];
  projects: ProjectRecord[];
  /** Checkpoint-enablement snapshot (global default + per-project
   *  overrides). Seeded from `BootstrapPayload.checkpoint_settings`
   *  and refreshed live via `RuntimeEvent::CheckpointEnablementChanged`.
   *  See `useCheckpointSettings` for the consumer-facing hook. */
  checkpointSettings: CheckpointSettings;
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
  /** Latest composer permission mode chosen by the user, per session.
   *  Mirrors the local state chat-view persists to sessionStorage so
   *  the sidebar thread spinner can tint by the *currently* selected
   *  mode rather than the mode the running turn was started with.
   *  Without this the spinner "sticks" on the turn's opening mode and
   *  doesn't follow a plan↔bypass↔accept flip mid-turn. Populated by
   *  chat-view on mount/change; cleared on session_deleted. */
  permissionModeBySession: Map<string, PermissionMode>;
  /** Latest rate-limit / plan-usage snapshot per bucket, keyed by
   *  the provider-defined bucket id. Account-wide, not scoped to
   *  any session — providers report these whenever they update.
   *  Flowstate surfaces them in the Context Display popover. */
  rateLimits: Record<string, RateLimitInfo>;
  /** Per-session command catalog (slash commands + sub-agents + MCP
   *  servers). Populated by `session_command_catalog_updated` events,
   *  which fire on session start, session load, and explicit refresh.
   *  The reducer short-circuits updates whose `commands[].id` array
   *  matches the cached one, so the slash-popup memo stays stable
   *  across no-op refreshes. */
  sessionCommands: Map<string, CommandCatalog>;
  /** Cross-session orchestration links, keyed by the child (spawned
   *  or messaged) session_id. Value is the origin session that
   *  issued the `flowstate_spawn*` / `flowstate_send*` call. Populated
   *  by `session_linked` events; purely local state (not persisted).
   *  Drives the "spawned by agent" chip on the sidebar row. */
  sessionLinks: Map<string, { fromSessionId: string; reason: SessionLinkReason }>;
  /** Whether the OS thinks our window has focus. Distinct from
   *  `activeSessionId` (which tracks the thread the user last
   *  opened, even after they alt-tab to another app). Updated by a
   *  Tauri `onFocusChanged` listener in AppProvider. Drives two
   *  things: (a) turn_completed now marks the active thread as
   *  "Done" when the window isn't focused — otherwise a thread that
   *  finishes while the user is in another app would never badge;
   *  (b) on refocus we clear the active thread from doneSessionIds
   *  because the user is now watching it. */
  isWindowFocused: boolean;
  /** Snapshot of runtime-provisioning failures (Node download, SDK
   *  bridge npm install). Seeded on mount via `get_provision_failures`
   *  to cover the warm-reload case where the live `provision` events
   *  fired before AppProvider mounted; updated live via the same
   *  `provision` Tauri event the splash screen listens to. Empty in
   *  the happy path. */
  provisionFailures: ProvisionFailure[];
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
    }
  /** OS-level window focus changed. Dispatched from AppProvider's
   *  Tauri `onFocusChanged` subscription. Distinct from browser
   *  `focus`/`blur` on the document, which don't track app-level
   *  focus reliably across platforms. */
  | { type: "window_focus_changed"; focused: boolean }
  /** Per-session composer permission mode — dispatched by chat-view
   *  whenever its local `permissionMode` state changes (on mount from
   *  sessionStorage / Settings defaults, and on every user toggle).
   *  Read by the sidebar thread spinner so it tints by the live mode
   *  rather than the turn's starting mode. No-ops if the stored value
   *  already matches so unrelated subscribers don't re-render. */
  | {
      type: "set_session_permission_mode";
      sessionId: string;
      mode: PermissionMode;
    }
  /** Replace the entire provisioning-failures list — used to seed
   *  state from `invoke("get_provision_failures")` on mount. */
  | { type: "set_provision_failures"; failures: ProvisionFailure[] }
  /** Add a new provisioning failure (or replace one for the same
   *  phase). Driven by `provision` events with `kind === "failed"`. */
  | { type: "upsert_provision_failure"; failure: ProvisionFailure }
  /** Remove a provisioning failure for a given phase. Driven by
   *  `provision` events with `kind === "completed"` (so a successful
   *  retry clears the banner without an extra round-trip) and by the
   *  `retry_provision_phase` command on success. */
  | { type: "clear_provision_failure"; phase: string };

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

/** Metadata registered by `createProject` BEFORE sending the SDK
 *  message, keyed by filesystem path. When the corresponding
 *  `project_created` event lands in the reducer, the handler reads
 *  this entry and folds the display name + (optional) worktree link
 *  into the SAME state transition.
 *
 *  Without this coordination the sequence is:
 *    1. Tauri event fires → reducer adds bare project → React renders
 *       an unlabeled, ungrouped entry at the top of the sidebar
 *       ("Untitled project").
 *    2. Caller's polling loop wakes, dispatches display + worktree
 *       link → React re-renders, the entry re-parents under the
 *       parent project.
 *  The flash in step 1 is what the user sees.
 *
 *  Using a module-level Map lets the reducer — which is a pure
 *  function with no access to hooks or refs — find the pending
 *  metadata keyed by the project's path (paths are stable, ids
 *  aren't known until after `project_created` arrives). The entry is
 *  deleted as soon as it's consumed.
 *
 *  Trailing-slash normalization (`normPath`) is applied on both write
 *  and read so git's porcelain output vs. the file picker can't miss
 *  each other. */
const pendingProjectCreates = new Map<
  string,
  {
    display: ProjectDisplay;
    worktreeOf?: { parentProjectId: string; branch: string | null };
  }
>();

function pendingKey(path: string): string {
  return path.endsWith("/") ? path.slice(0, -1) : path;
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
    case "window_focus_changed": {
      // Two jobs here: (1) stamp the new focus flag so the
      // turn_completed handler knows whether the user is watching,
      // and (2) if the window just regained focus and the active
      // thread is currently in doneSessionIds, clear it — the user
      // is now looking at it, so the badge should drop immediately
      // rather than persist until they switch threads.
      const isWindowFocused = action.focused;
      let doneSessionIds = state.doneSessionIds;
      if (
        isWindowFocused &&
        state.activeSessionId &&
        doneSessionIds.has(state.activeSessionId)
      ) {
        doneSessionIds = new Set(doneSessionIds);
        doneSessionIds.delete(state.activeSessionId);
      }
      return { ...state, isWindowFocused, doneSessionIds };
    }
    case "set_session_permission_mode": {
      // Short-circuit when the mode hasn't actually changed so
      // unrelated subscribers (every ThreadItem) don't re-render on
      // a no-op dispatch from chat-view's persist-to-sessionStorage
      // effect.
      const current = state.permissionModeBySession.get(action.sessionId);
      if (current === action.mode) return state;
      const permissionModeBySession = new Map(state.permissionModeBySession);
      permissionModeBySession.set(action.sessionId, action.mode);
      return { ...state, permissionModeBySession };
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
    case "set_provision_failures": {
      return { ...state, provisionFailures: action.failures };
    }
    case "upsert_provision_failure": {
      const others = state.provisionFailures.filter(
        (f) => f.phase !== action.failure.phase,
      );
      return { ...state, provisionFailures: [...others, action.failure] };
    }
    case "clear_provision_failure": {
      const next = state.provisionFailures.filter(
        (f) => f.phase !== action.phase,
      );
      // Avoid producing a new array (and triggering subscribers) if
      // the phase wasn't tracked — `completed` events fire on every
      // successful phase, including the warm-cache happy path.
      if (next.length === state.provisionFailures.length) return state;
      return { ...state, provisionFailures: next };
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
        checkpointSettings: message.bootstrap.checkpointSettings,
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
      let permissionModeBySession = state.permissionModeBySession;
      if (permissionModeBySession.has(event.session_id)) {
        permissionModeBySession = new Map(permissionModeBySession);
        permissionModeBySession.delete(event.session_id);
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
        permissionModeBySession,
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
      // watching it. "Watching" now requires two conditions: the
      // thread is active AND the app window has OS focus. Without
      // the focus check, a thread that finishes while the user is
      // in another app would never mark done (because it's still
      // the active thread), so the dock badge and sidebar badge
      // would both stay blank. See AppProvider's onFocusChanged
      // subscription for the focus tracking, and the
      // window_focus_changed reducer case for the complementary
      // "clear done on refocus of the active thread" logic.
      const viewingActive =
        event.session.sessionId === state.activeSessionId &&
        state.isWindowFocused;
      let doneSessionIds = state.doneSessionIds;
      if (!viewingActive) {
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
      // twice in rapid succession. (Also: StrictMode in dev invokes
      // reducers twice; if we got past this check on the second
      // invocation we'd duplicate the project in the list.)
      if (state.projects.some((p) => p.projectId === event.project.projectId)) {
        return state;
      }
      // If `createProject` pre-registered display/worktree metadata
      // for this path, fold it into the SAME state transition so the
      // sidebar never paints a bare "Untitled project" at the top
      // level while Tauri persistence round-trips.
      //
      // IMPORTANT: read but don't mutate the pending map here —
      // React StrictMode runs this reducer twice in dev, and a
      // mutation would make the second invocation see empty state
      // and commit an un-enriched return. The caller deletes the
      // entry from the polling loop instead.
      const key = pendingKey(event.project.path ?? "");
      const pending = key ? pendingProjectCreates.get(key) : undefined;

      let projectDisplay = state.projectDisplay;
      let projectWorktrees = state.projectWorktrees;
      if (pending) {
        projectDisplay = new Map(projectDisplay);
        projectDisplay.set(event.project.projectId, pending.display);
        if (pending.worktreeOf) {
          projectWorktrees = new Map(projectWorktrees);
          projectWorktrees.set(event.project.projectId, {
            projectId: event.project.projectId,
            parentProjectId: pending.worktreeOf.parentProjectId,
            branch: pending.worktreeOf.branch,
          });
        }
      }

      return {
        ...state,
        projects: [...state.projects, event.project],
        projectDisplay,
        projectWorktrees,
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

    case "session_linked": {
      // Record who spawned or messaged which session. The entry is
      // keyed by the child (the session being acted on) so a sidebar
      // row lookup is O(1). `spawn` events may arrive before the
      // child's `session_started` fan-out; that's fine — the lookup
      // happens at render time.
      const sessionLinks = new Map(state.sessionLinks);
      sessionLinks.set(event.to_session_id, {
        fromSessionId: event.from_session_id,
        reason: event.reason,
      });
      return { ...state, sessionLinks };
    }

    case "session_command_catalog_updated": {
      // id-equality short-circuit: if every command in the new payload
      // matches the cached id (same length, same order), skip the
      // dispatch. ChatView's memoized `mergeCommandsWithCatalog` depends
      // on the Map reference changing only when the popup actually
      // needs to re-render.
      const prev = state.sessionCommands.get(event.session_id);
      const next = event.catalog;
      if (
        prev &&
        prev.commands.length === next.commands.length &&
        prev.agents.length === next.agents.length &&
        prev.mcpServers.length === next.mcpServers.length &&
        prev.commands.every((c, i) => c.id === next.commands[i]?.id) &&
        prev.agents.every((a, i) => a.id === next.agents[i]?.id) &&
        prev.mcpServers.every((m, i) => m.id === next.mcpServers[i]?.id)
      ) {
        return state;
      }
      const sessionCommands = new Map(state.sessionCommands);
      sessionCommands.set(event.session_id, next);
      return { ...state, sessionCommands };
    }

    case "checkpoint_enablement_changed": {
      // Daemon is the source of truth — replace the local snapshot
      // wholesale rather than trying to diff. The settings struct
      // stays tiny (one bool + per-project overrides) so this is
      // cheap even for projects with dozens of overrides.
      return { ...state, checkpointSettings: event.settings };
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
  // Safe conservative default until Welcome lands: checkpoints on.
  // Matches the runtime default in
  // `PersistenceService::CHECKPOINTS_GLOBAL_DEFAULT`.
  checkpointSettings: { globalEnabled: true },
  projectWorktrees: new Map(),
  sessionDisplay: new Map(),
  projectDisplay: new Map(),
  activeSessionId: null,
  doneSessionIds: new Set(),
  awaitingInputSessionIds: new Set(),
  pendingPermissionsBySession: new Map(),
  pendingQuestionBySession: new Map(),
  permissionModeBySession: new Map(),
  rateLimits: {},
  sessionCommands: new Map(),
  sessionLinks: new Map(),
  // Default to focused: the first focus event only fires on the NEXT
  // focus change, so initialising false would incorrectly treat the
  // first turn as "user isn't watching" until they alt-tabbed.
  isWindowFocused: true,
  provisionFailures: [],
  ready: false,
};

/** Shape of the single, store-owned stream subscription. Chat-view
 *  used to open its own `connectStream` channel (doubling every IPC
 *  event); now it subscribes through `addServerMessageListener` on
 *  the store context, so the daemon only sees one subscriber. */
export type ServerMessageListener = (message: ServerMessage) => void;

interface AppContextValue {
  state: AppState;
  dispatch: React.Dispatch<AppAction>;
  send: (message: ClientMessage) => Promise<ServerMessage | null>;
  /** Register a listener invoked for every `ServerMessage` delivered
   *  by the store's single `connectStream` subscription. Returns an
   *  unsubscribe function; call it from the effect's cleanup. */
  addServerMessageListener: (listener: ServerMessageListener) => () => void;
  /** Rename a session locally — app-side store only, no SDK call. */
  renameSession: (sessionId: string, title: string) => Promise<void>;
  /** Rename a project locally — app-side store only, no SDK call. */
  renameProject: (projectId: string, name: string) => Promise<void>;
  /** Persist a new ordering for the active projects list. Accepts
   *  project_ids in their new visual sequence and writes sort_order
   *  = index for each, both to SQLite (via set_project_display) and
   *  to the in-memory store. */
  reorderProjects: (orderedProjectIds: string[]) => Promise<void>;
  /** Create a project via the SDK (path only) and immediately write
   *  the display name into the app-side store. Resolves once both
   *  the SDK row and the app-side display row exist; returns the
   *  new project_id.
   *
   *  `worktreeOf`, when supplied, ties the new project to a parent as
   *  a git worktree link AT THE SAME INSTANT the project_created event
   *  lands in state — so the sidebar never renders it as an unlinked
   *  top-level "Untitled project" while the worktree metadata catches
   *  up asynchronously. Skip it for plain (non-worktree) projects. */
  createProject: (
    path: string,
    name: string,
    worktreeOf?: { parentProjectId: string; branch: string | null },
  ) => Promise<string>;
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

  // Single-source stream subscription. Any consumer that wants
  // per-view reactions to `RuntimeEvent`s registers through
  // `addServerMessageListener` — the store owns the only open
  // `connectStream` channel, dispatches reducer updates, and then
  // notifies listeners. Previously chat-view opened a second
  // channel, so every IPC event was routed through two handlers
  // and the daemon saw twice the fan-out.
  const listenersRef = React.useRef<Set<ServerMessageListener>>(new Set());
  const addServerMessageListener = React.useCallback(
    (listener: ServerMessageListener) => {
      listenersRef.current.add(listener);
      return () => {
        listenersRef.current.delete(listener);
      };
    },
    [],
  );

  // Subscribe to OS-level window focus so the turn_completed handler
  // and the dock badge can tell the difference between "user is on
  // thread A and looking at it" vs "user is on thread A but has
  // alt-tabbed to another app". Tauri's onFocusChanged is
  // authoritative across platforms; browser `focus`/`blur` on the
  // document don't track app-level focus reliably.
  React.useEffect(() => {
    let unlisten: (() => void) | null = null;
    let cancelled = false;
    (async () => {
      try {
        const win = getCurrentWindow();
        // Seed with the current focus state on first mount so we
        // don't start with a stale default if the window is already
        // backgrounded when flowstate loads.
        const focused = await win.isFocused();
        if (cancelled) return;
        dispatchRef.current({ type: "window_focus_changed", focused });
        unlisten = await win.onFocusChanged(({ payload }) => {
          dispatchRef.current({ type: "window_focus_changed", focused: payload });
        });
      } catch (err) {
        console.warn("[app-store] window focus subscription failed:", err);
      }
    })();
    return () => {
      cancelled = true;
      if (unlisten) unlisten();
    };
  }, []);

  // Subscribe to runtime-provisioning events so the sidebar Settings
  // icon can paint a red dot and the Settings page can render Retry
  // banners after the splash dismisses. The splash listens to the
  // same `provision` event for its own purposes (showing the active
  // phase / first-line error); duplicating the subscription is fine —
  // both consumers run in independent React subtrees.
  //
  // Two paths into state:
  //   1. Initial seed — `get_provision_failures` returns whatever the
  //      Tauri shell collected during `provision_runtimes()` BEFORE
  //      AppProvider mounted. Covers warm reload + the case where the
  //      user opens Settings long after boot.
  //   2. Live updates — `provision` events with `kind: "failed"` /
  //      `kind: "completed"` keep the slice in sync as the user
  //      retries phases or successive phases land.
  React.useEffect(() => {
    let unlisten: UnlistenFn | null = null;
    let cancelled = false;
    (async () => {
      try {
        const seed = await invoke<ProvisionFailure[]>("get_provision_failures");
        if (cancelled) return;
        if (seed.length > 0) {
          dispatchRef.current({
            type: "set_provision_failures",
            failures: seed,
          });
        }
      } catch (err) {
        // Non-fatal — older Tauri shells without the command will
        // fall through to the live-event path. We don't surface
        // this anywhere; the absence of a red dot is the right UX
        // when we can't tell whether anything failed.
        console.warn("[app-store] get_provision_failures failed:", err);
      }
      try {
        const u = await listen<
          | { kind: "started"; phase: string; message: string }
          | { kind: "completed"; phase: string; duration_ms: number }
          | { kind: "all-done"; duration_ms: number }
          | { kind: "failed"; phase: string; error: string }
        >("provision", ({ payload }) => {
          if (payload.kind === "failed") {
            dispatchRef.current({
              type: "upsert_provision_failure",
              failure: { phase: payload.phase, error: payload.error },
            });
          } else if (payload.kind === "completed") {
            dispatchRef.current({
              type: "clear_provision_failure",
              phase: payload.phase,
            });
          }
        });
        if (cancelled) {
          u();
        } else {
          unlisten = u;
        }
      } catch (err) {
        console.warn("[app-store] provision event subscription failed:", err);
      }
    })();
    return () => {
      cancelled = true;
      if (unlisten) unlisten();
    };
  }, []);

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
          }).catch((err) => {
            // Best effort — the SDK may already have this provider
            // enabled, or the daemon may be racing shutdown. Log at
            // debug level so the failure isn't fully silent.
            console.debug("[app-store] set_provider_enabled burst failed", err);
          });
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
        } else if (message.event.type === "project_created") {
          // Backend-initiated creates (e.g. an agent used a worktree
          // tool — see WorktreeProvisionerImpl in src-tauri) don't go
          // through the frontend's `createProject` wrapper, so the
          // `pendingProjectCreates` map has no entry to fold into the
          // reducer. Without hydrating here, the sidebar paints the
          // new project as an un-grouped "Untitled project" at the
          // top level until the next app restart rereads
          // `listProjectWorktree`.
          //
          // The Rust provisioner persists the `project_worktree` link
          // BEFORE firing the event, so by the time we land here the
          // row is guaranteed to be available on disk — we just need
          // to pull it into in-memory state.
          const ev = message.event;
          const project = ev.project;
          const key = pendingKey(project.path ?? "");
          if (!pendingProjectCreates.has(key)) {
            void getProjectWorktree(project.projectId)
              .then((record) => {
                if (!active || !record) return;
                dispatchRef.current({
                  type: "set_project_worktree",
                  projectId: project.projectId,
                  record,
                });
              })
              .catch((err) => {
                console.debug(
                  "[app-store] getProjectWorktree failed for",
                  project.projectId,
                  err,
                );
              });
          }
        }
      }
      // Fan out to registered listeners AFTER the reducer and the
      // display-cleanup side effects have run, so any per-view code
      // that reads the store sees fully-updated state. Iterating a
      // copy insulates against concurrent listener unsubscribes.
      const snapshot = Array.from(listenersRef.current);
      for (const listener of snapshot) {
        try {
          listener(message);
        } catch (err) {
          console.error("[app-store] stream listener threw", err);
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

  const reorderProjects = React.useCallback(
    async (orderedProjectIds: string[]) => {
      // Rewrite sort_order = 0..N-1 in the new visual sequence. N is
      // typically <30, so the O(N) Tauri round-trips are cheap, and
      // a dense 0..N-1 range keeps future reorders simple (no
      // fractional-rank rebalancing). Preserve each project's
      // existing name — we only mutate sortOrder here.
      await Promise.all(
        orderedProjectIds.map(async (projectId, index) => {
          const existing = stateRef.current.projectDisplay.get(projectId);
          if (existing?.sortOrder === index) return;
          const display: ProjectDisplay = {
            name: existing?.name ?? null,
            sortOrder: index,
          };
          await setProjectDisplay(projectId, display);
          dispatchRef.current({
            type: "set_project_display",
            projectId,
            display,
          });
        }),
      );
    },
    [],
  );

  const createProject = React.useCallback(
    async (
      path: string,
      name: string,
      worktreeOf?: { parentProjectId: string; branch: string | null },
    ): Promise<string> => {
      const trimmed = name.trim();
      const display: ProjectDisplay = {
        name: trimmed.length > 0 ? trimmed : null,
        sortOrder: null,
      };

      // Register display + worktree metadata BEFORE sending the SDK
      // message. The reducer's `project_created` handler reads this
      // map (keyed by path) when the event lands and folds everything
      // into the SAME state transition — so React renders the new
      // project with its final name and parent link in a single paint,
      // with no "Untitled project" flash at the top of the sidebar.
      const key = pendingKey(path);
      pendingProjectCreates.set(key, { display, worktreeOf });

      // Snapshot existing ids so we can identify the new project when
      // it lands in state (polling below).
      const beforeIds = new Set(
        stateRef.current.projects.map((p) => p.projectId),
      );

      try {
        await sendMessage({ type: "create_project", path });
      } catch (err) {
        pendingProjectCreates.delete(key);
        throw err;
      }

      // Poll for the project_created event to land in state.
      let projectId: string | null = null;
      for (let i = 0; i < 40; i++) {
        const match = stateRef.current.projects.find(
          (p) =>
            !beforeIds.has(p.projectId) && pendingKey(p.path ?? "") === key,
        );
        if (match) {
          projectId = match.projectId;
          break;
        }
        await new Promise((resolve) => setTimeout(resolve, 25));
      }
      // Pending entry has done its job (or never will). Delete here,
      // OUTSIDE the reducer, so StrictMode's double-invocation of the
      // reducer doesn't lose the metadata between the two calls.
      pendingProjectCreates.delete(key);
      if (!projectId) {
        throw new Error("create_project: project_created event never arrived");
      }

      // Fallback dispatch: if the reducer somehow missed the pending
      // entry (e.g. the path in the event differs from what we sent,
      // or the entry was already consumed by a duplicate event), make
      // sure the in-memory state still has the right display + link
      // before we persist. These dispatches are no-ops when the
      // reducer already applied them.
      const applied = stateRef.current;
      if (!applied.projectDisplay.has(projectId)) {
        dispatchRef.current({
          type: "set_project_display",
          projectId,
          display,
        });
      }
      if (worktreeOf && !applied.projectWorktrees.has(projectId)) {
        dispatchRef.current({
          type: "set_project_worktree",
          projectId,
          record: {
            projectId,
            parentProjectId: worktreeOf.parentProjectId,
            branch: worktreeOf.branch,
          },
        });
      }

      // Persist to SQLite — on failure roll back any worktree link
      // dispatch so the UI doesn't claim state that never landed on
      // disk. Display name is idempotent enough that a failed write is
      // fine to leave in memory.
      try {
        await setProjectDisplay(projectId, display);
        if (worktreeOf) {
          await setProjectWorktree(
            projectId,
            worktreeOf.parentProjectId,
            worktreeOf.branch,
          );
        }
      } catch (err) {
        if (worktreeOf) {
          dispatchRef.current({
            type: "set_project_worktree",
            projectId,
            record: null,
          });
        }
        throw err;
      }
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
      // Optimistic dispatch — happens in the same tick as the caller's
      // post-createProject code, so React batches it with the
      // project_created render. Without this the sidebar briefly shows
      // the freshly-created worktree project as a separate top-level
      // entry (with name "Untitled project" until display lands) until
      // the Tauri `set_project_worktree` write round-trips and the
      // grouping kicks in. On persistence failure we roll back so the
      // UI doesn't silently claim a link that was never saved.
      const record = { projectId, parentProjectId, branch };
      dispatchRef.current({
        type: "set_project_worktree",
        projectId,
        record,
      });
      try {
        await setProjectWorktree(projectId, parentProjectId, branch);
      } catch (err) {
        dispatchRef.current({
          type: "set_project_worktree",
          projectId,
          record: null,
        });
        throw err;
      }
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
      addServerMessageListener,
      renameSession,
      renameProject,
      reorderProjects,
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
      addServerMessageListener,
      renameSession,
      renameProject,
      reorderProjects,
      createProject,
      updateSessionPreview,
      deleteSessionDisplayLocal,
      deleteProjectDisplayLocal,
      linkProjectWorktree,
      unlinkProjectWorktree,
    ],
  );

  return (
    <AppContext.Provider value={value}>
      {/* Sync the OS dock/taskbar badge with the number of threads
          awaiting user input or freshly finished. Rendered as a
          sibling of `{children}` inside the provider so the hook can
          read our context without any prop plumbing; returns null and
          produces zero DOM. */}
      <DockBadgeSync />
      {children}
    </AppContext.Provider>
  );
}

function DockBadgeSync(): null {
  useDockBadge();
  return null;
}

export function useApp() {
  const ctx = React.useContext(AppContext);
  if (!ctx) throw new Error("useApp must be used within AppProvider");
  return ctx;
}

/** Snapshot of runtime-provisioning failures (Node.js download,
 *  provider-SDK npm installs). Empty in the happy path. Consumers
 *  use it for the sidebar Settings red dot and the Settings-page
 *  Retry banners. */
export function useProvisionFailures(): ProvisionFailure[] {
  const { state } = useApp();
  return state.provisionFailures;
}

/** Subscribe to the per-session command catalog (slash commands,
 *  sub-agents, MCP servers). Returns `undefined` until the first
 *  `session_command_catalog_updated` event lands for this session;
 *  consumers should treat that as "show core commands only". */
export function useSessionCommandCatalog(
  sessionId: string | undefined,
): CommandCatalog | undefined {
  const { state } = useApp();
  if (!sessionId) return undefined;
  return state.sessionCommands.get(sessionId);
}

// Narrow slice hooks. Each projects just the fields in its slice so
// consumers only re-render on the data they actually read. We still
// wrap `useApp()` under the hood (single reducer) but the component
// surface reads as if the store were sliced by domain. When the
// reducer is eventually fractured into four separate reducers (§5.2
// full), these hooks stay — swap the implementation, leave the call
// sites alone.

/** Session-domain slice: active/archived sessions, `doneSessionIds`,
 *  `awaitingInputSessionIds`, active id, window focus. */
export function useSessionSlice() {
  const { state } = useApp();
  return {
    sessions: state.sessions,
    archivedSessions: state.archivedSessions,
    activeSessionId: state.activeSessionId,
    doneSessionIds: state.doneSessionIds,
    awaitingInputSessionIds: state.awaitingInputSessionIds,
    isWindowFocused: state.isWindowFocused,
    ready: state.ready,
  };
}

/** Pending-prompt slice: permission queues, in-flight questions,
 *  per-session composer permission mode. */
export function usePendingSlice() {
  const { state } = useApp();
  return {
    pendingPermissionsBySession: state.pendingPermissionsBySession,
    pendingQuestionBySession: state.pendingQuestionBySession,
    permissionModeBySession: state.permissionModeBySession,
  };
}

/** Provider-domain slice: adapter statuses, rate limits, per-session
 *  command catalogs. */
export function useProviderSlice() {
  const { state } = useApp();
  return {
    providers: state.providers,
    rateLimits: state.rateLimits,
    sessionCommands: state.sessionCommands,
  };
}

/** Project-domain slice: SDK project list, app-side display
 *  metadata, worktree links. */
export function useProjectSlice() {
  const { state } = useApp();
  return {
    projects: state.projects,
    projectDisplay: state.projectDisplay,
    projectWorktrees: state.projectWorktrees,
    sessionDisplay: state.sessionDisplay,
  };
}
