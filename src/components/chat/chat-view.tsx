import * as React from "react";
import { useNavigate } from "@tanstack/react-router";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { SidebarTrigger } from "@/components/ui/sidebar";
import { cn } from "@/lib/utils";
import { useApp } from "@/stores/app-store";
import type {
  AttachedImage,
  AttachmentRef,
  ContentBlock,
  PermissionDecision,
  PermissionMode,
  ReasoningEffort,
  RuntimeEvent,
  TurnRecord,
  UserInputAnswer,
  UserInputQuestion,
} from "@/lib/types";
import { connectStream, sendMessage } from "@/lib/api";
import {
  gitBranchQueryOptions,
  loadFullSession,
  pathExistsQueryOptions,
  sessionQueryKey,
  sessionQueryOptions,
  type SessionPage,
} from "@/lib/queries";
import { useStreamedGitDiffSummary } from "@/lib/git-diff-stream";
import { cycleMode, MODE_LABELS } from "@/lib/mode-cycling";
import { resolveCommand, COMMAND_META, type SlashCommandContext } from "@/lib/slash-commands";
import { toast } from "@/hooks/use-toast";
import { MessageList } from "./messages/message-list";
import { ChatInput } from "./chat-input";
import { PermissionPrompt } from "./permission-prompt";
import { QuestionPrompt } from "./question-prompt";
import { ChatToolbar } from "./chat-toolbar";
import { HeaderActions } from "./header-actions";
import { BranchSwitcher } from "./branch-switcher";
import { WorkingIndicator } from "./working-indicator";
import { StuckBanner } from "./stuck-banner";
import { DiffPanel, type DiffStyle } from "./diff-panel";
import { ImageLightbox } from "./image-lightbox";
import type { AggregatedFileDiff } from "@/lib/session-diff";

// Trip the watchdog after this many seconds of silence while a tool
// call is pending. Picked to be well past a normal tool round-trip
// (even a slow Bash / Git command rarely exceeds 15–20s) but short
// enough that a user who just clicked Allow doesn't sit for a minute
// wondering if anything is happening.
const STUCK_TIMEOUT_MS = 45_000;

// Diff-panel sizing. Clamped so neither the chat column nor the diff
// pane can collapse to nothing when the user drags the handle.
const DIFF_WIDTH_KEY = "flowzen:diff-width";
const DIFF_STYLE_KEY = "flowzen:diff-style";
const DIFF_MIN_WIDTH = 360;
const DIFF_DEFAULT_WIDTH = 560;
const DIFF_CHAT_MIN_WIDTH = 420;

interface PermissionRequest {
  requestId: string;
  toolName: string;
  input: unknown;
  suggested: string;
}

interface QuestionRequest {
  requestId: string;
  questions: UserInputQuestion[];
}

// Stream-order block accumulators. Adjacent text deltas coalesce into
// the trailing text block; a non-text block (e.g. a tool call) closes
// the run so the next text delta opens a new block. Always returns a
// new array so React.memo / reference equality picks up the change.
function appendTextDelta(
  blocks: ContentBlock[] | undefined,
  delta: string,
): ContentBlock[] {
  const list = blocks ?? [];
  const last = list[list.length - 1];
  if (last && last.kind === "text") {
    return [...list.slice(0, -1), { kind: "text", text: last.text + delta }];
  }
  return [...list, { kind: "text", text: delta }];
}

function appendReasoningDelta(
  blocks: ContentBlock[] | undefined,
  delta: string,
): ContentBlock[] {
  const list = blocks ?? [];
  const last = list[list.length - 1];
  if (last && last.kind === "reasoning") {
    return [
      ...list.slice(0, -1),
      { kind: "reasoning", text: last.text + delta },
    ];
  }
  return [...list, { kind: "reasoning", text: delta }];
}

// Apply a single runtime event to a turns array and return the
// next-state turns. Used by the stream handler to update the
// query cache entry for the event's session — because this is a
// pure function over `prev`, we can run it inside a
// `queryClient.setQueryData` updater and route every event
// directly to the right session's cache entry without any
// cross-session state leakage. Returns the same array reference
// when the event doesn't apply to any known turn, so the
// updater can bail out and avoid a wasted re-render.
function applyEventToTurns(
  prev: TurnRecord[],
  event: RuntimeEvent,
): TurnRecord[] {
  switch (event.type) {
    case "turn_started":
    case "turn_completed": {
      const exists = prev.some((t) => t.turnId === event.turn.turnId);
      if (exists) {
        return prev.map((t) =>
          t.turnId === event.turn.turnId ? event.turn : t,
        );
      }
      return [...prev, event.turn];
    }
    case "content_delta":
      return prev.map((t) =>
        t.turnId === event.turn_id
          ? {
              ...t,
              output: event.accumulated_output,
              blocks: appendTextDelta(t.blocks, event.delta),
            }
          : t,
      );
    case "reasoning_delta":
      return prev.map((t) =>
        t.turnId === event.turn_id
          ? {
              ...t,
              reasoning: (t.reasoning ?? "") + event.delta,
              blocks: appendReasoningDelta(t.blocks, event.delta),
            }
          : t,
      );
    case "tool_call_started":
      return prev.map((t) =>
        t.turnId === event.turn_id
          ? {
              ...t,
              toolCalls: [
                ...(t.toolCalls ?? []),
                {
                  callId: event.call_id,
                  name: event.name,
                  args: event.args,
                  status: "pending" as const,
                  parentCallId: event.parent_call_id,
                },
              ],
              blocks: [
                ...(t.blocks ?? []),
                { kind: "tool_call", callId: event.call_id },
              ],
            }
          : t,
      );
    case "tool_call_completed":
      return prev.map((t) => {
        if (t.turnId !== event.turn_id || !t.toolCalls) return t;
        return {
          ...t,
          toolCalls: t.toolCalls.map((tc) =>
            tc.callId === event.call_id
              ? {
                  ...tc,
                  output: event.output,
                  error: event.error,
                  status: event.error
                    ? ("failed" as const)
                    : ("completed" as const),
                }
              : tc,
          ),
        };
      });
    default:
      return prev;
  }
}

// Vertical drag handle between the chat column and the diff pane.
// Mirrors the sidebar DragHandle pattern in router.tsx but measures
// against the split container's right edge so the panel grows from
// the right as the mouse moves left. The handle lives inline between
// the two flex children (not absolutely positioned) to avoid z-index
// fights with the sidebar handle and other overlays.
function DiffDragHandle({
  containerRef,
  width,
  onResize,
}: {
  containerRef: React.RefObject<HTMLDivElement | null>;
  width: number;
  onResize: (w: number) => void;
}) {
  const draggingRef = React.useRef(false);
  const latestWidthRef = React.useRef(width);

  React.useEffect(() => {
    latestWidthRef.current = width;
  }, [width]);

  React.useEffect(() => {
    function onMove(e: MouseEvent) {
      if (!draggingRef.current || !containerRef.current) return;
      const rect = containerRef.current.getBoundingClientRect();
      const maxWidth = Math.max(
        DIFF_MIN_WIDTH,
        Math.floor(rect.width - DIFF_CHAT_MIN_WIDTH),
      );
      const next = Math.max(
        DIFF_MIN_WIDTH,
        Math.min(maxWidth, Math.round(rect.right - e.clientX)),
      );
      latestWidthRef.current = next;
      onResize(next);
    }
    function onUp() {
      if (!draggingRef.current) return;
      draggingRef.current = false;
      document.body.style.cursor = "";
      document.body.style.userSelect = "";
      try {
        window.localStorage.setItem(
          DIFF_WIDTH_KEY,
          String(latestWidthRef.current),
        );
      } catch {
        /* storage may be unavailable; width is still live in state */
      }
    }
    window.addEventListener("mousemove", onMove);
    window.addEventListener("mouseup", onUp);
    return () => {
      window.removeEventListener("mousemove", onMove);
      window.removeEventListener("mouseup", onUp);
    };
  }, [containerRef, onResize]);

  return (
    <div
      role="separator"
      aria-label="Resize diff panel"
      aria-orientation="vertical"
      className="w-1 shrink-0 cursor-col-resize bg-border/50 hover:bg-border"
      onMouseDown={(e) => {
        e.preventDefault();
        draggingRef.current = true;
        document.body.style.cursor = "col-resize";
        document.body.style.userSelect = "none";
      }}
    />
  );
}

// ChatView is stable across thread switches — we deliberately do
// *not* key it on `sessionId`. Instead, `turns` is derived directly
// from the tanstack query cache entry for the active session, and
// streaming events write straight into that cache via setQueryData
// (keyed by `event.session_id`). This gives us two properties:
//
//  1. Cross-session isolation is free. Every session has its own
//     cache entry; an event for thread A can never touch thread B.
//     That replaces the ref-juggling and defensive setTurns guards
//     an earlier iteration needed, and eliminates the whole class
//     of "two threads at once" races.
//
//  2. Re-visits are instant with no full re-render. Going A → B → A
//     returns the *same* cached turns array reference, so React's
//     reconciliation of MessageList is near-free — TurnView/
//     MarkdownContent/CodeBlock all keep their rendered output
//     under Virtuoso's key-based item reconciliation. The previous
//     keyed-remount design correctly isolated state but paid the
//     full render cost on every click, which is why threads felt
//     slow to load even on warm cache.
export function ChatView({ sessionId }: { sessionId: string }) {
  const { state, dispatch, send, renameSession } = useApp();
  const navigate = useNavigate();
  const queryClient = useQueryClient();
  const sessionQuery = useQuery(sessionQueryOptions(sessionId));
  const turns: TurnRecord[] = sessionQuery.data?.detail.turns ?? [];
  const loading = sessionQuery.isLoading && !sessionQuery.data;

  // Ref tracking the currently-visible session. Used by the stream
  // handler to decide whether an incoming event should mutate
  // *current-view* UI state (pending input, permission queue, etc.)
  // in addition to the session-specific cache update. Events for
  // inactive sessions still update their own cache entry but don't
  // touch the current view's transient state.
  const sessionIdRef = React.useRef(sessionId);
  sessionIdRef.current = sessionId;

  // "Load older" state. The paginated initial load returns at most
  // SESSION_PAGE_SIZE turns from the tail; older turns are fetched
  // in a single additional round-trip that replaces the cache entry
  // with the full history. `loadingOlder` flips while the request
  // is in flight so the banner can show a spinner.
  const [loadingOlder, setLoadingOlder] = React.useState(false);
  const hiddenOlderCount = Math.max(
    0,
    (sessionQuery.data?.totalTurns ?? 0) - turns.length,
  );
  const handleLoadOlder = React.useCallback(async () => {
    if (loadingOlder) return;
    setLoadingOlder(true);
    try {
      // loadFullSession writes the complete turn history into the
      // cache entry itself; useQuery notifies and the view re-renders
      // against the fatter `sessionQuery.data` automatically.
      await loadFullSession(queryClient, sessionId);
    } finally {
      setLoadingOlder(false);
    }
  }, [loadingOlder, queryClient, sessionId]);
  // FIFO queue of outstanding permission requests + the in-flight
  // clarifying question, BOTH read from the global store. Lifting
  // them out of chat-view fixes the cross-thread drop bug: events
  // that arrive while the user is on a different session used to be
  // silently discarded by the eventSessionId !== sessionIdRef early
  // return below, leaving the affected thread permanently stuck on
  // a never-rendered prompt. The store now captures every event
  // keyed by session_id, and chat-view becomes a pure consumer.
  const pendingPermissions = React.useMemo<PermissionRequest[]>(
    () => state.pendingPermissionsBySession.get(sessionId) ?? [],
    [state.pendingPermissionsBySession, sessionId],
  );
  const pendingQuestion: QuestionRequest | null = React.useMemo(() => {
    const q = state.pendingQuestionBySession.get(sessionId);
    return q ?? null;
  }, [state.pendingQuestionBySession, sessionId]);
  const [effort, setEffort] = React.useState<ReasoningEffort>("high");
  const permissionStorageKey = `flowzen:permissionMode:${sessionId}`;
  const [permissionMode, setPermissionMode] =
    React.useState<PermissionMode>(
      () =>
        (sessionStorage.getItem(permissionStorageKey) as PermissionMode) ??
        "accept_edits",
    );

  // Persist permission mode to sessionStorage so it survives navigation
  // (e.g. Settings → back) without losing the user's choice.
  React.useEffect(() => {
    sessionStorage.setItem(permissionStorageKey, permissionMode);
  }, [permissionStorageKey, permissionMode]);

  const [pendingInput, setPendingInput] = React.useState<string | null>(null);
  // Watchdog state: `lastEventAt` bumps on every stream event for this
  // session so the 45s inactivity timer resets. `stuckSince` is set
  // when the timer fires and a pending tool call exists; rendering the
  // StuckBanner keys off it.
  const [lastEventAt, setLastEventAt] = React.useState<number>(() =>
    Date.now(),
  );
  const [stuckSince, setStuckSince] = React.useState<number | null>(null);

  // The diff view is sourced directly from `git diff HEAD` against
  // the project's working tree (plus untracked files). It refreshes
  // on session load, on every `turn_completed` event, and whenever
  // the user opens the panel — so each turn shows its cumulative
  // effect without us instrumenting individual tool calls.
  // `diffRefreshTick` bumps to restart the streamed subscription
  // without blowing away the previously-committed file list, so the
  // Diff button badge stays steady across refreshes.
  const [diffRefreshTick, setDiffRefreshTick] = React.useState(0);
  // Latches true the first time the user opens or hovers the diff
  // panel button for this chat view. Gates the streamed
  // subscription itself (`enabled`) AND the stream-event refresh
  // path — before the first interaction we don't run a single git
  // subprocess for this view. The state flavor drives the hook's
  // `enabled` prop; the ref flavor is read synchronously from
  // stream-event handlers whose effect we don't want to re-run on
  // every flip.
  const [diffSubscriptionActive, setDiffSubscriptionActive] =
    React.useState(false);
  const diffPanelEverOpenedRef = React.useRef(false);
  // 400ms grace window to collapse back-to-back `refreshDiffs()`
  // triggers into a single tick bump. `session_loaded` and the first
  // `turn_completed` can fire within a few hundred ms of each other
  // on initial load, and rapid multi-turn runs can also storm this
  // path; either way we don't need more than ~2.5 refetches/sec.
  // Branch checkout passes `{ force: true }` to bypass — it's a
  // one-shot gesture that must always reach the subscription even
  // if a streaming refresh just landed inside the debounce window.
  const lastRefreshAtRef = React.useRef(0);
  const refreshDiffs = React.useCallback((opts?: { force?: boolean }) => {
    const now = Date.now();
    if (!opts?.force && now - lastRefreshAtRef.current < 400) return;
    lastRefreshAtRef.current = now;
    setDiffRefreshTick((t) => t + 1);
  }, []);

  // Arm the diff subscription. Called on Diff-button hover/focus
  // AND on first panel open. Unlike the old hover prefetch this
  // does NOT bump the refresh tick on every call — flipping
  // `diffSubscriptionActive` from false → true starts the
  // subscription exactly once, and subsequent calls are React
  // no-ops (setState bails when the value didn't change). The
  // streamed hook keeps previous diffs visible across any future
  // refresh-tick bumps, so the Diff button badge never flickers
  // empty on hover the way it did with the tanstack-query version.
  const activateDiffSubscription = React.useCallback(() => {
    diffPanelEverOpenedRef.current = true;
    setDiffSubscriptionActive(true);
  }, []);

  // Diff panel state. Closed by default — open it from the chat
  // header's "Show diff" button when you want to see what the
  // session changed. `diffWidth` and `diffStyle` are user
  // preferences persisted to localStorage so they survive
  // restarts.
  const splitContainerRef = React.useRef<HTMLDivElement | null>(null);
  const [diffOpen, setDiffOpen] = React.useState(false);
  const [diffFullscreen, setDiffFullscreen] = React.useState(false);
  /** When set, a lightbox is open on top of everything for a persisted
   * attachment. The bytes are fetched lazily via attachmentQueryOptions
   * the first time it opens. */
  const [persistedLightboxRef, setPersistedLightboxRef] =
    React.useState<AttachmentRef | null>(null);
  const handleOpenPersistedAttachment = React.useCallback(
    (attachment: AttachmentRef) => setPersistedLightboxRef(attachment),
    [],
  );
  const [diffWidth, setDiffWidth] = React.useState<number>(() => {
    try {
      const saved = window.localStorage.getItem(DIFF_WIDTH_KEY);
      if (saved) {
        const parsed = Number.parseInt(saved, 10);
        if (Number.isFinite(parsed) && parsed >= DIFF_MIN_WIDTH) {
          return parsed;
        }
      }
    } catch {
      /* storage may be unavailable */
    }
    return DIFF_DEFAULT_WIDTH;
  });
  const [diffStyle, setDiffStyleState] = React.useState<DiffStyle>(() => {
    try {
      const saved = window.localStorage.getItem(DIFF_STYLE_KEY);
      if (saved === "split" || saved === "unified") return saved;
    } catch {
      /* storage may be unavailable */
    }
    return "split";
  });
  const setDiffStyle = React.useCallback((s: DiffStyle) => {
    setDiffStyleState(s);
    try {
      window.localStorage.setItem(DIFF_STYLE_KEY, s);
    } catch {
      /* storage may be unavailable */
    }
  }, []);

  // Look up the active session first, then fall back to the archived
  // list so the chat view can render read-only history for an archived
  // thread without the caller having to know which table it lives in.
  const session =
    state.sessions.get(sessionId) ??
    state.archivedSessions.find((s) => s.sessionId === sessionId);
  const isArchived = React.useMemo(
    () => state.archivedSessions.some((s) => s.sessionId === sessionId),
    [state.archivedSessions, sessionId],
  );
  // Runtime enablement lookup for the session's provider. When the
  // user disables a provider from Settings, existing sessions stay
  // visible (history preserved) but the chat header shows a badge
  // and the composer's send button is forced off. A disabled provider
  // is also `undefined` here if its health check hasn't arrived yet,
  // which matches the "start optimistic, reconcile on welcome" flow
  // the rest of the view already uses.
  const providerDisabled = React.useMemo(() => {
    if (!session) return false;
    const provider = state.providers.find((p) => p.kind === session.provider);
    return provider?.enabled === false;
  }, [session, state.providers]);
  const projectPath = React.useMemo(() => {
    if (!session?.projectId) return null;
    return state.projects.find((p) => p.projectId === session.projectId)?.path ?? null;
  }, [session?.projectId, state.projects]);
  // Worktree threads live under their own SDK project whose path is
  // the worktree folder. The "parent" is the main repo's SDK project
  // — what the user perceives as the one project in the sidebar. If
  // this session isn't a worktree, parent == current.
  const parentProjectId = React.useMemo(() => {
    if (!session?.projectId) return null;
    return (
      state.projectWorktrees.get(session.projectId)?.parentProjectId ??
      session.projectId
    );
  }, [session?.projectId, state.projectWorktrees]);
  const parentProjectPath = React.useMemo(() => {
    if (!parentProjectId) return null;
    return (
      state.projects.find((p) => p.projectId === parentProjectId)?.path ?? null
    );
  }, [parentProjectId, state.projects]);
  // Branch + diff summary are fetched via project-scoped queries,
  // so switching between threads in the same project reuses the
  // cached values rather than re-shelling out to git on every
  // navigation. Both queries sit behind `enabled: !!path`, so the
  // cache read is a no-op for folder-less (null-project) sessions
  // where there's nothing to diff against.
  const gitBranchQuery = useQuery(gitBranchQueryOptions(projectPath));
  const gitBranch = gitBranchQuery.data ?? null;
  // Streamed replacement for the old useQuery(gitDiffSummaryQueryOptions)
  // call. The hook handles phase-1/phase-2 streaming, cancellation on
  // unmount, and keep-previous-data across refresh-tick bumps so the
  // Diff button badge doesn't flash empty when we restart the
  // subscription (turn_completed, session_loaded, branch checkout).
  const diffStream = useStreamedGitDiffSummary(
    projectPath,
    diffRefreshTick,
    diffSubscriptionActive,
  );
  // Worktree threads live under their own SDK project whose path IS
  // the worktree folder. If that folder has been removed on disk —
  // either from the branch-switcher's delete button or out-of-band
  // in a terminal — the agent can't run there anymore, so we flip
  // the thread into the same read-only mode archived threads use.
  const isWorktreeThread = React.useMemo(() => {
    if (!session?.projectId) return false;
    return state.projectWorktrees.has(session.projectId);
  }, [session?.projectId, state.projectWorktrees]);
  const worktreeFolderQuery = useQuery({
    ...pathExistsQueryOptions(projectPath),
    enabled: isWorktreeThread && !!projectPath,
  });
  const worktreeFolderMissing =
    isWorktreeThread && worktreeFolderQuery.data === false;
  const diffs = React.useMemo<AggregatedFileDiff[]>(
    () => diffStream.diffs,
    [diffStream.diffs],
  );

  // Keyboard shortcut for mode cycling (Shift+Tab)
  React.useEffect(() => {
    if (!session) return; // Only active when session exists

    const handleKeyDown = (event: KeyboardEvent) => {
      // Only respond to Shift+Tab
      if (event.key !== "Tab" || !event.shiftKey) return;

      // Skip when focus is on an INPUT or contenteditable — e.g.
      // title-rename, branch switcher search, diff style toggles —
      // where Shift+Tab should keep its default focus-navigation
      // behavior. The composer <textarea> is the only textarea in
      // the app and is intentionally NOT skipped: users want the
      // mode to cycle while typing without losing their cursor.
      const target = event.target as HTMLElement;
      if (target.tagName === "INPUT" || target.isContentEditable) {
        return;
      }

      // Prevent default Tab behavior (focus navigation)
      event.preventDefault();

      // Cycle to next mode. Local state only — the new mode rides out on
      // the next `send_turn`. Pushing `update_permission_mode` to the
      // daemon mid-stream flips the live SDK Query and drops the running
      // turn from view, which is exactly what we're avoiding here.
      const newMode = cycleMode(permissionMode, "forward");
      setPermissionMode(newMode);

      toast({
        description: `Mode: ${MODE_LABELS[newMode]}`,
        duration: 2000,
      });
    };

    window.addEventListener("keydown", handleKeyDown);
    return () => window.removeEventListener("keydown", handleKeyDown);
  }, [session, permissionMode]);

  // Escape interrupts the in-flight turn. Mirrors the "esc" hint shown in
  // the working indicator. The title-rename Escape handler is scoped to
  // its own input element, so this window-level listener doesn't clobber
  // it when a rename is in progress.
  React.useEffect(() => {
    if (!session) return;

    const handleKeyDown = (event: KeyboardEvent) => {
      if (event.key !== "Escape") return;
      if (session.status !== "running") return;
      event.preventDefault();
      sendMessage({ type: "interrupt_turn", session_id: sessionId }).catch(
        (err) => {
          console.error("Failed to interrupt turn", err);
        },
      );
    };

    window.addEventListener("keydown", handleKeyDown);
    return () => window.removeEventListener("keydown", handleKeyDown);
  }, [sessionId, session]);

  // Set active session
  React.useEffect(() => {
    dispatch({ type: "set_active_session", sessionId });
    return () => {
      dispatch({ type: "set_active_session", sessionId: null });
    };
  }, [sessionId, dispatch]);

  // Restore permission mode from the last persisted turn when the
  // user lands on a session with no sessionStorage entry (a full
  // page refresh, or the very first visit). Doesn't need to be
  // synchronous — the toolbar picker tolerates a one-frame delay,
  // and we don't want to clobber an explicit choice the user made
  // in sessionStorage by racing them on render.
  React.useEffect(() => {
    const data = sessionQuery.data;
    if (!data || data.detail.turns.length === 0) return;
    if (sessionStorage.getItem(permissionStorageKey)) return;
    const lastMode = [...data.detail.turns]
      .reverse()
      .find((t) => t.permissionMode)?.permissionMode;
    if (lastMode) {
      setPermissionMode(lastMode);
    }
  }, [sessionQuery.data, permissionStorageKey]);

  // Reset per-view transient UI state on every thread switch.
  // These are "what the user sees right now" state values — they
  // don't belong to any specific session long-term, but they must
  // not leak from the session the user is leaving. (Per-session
  // *turns* don't need to reset because they live in the query
  // cache, keyed by sessionId; pending permissions / questions
  // don't need to reset because they live in the global store
  // keyed by sessionId — switching threads just reads a different
  // entry.)
  React.useEffect(() => {
    setPendingInput(null);
    setLastEventAt(Date.now());
    setStuckSince(null);
  }, [sessionId]);

  // Single stream listener for the lifetime of ChatView. It
  // *never* reads sessionId from a closure — turn updates go into
  // the query cache entry identified by `event.session_id`, and
  // per-view side effects (pending input, permission prompts)
  // check against the `sessionIdRef` so only events for the
  // currently-visible thread touch transient UI state. That's
  // what makes cross-session isolation structural: a thread-A
  // content_delta that lands after the user has clicked over to
  // thread B writes into cache[A], updates exactly nothing on
  // screen, and silently waits for the user to come back to A.
  React.useEffect(() => {
    let active = true;

    connectStream((message) => {
      if (!active) return;

      if (message.type === "session_loaded") {
        // Replace the cache entry for the target session outright —
        // this is the lag-recovery path, where the daemon is telling
        // us "here is the authoritative state of session X right now".
        const detail = message.session;
        const targetId = detail.summary.sessionId;
        const totalTurns = detail.summary.turnCount ?? detail.turns.length;
        queryClient.setQueryData<SessionPage>(sessionQueryKey(targetId), {
          detail,
          loadedTurns: detail.turns.length,
          totalTurns,
          hasMoreOlder: detail.turns.length < totalTurns,
        });
        if (targetId === sessionIdRef.current) {
          setPendingInput(null);
          setLastEventAt(Date.now());
          setStuckSince(null);
          // Activate and refresh the diff subscription so the badge
          // reflects the current working-tree state after reconnection.
          activateDiffSubscription();
          refreshDiffs();
        }
        return;
      }

      if (message.type !== "event") return;
      const event = message.event;
      if (!("session_id" in event)) return;
      const eventSessionId = event.session_id;

      // Route turn mutations to the event's session cache. Events
      // whose session isn't in the cache (the user has never
      // visited) silently no-op — when the user eventually opens
      // that thread, useQuery fetches fresh data from the daemon.
      queryClient.setQueryData<SessionPage>(
        sessionQueryKey(eventSessionId),
        (prev) => {
          if (!prev) return prev;
          const nextTurns = applyEventToTurns(prev.detail.turns, event);
          if (nextTurns === prev.detail.turns) return prev;
          const total = Math.max(prev.totalTurns, nextTurns.length);
          return {
            ...prev,
            detail: { ...prev.detail, turns: nextTurns },
            loadedTurns: nextTurns.length,
            totalTurns: total,
            hasMoreOlder: nextTurns.length < total,
          };
        },
      );

      // Per-view UI state only moves for events on the currently-
      // visible session. Everything below here is "reset pending
      // chrome" / "scroll-to-bottom hints" / "router navigation" —
      // all current-view concerns.
      if (eventSessionId !== sessionIdRef.current) return;

      setLastEventAt(Date.now());
      setStuckSince(null);

      switch (event.type) {
        case "turn_started":
          // Clear the optimistic pending row now that the real turn
          // has been appended to the cache. The store handles
          // pendingPermissions/pendingQuestion clearing globally
          // (turn_completed / session_interrupted reducer paths).
          setPendingInput(null);
          break;

        case "turn_completed":
          setPendingInput(null);
          // Every completed turn activates the diff subscription
          // (idempotent after the first call) and restarts it so
          // the badge reflects what this turn left on disk. The
          // git work runs entirely on the Rust side via Tauri IPC
          // — non-blocking for the UI.
          activateDiffSubscription();
          refreshDiffs();
          break;

        // permission_requested / user_question_asked are handled in
        // the global store reducer (app-store.tsx). chat-view reads
        // pendingPermissions / pendingQuestion from the store, so a
        // prompt that arrives while the user is on a different
        // thread now lives in the store until they switch over.

        case "session_deleted":
        case "session_archived":
          // Active thread deleted / archived from elsewhere — bail
          // out so the user isn't staring at a title with no data.
          navigate({ to: "/" });
          break;
      }
    });

    return () => {
      active = false;
    };
  }, [queryClient, navigate, refreshDiffs, activateDiffSubscription]);

  async function handleSend(input: string, images: AttachedImage[] = []) {
    if (isArchived) {
      // Defense in depth — the composer is disabled when archived,
      // but slash-command and keyboard shortcut paths could still
      // call this; reject at the source.
      toast({
        description: "This thread is archived and can't accept new messages",
        duration: 3000,
      });
      return;
    }
    if (worktreeFolderMissing) {
      toast({
        description:
          "This worktree's folder no longer exists — recreate it to continue",
        duration: 3000,
      });
      return;
    }
    // --- Slash command interception ---
    const resolved = resolveCommand(input);
    if (resolved) {
      if (!resolved.command) {
        toast({
          description: `Unknown command: ${resolved.raw}`,
          duration: 3000,
        });
        return;
      }
      if (session?.status === "running") {
        toast({
          description: "Cannot run commands while a turn is in progress",
          duration: 3000,
        });
        return;
      }
      if (!session) {
        toast({ description: "No active session", duration: 3000 });
        return;
      }
      const ctx: SlashCommandContext = {
        sessionId,
        session,
        send,
        navigate,
        toast,
      };
      await resolved.command.execute(ctx, resolved.args);
      return;
    }

    // --- Normal message flow ---

    // First-turn auto-title. Mirrors what the SDK's orchestration layer
    // used to do before display metadata moved app-side; see the zenui
    // reference in rs-agent-sdk/apps/zenui/frontend/src/state/appStore.ts.
    const existingDisplay = state.sessionDisplay.get(sessionId);
    if (
      session &&
      session.turnCount === 0 &&
      !existingDisplay?.title
    ) {
      const autoTitle = input
        .split(/\s+/)
        .filter(Boolean)
        .slice(0, 10)
        .join(" ");
      if (autoTitle.length > 0) {
        void renameSession(sessionId, autoTitle);
      }
    }

    // Optimistic: show the user's message immediately, then await the
    // round-trip. turn_started will clear this and replace it with the
    // real turn from the daemon.
    setPendingInput(input);
    try {
      await sendMessage({
        type: "send_turn",
        session_id: sessionId,
        input,
        images: images.map((img) => ({
          media_type: img.mediaType,
          data_base64: img.dataBase64,
          name: img.name,
        })),
        permission_mode: permissionMode,
        reasoning_effort: effort,
      });
    } catch (err) {
      setPendingInput(null);
      throw err;
    }
  }

  async function handleInterrupt() {
    await sendMessage({ type: "interrupt_turn", session_id: sessionId });
  }

  async function handlePermissionDecision(
    decision: PermissionDecision,
    modeOverride?: PermissionMode,
  ) {
    // Always act on the head of the queue — that's what the user
    // just clicked on. Pop it before the await so a rapid double
    // click can't answer the same request twice, and so the next
    // queued prompt becomes visible immediately.
    const head = pendingPermissions[0];
    if (!head) return;
    dispatch({
      type: "consume_pending_permission",
      sessionId,
      requestId: head.requestId,
    });
    await sendMessage({
      type: "answer_permission",
      session_id: sessionId,
      request_id: head.requestId,
      decision,
      ...(modeOverride ? { permission_mode_override: modeOverride } : {}),
    });
    if (modeOverride) {
      // Mirror the chosen mode into local state so the toolbar dropdown
      // and the next send_turn pick it up. The Claude SDK side already
      // applies the mode via the bundled updatedPermissions, so this is
      // purely a UI sync — no second daemon round-trip.
      setPermissionMode(modeOverride);
    }
  }

  async function handleQuestionSubmit(answers: UserInputAnswer[]) {
    if (!pendingQuestion) return;
    const requestId = pendingQuestion.requestId;
    dispatch({ type: "consume_pending_question", sessionId, requestId });
    await sendMessage({
      type: "answer_question",
      session_id: sessionId,
      request_id: requestId,
      answers,
    });
  }

  async function handleQuestionCancel() {
    if (!pendingQuestion) return;
    const requestId = pendingQuestion.requestId;
    dispatch({ type: "consume_pending_question", sessionId, requestId });
    await sendMessage({
      type: "cancel_question",
      session_id: sessionId,
      request_id: requestId,
    });
  }

  const isRunning = session?.status === "running";
  // The in-flight turn (if any). Used to drive the WorkingIndicator's
  // elapsed-time clock from the daemon-side createdAt timestamp so the
  // counter doesn't drift between client and server.
  const runningTurn = React.useMemo(() => {
    if (!isRunning) return null;
    for (let i = turns.length - 1; i >= 0; i--) {
      if (turns[i].status === "running") return turns[i];
    }
    return null;
  }, [isRunning, turns]);

  // Is there at least one tool call on the running turn still waiting
  // for its completion event? That's the precondition for the
  // stuck-watchdog: we don't care about ordinary model thinking
  // latency, only about cases where a tool is visibly in "pending"
  // and nothing is moving.
  const hasPendingToolCall = React.useMemo(() => {
    if (!runningTurn) return false;
    return (runningTurn.toolCalls ?? []).some((tc) => tc.status === "pending");
  }, [runningTurn]);

  // Arm the stuck-watchdog. We only trip it when the session is
  // running *and* at least one tool call is pending, so idle
  // pre-tool "Thinking…" periods don't falsely flag as stuck. The
  // timer is rearmed by `lastEventAt` bumping on each event.
  React.useEffect(() => {
    if (!isRunning || !hasPendingToolCall) {
      setStuckSince(null);
      return;
    }
    const now = Date.now();
    const elapsed = now - lastEventAt;
    if (elapsed >= STUCK_TIMEOUT_MS) {
      setStuckSince(lastEventAt);
      return;
    }
    const id = setTimeout(() => {
      setStuckSince(lastEventAt);
    }, STUCK_TIMEOUT_MS - elapsed);
    return () => clearTimeout(id);
  }, [isRunning, hasPendingToolCall, lastEventAt]);

  const title = state.sessionDisplay.get(sessionId)?.title || "New thread";

  const [editingTitle, setEditingTitle] = React.useState(false);
  const [titleDraft, setTitleDraft] = React.useState(title);
  const titleInputRef = React.useRef<HTMLInputElement>(null);

  React.useEffect(() => {
    setTitleDraft(title);
  }, [title]);

  React.useEffect(() => {
    if (editingTitle) {
      titleInputRef.current?.focus();
      titleInputRef.current?.select();
    }
  }, [editingTitle]);

  function commitTitleRename() {
    const trimmed = titleDraft.trim();
    setEditingTitle(false);
    if (trimmed && trimmed !== title) {
      void renameSession(sessionId, trimmed);
    }
  }

  // Mode changes are local-only: the new mode rides out on the next
  // `send_turn`, so the running turn stays untouched. Plan-exit mode
  // switches go through `handlePermissionDecision` /
  // `permission_mode_override`, which is the sanctioned atomic path.
  const handlePermissionModeChange = React.useCallback(
    (mode: PermissionMode) => {
      setPermissionMode(mode);
    },
    [],
  );

  const toolbar = session ? (
    <ChatToolbar
      sessionId={sessionId}
      provider={session.provider}
      currentModel={session.model}
      effort={effort}
      onEffortChange={setEffort}
      permissionMode={permissionMode}
      onPermissionModeChange={handlePermissionModeChange}
    />
  ) : null;

  return (
    <div className="flex h-svh min-w-0 flex-col overflow-hidden">
      <header className="flex h-12 shrink-0 items-center gap-2 border-b border-border px-2 text-sm">
        <SidebarTrigger />
        <div className="flex min-w-0 flex-col leading-tight">
          {editingTitle ? (
            <input
              ref={titleInputRef}
              className="min-w-0 truncate rounded border border-input bg-background px-1.5 py-0.5 text-sm font-medium outline-none"
              value={titleDraft}
              onChange={(e) => setTitleDraft(e.target.value)}
              onBlur={commitTitleRename}
              onKeyDown={(e) => {
                if (e.key === "Enter") commitTitleRename();
                if (e.key === "Escape") {
                  setTitleDraft(title);
                  setEditingTitle(false);
                }
              }}
            />
          ) : (
            <span
              className="cursor-pointer truncate font-medium hover:text-muted-foreground"
              onClick={() => setEditingTitle(true)}
            >
              {title}
            </span>
          )}
          <div className="flex items-center gap-2">
            {gitBranch && projectPath && session && parentProjectId && parentProjectPath && (
              <BranchSwitcher
                projectPath={projectPath}
                currentBranch={gitBranch}
                parentProjectId={parentProjectId}
                parentProjectPath={parentProjectPath}
                provider={session.provider}
                model={session.model ?? null}
                onCheckedOut={() => refreshDiffs({ force: true })}
              />
            )}
            {providerDisabled && (
              <span className="inline-flex shrink-0 items-center rounded-full border border-destructive/30 bg-destructive/10 px-2 py-0.5 text-[10px] font-medium text-destructive">
                Provider disabled
              </span>
            )}
          </div>
        </div>
        <div className="ml-auto flex items-center gap-2">
          <HeaderActions
            sessionId={sessionId}
            projectPath={projectPath}
            diffs={diffs}
            diffOpen={diffOpen}
            onToggleDiff={() => {
              setDiffOpen((v) => {
                if (!v) {
                  // First interaction with the diff button of any
                  // kind activates the subscription. `refreshDiffs`
                  // then bumps the tick so the newly-opened panel
                  // picks up any on-disk changes made outside the
                  // agent — but unlike the old path this does NOT
                  // blank out the badge, because the streamed hook
                  // keeps the previous diffs committed until the
                  // new subscription's Phase 1 lands.
                  activateDiffSubscription();
                  refreshDiffs({ force: true });
                }
                return !v;
              });
            }}
            // Hover arms the subscription (exactly once, via the
            // React setState bail-out). No tick bump, no refetch —
            // the subscription fires as soon as it's activated and
            // subsequent hovers are no-ops. This is the fix for the
            // old button-badge flicker: hovers never restart the
            // query, so the `+N/−M` count stays visible.
            onHoverDiff={diffOpen ? undefined : activateDiffSubscription}
          />
        </div>
      </header>

      {/* Horizontal split: chat column on the left, optional diff pane
          on the right. min-w-0 on the outer row lets the left column
          shrink below its content's intrinsic width so wide messages
          or code blocks don't push the diff pane off-screen. */}
      <div
        ref={splitContainerRef}
        className="flex min-h-0 min-w-0 flex-1"
      >
        <div
          className={cn(
            "flex min-w-0 flex-col",
            diffFullscreen ? "hidden" : "flex-1",
          )}
        >
          <MessageList
            sessionId={sessionId}
            turns={turns}
            loading={loading}
            pendingInput={pendingInput}
            hiddenOlderCount={hiddenOlderCount}
            loadingOlder={loadingOlder}
            onLoadOlder={handleLoadOlder}
            onOpenAttachment={handleOpenPersistedAttachment}
          />

          {isRunning && session && runningTurn && (
            <WorkingIndicator
              turnStartedAt={new Date(runningTurn.createdAt).getTime()}
              lastEventAt={lastEventAt}
              onInterrupt={handleInterrupt}
            />
          )}

          {pendingQuestion && (
            <QuestionPrompt
              questions={pendingQuestion.questions}
              onSubmit={handleQuestionSubmit}
              onCancel={handleQuestionCancel}
            />
          )}

          {pendingPermissions.length > 0 && (
            <PermissionPrompt
              // Head-of-queue. The `key` forces React to remount the
              // prompt so any local component state (e.g. the plan-exit
              // mode picker's `pending` flag) resets between queued
              // prompts and the user can't accidentally double-answer
              // the next one with stale state.
              key={pendingPermissions[0].requestId}
              toolName={pendingPermissions[0].toolName}
              input={pendingPermissions[0].input}
              onDecision={handlePermissionDecision}
              queueDepth={pendingPermissions.length}
            />
          )}

          {stuckSince !== null &&
            pendingPermissions.length === 0 &&
            !pendingQuestion && (
              <StuckBanner
                elapsedSeconds={Math.floor((Date.now() - stuckSince) / 1000)}
                onInterrupt={() => {
                  setStuckSince(null);
                  handleInterrupt();
                }}
                onReload={() => {
                  setStuckSince(null);
                  // Invalidate the cache entry and force a refetch so
                  // the next render re-seeds `turns` with whatever the
                  // daemon has now. The fetched `SessionPage` replaces
                  // the cache entry, and the streaming handler picks
                  // up from there on the next session_loaded reseed.
                  void queryClient.invalidateQueries({
                    queryKey: sessionQueryKey(sessionId),
                    refetchType: "active",
                  });
                }}
              />
            )}

          {isArchived && (
            <div className="mx-4 mb-2 rounded border border-destructive/30 bg-destructive/10 px-3 py-1.5 text-[11px] font-medium text-destructive">
              This thread is archived — read-only history. Archived
              conversations can't receive new messages.
            </div>
          )}
          {!isArchived && worktreeFolderMissing && (
            <div className="mx-4 mb-2 rounded border border-destructive/30 bg-destructive/10 px-3 py-1.5 text-[11px] font-medium text-destructive">
              This worktree's folder no longer exists — read-only
              history. Recreate the worktree to keep working on it.
            </div>
          )}
          <ChatInput
            // Remount the composer on every thread switch so its
            // internal state (textarea draft, pendingSend queue
            // flag, slash-command popup) resets cleanly. Without
            // the key the draft from session A would leak into
            // session B; the old pendingSend bug — where a queued
            // message in A auto-flushed against B the moment B's
            // status flipped to ready — is the scarier version
            // of the same leak.
            key={sessionId}
            onSend={handleSend}
            onInterrupt={handleInterrupt}
            sessionStatus={session?.status}
            disabled={loading}
            providerDisabled={providerDisabled}
            archived={isArchived || worktreeFolderMissing}
            toolbar={toolbar}
            commands={COMMAND_META}
          />
        </div>

        {diffOpen && (
          <>
            {!diffFullscreen && (
              <DiffDragHandle
                containerRef={splitContainerRef}
                width={diffWidth}
                onResize={setDiffWidth}
              />
            )}
            <aside
              className={cn(
                "border-l border-border bg-background",
                diffFullscreen ? "flex-1" : "shrink-0",
              )}
              style={diffFullscreen ? undefined : { width: diffWidth }}
            >
              <DiffPanel
                projectPath={projectPath}
                diffs={diffs}
                refreshKey={diffRefreshTick}
                streamStatus={diffStream.status}
                style={diffStyle}
                onStyleChange={setDiffStyle}
                onClose={() => {
                  setDiffOpen(false);
                  setDiffFullscreen(false);
                }}
                isFullscreen={diffFullscreen}
                onToggleFullscreen={() => setDiffFullscreen((v) => !v)}
              />
            </aside>
          </>
        )}
      </div>
      {persistedLightboxRef && (
        <ImageLightbox
          source={{ kind: "persisted", ref: persistedLightboxRef }}
          onClose={() => setPersistedLightboxRef(null)}
        />
      )}
    </div>
  );
}
