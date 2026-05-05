import * as React from "react";
import { useNavigate } from "@tanstack/react-router";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { SidebarTrigger, useSidebar } from "@/components/ui/sidebar";
import { cn } from "@/lib/utils";
import { isMacOS, isPopoutWindow } from "@/lib/popout";
import { deriveAutoTitle } from "@/lib/auto-title";
import {
  TOGGLE_CONTEXT_EVENT,
  TOGGLE_DIFF_EVENT,
  TOGGLE_CODE_VIEW_EVENT,
  OPEN_CODE_VIEW_EVENT,
} from "@/lib/keyboard-shortcuts";
import { CodeView, type SearchMode } from "@/components/code/code-view";
import { useApp, useSessionCommandCatalog } from "@/stores/app-store";
import type {
  AttachedImage,
  AttachmentRef,
  PermissionDecision,
  PermissionMode,
  ReasoningEffort,
  ThinkingMode,
  TurnRecord,
  UserInputAnswer,
  UserInputQuestion,
} from "@/lib/types";
import { sendMessage } from "@/lib/api";
import { useSessionStreamSubscription } from "@/hooks/useSessionStreamSubscription";
import {
  gitBranchQueryOptions,
  gitRootQueryOptions,
  loadFullSession,
  pathExistsQueryOptions,
  sessionQueryOptions,
} from "@/lib/queries";
import { useStreamedGitDiffSummary } from "@/lib/git-diff-stream";
import { cycleMode, MODE_LABELS } from "@/lib/mode-cycling";
import { toneForMode } from "@/lib/mode-tone";
import {
  readDefaultEffort,
  readDefaultPermissionMode,
} from "@/lib/defaults-settings";
import {
  clampEffortToModel,
  clampThinkingModeToModel,
  readPickedModel,
} from "@/lib/model-settings";
import { resolveModelDisplay } from "@/lib/model-lookup";
import {
  mergeCommandsWithCatalog,
  resolveCommand,
  type SlashCommandContext,
} from "@/lib/slash-commands";
import { toast } from "@/hooks/use-toast";
import { useProviderEnabled } from "@/hooks/use-provider-enabled";
import { useProviderFeatures } from "@/hooks/use-provider-features";
import { MessageList } from "./messages/message-list";
import { SessionProvider } from "./session-context";
import { ChatInput, type QueuedMessage } from "./chat-input";
import { PermissionPrompt } from "./permission-prompt";
import { QuestionPrompt } from "./question-prompt";
import { ChatToolbar } from "./chat-toolbar";
import { HeaderActions } from "./header-actions";
import { BranchSwitcher } from "./branch-switcher";
import { WorkingIndicator } from "./working-indicator";
import { ApiRetryBanner } from "./api-retry-banner";
import { AgentContextPanel } from "./agent-context-panel";
import {
  findLatestMainTodoWrite,
  parseTodoProgress,
} from "@/lib/todo-extract";
// import { StuckBanner } from "./stuck-banner"; // commented: stuck banner disabled for now
import { DiffPanel, type DiffStyle } from "./diff-panel";
import { ImageLightbox } from "./image-lightbox";
import type { AggregatedFileDiff } from "@/lib/session-diff";
import { sessionTransient } from "@/stores/session-transient-store";

// Per-session draft text. Module-level so it survives ChatView
// re-renders and ChatInput remounts (keyed by sessionId). Cleared
// on send so completed messages don't linger as stale drafts.
const sessionDrafts = new Map<string, string>();

// Per-session message queue. Module-level so it survives ChatInput
// remounts (keyed by sessionId). Same pattern as sessionDrafts —
// preserves queued messages when the user switches threads mid-turn.
const sessionQueues = new Map<string, QueuedMessage[]>();

// Per-session diff / context / code-view panel open flags. Module-level
// so they survive thread switches (ChatView does NOT remount on
// sessionId change — sessionId is a prop, not a key). Lost on reload,
// which is intentional: "did I leave the diff open" is transient UI
// state, not a persisted preference. Width / style live in localStorage
// below because those ARE preferences. Fullscreen is deliberately NOT
// per-thread (plain useState) — it's a momentary intent.
//
// Diff and Context still use these module-local maps for now; only the
// code-view flag has been promoted to the typed `sessionTransient`
// store next to the editor's per-session git-mode flag. Migrating
// diff/context is the obvious next step but isn't this change's scope.
const sessionDiffOpen = new Map<string, boolean>();
const sessionContextOpen = new Map<string, boolean>();

// Trip the watchdog after this many seconds of silence while a tool
// call is pending. Picked to be well past a normal tool round-trip
// (even a slow Bash / Git command rarely exceeds 15–20s) but short
// enough that a user who just clicked Allow doesn't sit for a minute
// wondering if anything is happening.
const STUCK_TIMEOUT_MS = 45_000;

// Diff-panel sizing. Clamped so neither the chat column nor the diff
// pane can collapse to nothing when the user drags the handle.
const DIFF_WIDTH_KEY = "flowstate:diff-width";
const DIFF_STYLE_KEY = "flowstate:diff-style";
const DIFF_MIN_WIDTH = 360;
const DIFF_DEFAULT_WIDTH = 560;
const DIFF_CHAT_MIN_WIDTH = 420;

const CONTEXT_WIDTH_KEY = "flowstate:context-width";
const CONTEXT_MIN_WIDTH = 320;
const CONTEXT_DEFAULT_WIDTH = 440;

const CODE_VIEW_WIDTH_KEY = "flowstate:code-view-width";
const CODE_VIEW_MIN_WIDTH = 480;
const CODE_VIEW_DEFAULT_WIDTH = 720;

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


// Vertical drag handle between the chat column and the diff pane.
// Mirrors the sidebar DragHandle pattern in router.tsx but measures
// against the split container's right edge so the panel grows from
// the right as the mouse moves left. The handle lives inline between
// the two flex children (not absolutely positioned) to avoid z-index
// fights with the sidebar handle and other overlays. Generic over
// storageKey/minWidth so both the diff pane and the agent-context
// pane can reuse the same primitive with their own persisted width.
function PanelDragHandle({
  containerRef,
  width,
  onResize,
  storageKey,
  minWidth,
  ariaLabel,
}: {
  containerRef: React.RefObject<HTMLDivElement | null>;
  width: number;
  onResize: (w: number) => void;
  storageKey: string;
  minWidth: number;
  ariaLabel: string;
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
        minWidth,
        Math.floor(rect.width - DIFF_CHAT_MIN_WIDTH),
      );
      const next = Math.max(
        minWidth,
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
          storageKey,
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
  }, [containerRef, onResize, storageKey, minWidth]);

  return (
    <div
      role="separator"
      aria-label={ariaLabel}
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

  // macOS traffic-light spacer is only needed when the chat header is
  // the leftmost element of the window. With the sidebar expanded the
  // traffic lights sit over the SidebarHeader instead, so the spacer
  // here would just be wasted space. In a popout there's no sidebar
  // mounted at all, so always show the spacer there.
  const { state: sidebarState } = useSidebar();
  const inPopoutWindow = isPopoutWindow();
  const showMacTrafficSpacer =
    isMacOS() && (inPopoutWindow || sidebarState === "collapsed");

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
  const effortStorageKey = `flowstate:effort:${sessionId}`;
  const [effort, setEffortState] = React.useState<ReasoningEffort>(
    () =>
      (sessionStorage.getItem(effortStorageKey) as ReasoningEffort) ?? "high",
  );
  // Per-thread thinking-mode toggle. Default = "always" mirrors the
  // bridge default: restores the pre-`11232b3` deterministic reasoning
  // behaviour. Users who prefer the SDK's adaptive non-determinism can
  // flip this per thread in the composer toolbar.
  const thinkingModeStorageKey = `flowstate:thinkingMode:${sessionId}`;
  const [thinkingMode, setThinkingModeState] = React.useState<ThinkingMode>(
    () =>
      (sessionStorage.getItem(thinkingModeStorageKey) as ThinkingMode) ??
      "always",
  );
  const permissionStorageKey = `flowstate:permissionMode:${sessionId}`;
  const [permissionMode, setPermissionModeState] =
    React.useState<PermissionMode>(
      () =>
        (sessionStorage.getItem(permissionStorageKey) as PermissionMode) ??
        "accept_edits",
    );

  // Committed setters — every explicit `setPermissionMode` /
  // `setEffort` call persists to sessionStorage and (for mode) mirrors
  // to the app-store so the sidebar stays in sync. The raw useState
  // setters `setPermissionModeState` / `setEffortState` are reserved
  // for the session-switch reset below: that re-initializes React
  // state from the NEW session's sessionStorage, and must not write
  // (writing would clobber the "never visited" signal that the
  // fresh-thread-defaults and turn-history-restore effects rely on
  // via their `if (sessionStorage.getItem(...)) return` guards).
  const setPermissionMode = React.useCallback(
    (mode: PermissionMode) => {
      setPermissionModeState(mode);
      sessionStorage.setItem(permissionStorageKey, mode);
      dispatch({
        type: "set_session_permission_mode",
        sessionId,
        mode,
      });
    },
    [dispatch, permissionStorageKey, sessionId],
  );
  const setEffort = React.useCallback(
    (eff: ReasoningEffort) => {
      setEffortState(eff);
      sessionStorage.setItem(effortStorageKey, eff);
    },
    [effortStorageKey],
  );
  const setThinkingMode = React.useCallback(
    (m: ThinkingMode) => {
      setThinkingModeState(m);
      sessionStorage.setItem(thinkingModeStorageKey, m);
    },
    [thinkingModeStorageKey],
  );

  // Reset composer state when switching threads. `ChatView` doesn't
  // remount on session-id change (see the sibling reset effect
  // further down), so without this the `permissionMode` / `effort`
  // React state leaks from the thread we just left — e.g. leaving
  // thread A in `plan` mode and clicking into thread B would render
  // thread B's composer badge as `plan` even though B had nothing to
  // do with that choice. Re-reading sessionStorage here is
  // equivalent to what the lazy useState initializer did on first
  // mount; using the raw setters (not the committed ones) keeps
  // sessionStorage untouched for the new session so the
  // fresh-thread-defaults and turn-history-restore effects below can
  // still detect a never-visited thread. `useLayoutEffect` runs
  // synchronously before paint so there's no one-frame flash of the
  // stale badge. The `dispatch` mirrors the resolved mode to the
  // app-store so the sidebar tint matches the composer on every
  // thread switch.
  React.useLayoutEffect(() => {
    const storedMode =
      (sessionStorage.getItem(permissionStorageKey) as PermissionMode) ??
      "accept_edits";
    const storedEffort =
      (sessionStorage.getItem(effortStorageKey) as ReasoningEffort) ?? "high";
    const storedThinkingMode =
      (sessionStorage.getItem(thinkingModeStorageKey) as ThinkingMode) ??
      "always";
    setPermissionModeState(storedMode);
    setEffortState(storedEffort);
    setThinkingModeState(storedThinkingMode);
    dispatch({
      type: "set_session_permission_mode",
      sessionId,
      mode: storedMode,
    });
  }, [
    sessionId,
    permissionStorageKey,
    effortStorageKey,
    thinkingModeStorageKey,
    dispatch,
  ]);

  // Load user-configured defaults from Settings for freshly-created
  // threads only. "Fresh" = session has loaded AND has zero turns yet
  // (i.e. the user just created this thread and hasn't sent anything).
  // Applying the default on every thread switch is wrong: existing
  // threads carry their own mode/effort forward (see the
  // restore-from-last-turn effect below for permissionMode), and a
  // stale sessionStorage miss — e.g. first visit to an older thread
  // in a new browser session — must not clobber that. We also
  // short-circuit when sessionStorage already has a value, so an
  // in-session choice survives navigation away and back.
  React.useEffect(() => {
    if (sessionStorage.getItem(effortStorageKey)) return;
    const data = sessionQuery.data;
    if (!data || data.detail.turns.length > 0) return;
    let cancelled = false;
    readDefaultEffort().then((saved) => {
      if (!cancelled && saved) setEffort(saved);
    });
    return () => {
      cancelled = true;
    };
  }, [effortStorageKey, sessionQuery.data]);

  React.useEffect(() => {
    if (sessionStorage.getItem(permissionStorageKey)) return;
    const data = sessionQuery.data;
    if (!data || data.detail.turns.length > 0) return;
    let cancelled = false;
    readDefaultPermissionMode().then((saved) => {
      if (!cancelled && saved) setPermissionMode(saved);
    });
    return () => {
      cancelled = true;
    };
  }, [permissionStorageKey, sessionQuery.data]);

  // (Strict Plan Mode auto-deny lives in <RoutePromptOverlay /> so it
  // fires regardless of which route the user is on — otherwise a
  // mutating-tool request that arrived while the user was browsing
  // /code would sit undenied until they navigated back to /chat. The
  // overlay reads the same `readStrictPlanMode` setting and looks at
  // the session's `permissionModeBySession` entry that this view's
  // `setPermissionMode` wrapper keeps up to date.)

  // (Effort / permissionMode are persisted to sessionStorage +
  // dispatched to the app-store inside their wrapped setters above —
  // no standalone auto-persist effect. An auto-persist effect here
  // would race with the session-switch reset: on a sessionId change,
  // React state still holds the previous thread's value for one
  // render, and an auto-persist effect would write it to the new
  // thread's storage key before the reset has a chance to fix it.)

  const [pendingInput, setPendingInput] = React.useState<string | null>(null);
  // Monotonically-increasing tick bumped each time the user dispatches a
  // message via handleSend. MessageList watches this to force a scroll-
  // to-bottom on every send, regardless of current scroll position. A
  // counter (rather than a boolean) ensures every send fires the effect
  // even when consecutive sends would otherwise debounce to the same value.
  const [userSendTick, setUserSendTick] = React.useState(0);
  // Watchdog state: `lastEventAt` bumps on every stream event for this
  // session so the 45s inactivity timer resets. `stuckSince` is set
  // when the timer fires and a pending tool call exists; rendering the
  // StuckBanner keys off it.
  const [lastEventAt, setLastEventAt] = React.useState<number>(() =>
    Date.now(),
  );
  // `stuckSince` is still driven by the watchdog but unread while the
  // StuckBanner is disabled — prefix with _ so tsc doesn't complain.
  const [_stuckSince, setStuckSince] = React.useState<number | null>(null);
  // Coarse turn phase ("requesting" / "compacting" / …). Provider-
  // driven; only Claude SDK emits today. Cleared on turn_completed
  // so the stale label doesn't linger onto the next turn.
  const [turnPhase, setTurnPhase] = React.useState<
    import("@/lib/types").TurnPhase | undefined
  >(undefined);
  // In-flight auto-retry banner state. Set from `turn_retrying`
  // events; cleared on the first subsequent `content_delta` (model
  // started responding, retry succeeded) or `turn_completed` /
  // `session_interrupted`.
  const [retryState, setRetryState] = React.useState<
    import("@/lib/types").RetryState | null
  >(null);
  // Latest predicted next prompt from `prompt_suggested` events.
  // Rendered as ghost text in the empty composer; any keystroke,
  // new turn start, or turn completion clears it so stale
  // suggestions don't linger.
  const [promptSuggestion, setPromptSuggestion] = React.useState<string | null>(
    null,
  );
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
  const [diffOpen, setDiffOpenState] = React.useState<boolean>(
    () => sessionDiffOpen.get(sessionId) ?? false,
  );
  const [diffFullscreen, setDiffFullscreen] = React.useState(false);
  // Write-through wrapper: every update also records the session's
  // new open flag in the module-level map so it's still there when
  // the user returns to this thread after visiting another.
  const setDiffOpen = React.useCallback<
    React.Dispatch<React.SetStateAction<boolean>>
  >(
    (value) => {
      setDiffOpenState((prev) => {
        const next =
          typeof value === "function"
            ? (value as (p: boolean) => boolean)(prev)
            : value;
        if (next) sessionDiffOpen.set(sessionId, true);
        else sessionDiffOpen.delete(sessionId);
        return next;
      });
    },
    [sessionId],
  );

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

  // Agent-context pane state — mirrors the diff pane state. The two
  // panes are mutually exclusive (enforced in the toggle handlers
  // below); they share the split-right slot inside splitContainerRef.
  const [contextOpen, setContextOpenState] = React.useState<boolean>(
    () => sessionContextOpen.get(sessionId) ?? false,
  );
  const [contextFullscreen, setContextFullscreen] = React.useState(false);
  const setContextOpen = React.useCallback<
    React.Dispatch<React.SetStateAction<boolean>>
  >(
    (value) => {
      setContextOpenState((prev) => {
        const next =
          typeof value === "function"
            ? (value as (p: boolean) => boolean)(prev)
            : value;
        if (next) sessionContextOpen.set(sessionId, true);
        else sessionContextOpen.delete(sessionId);
        return next;
      });
    },
    [sessionId],
  );
  const [contextWidth, setContextWidth] = React.useState<number>(() => {
    try {
      const saved = window.localStorage.getItem(CONTEXT_WIDTH_KEY);
      if (saved) {
        const parsed = Number.parseInt(saved, 10);
        if (Number.isFinite(parsed) && parsed >= CONTEXT_MIN_WIDTH) {
          return parsed;
        }
      }
    } catch {
      /* storage may be unavailable */
    }
    return CONTEXT_DEFAULT_WIDTH;
  });

  // Code-view panel state — same shape as the diff/context panels:
  // per-session open flag in a module map so the choice survives
  // thread switches, transient fullscreen flag (no need to persist
  // it — momentary intent), and a localStorage-backed width.
  // Mutually exclusive with diff/context inside the toggle handler.
  const [codeViewOpen, setCodeViewOpenState] = React.useState<boolean>(
    () => sessionTransient.getCodeViewOpen(sessionId),
  );
  const [codeViewFullscreen, setCodeViewFullscreen] = React.useState(false);
  // Fresh-reference object passed down to CodeView's `searchRequest`
  // prop. Each Cmd+P / Cmd+Shift+F press creates a new object so the
  // effect on the other side fires even when the mode is unchanged
  // (which is the common case — repeated Cmd+P should re-focus the
  // input). Null between presses; never resets to null after first
  // use (no need — CodeView's effect bails on null and the prop only
  // matters at the moment of focus).
  const [codeViewSearchRequest, setCodeViewSearchRequest] = React.useState<
    { mode: SearchMode } | null
  >(null);
  const setCodeViewOpen = React.useCallback<
    React.Dispatch<React.SetStateAction<boolean>>
  >(
    (value) => {
      setCodeViewOpenState((prev) => {
        const next =
          typeof value === "function"
            ? (value as (p: boolean) => boolean)(prev)
            : value;
        sessionTransient.setCodeViewOpen(sessionId, next);
        return next;
      });
    },
    [sessionId],
  );
  const [codeViewWidth, setCodeViewWidth] = React.useState<number>(() => {
    try {
      const saved = window.localStorage.getItem(CODE_VIEW_WIDTH_KEY);
      if (saved) {
        const parsed = Number.parseInt(saved, 10);
        if (Number.isFinite(parsed) && parsed >= CODE_VIEW_MIN_WIDTH) {
          return parsed;
        }
      }
    } catch {
      /* storage may be unavailable */
    }
    return CODE_VIEW_DEFAULT_WIDTH;
  });

  // Single toggle handler for the diff pane, shared by the header
  // button and the global ⌘⇧D shortcut. Encapsulates everything that
  // happens on "open": activating the streamed-diff subscription,
  // forcing a refresh tick so the panel reflects on-disk changes,
  // and closing the mutually-exclusive context pane. On "close" it
  // also drops fullscreen so a re-open isn't surprising.
  const toggleDiff = React.useCallback(() => {
    setDiffOpen((v) => {
      if (!v) {
        activateDiffSubscription();
        refreshDiffs({ force: true });
        setContextOpen(false);
        setContextFullscreen(false);
        setCodeViewOpen(false);
        setCodeViewFullscreen(false);
      } else {
        setDiffFullscreen(false);
      }
      return !v;
    });
  }, [
    setDiffOpen,
    activateDiffSubscription,
    refreshDiffs,
    setContextOpen,
    setContextFullscreen,
    setDiffFullscreen,
    setCodeViewOpen,
    setCodeViewFullscreen,
  ]);

  // Code-view panel toggle. Mutually exclusive with diff/context —
  // the panel column has one slot. On open, drops the other panels
  // and any of their fullscreens; on close, drops fullscreen on
  // self so a re-open isn't surprising.
  const toggleCodeView = React.useCallback(() => {
    setCodeViewOpen((v) => {
      if (!v) {
        setDiffOpen(false);
        setDiffFullscreen(false);
        setContextOpen(false);
        setContextFullscreen(false);
      } else {
        setCodeViewFullscreen(false);
      }
      return !v;
    });
  }, [
    setCodeViewOpen,
    setDiffOpen,
    setDiffFullscreen,
    setContextOpen,
    setContextFullscreen,
    setCodeViewFullscreen,
  ]);

  // Bridge for the global Mod+Alt+E shortcut. Same indirection
  // pattern as TOGGLE_DIFF_EVENT.
  React.useEffect(() => {
    window.addEventListener(TOGGLE_CODE_VIEW_EVENT, toggleCodeView);
    return () =>
      window.removeEventListener(TOGGLE_CODE_VIEW_EVENT, toggleCodeView);
  }, [toggleCodeView]);

  // Bridge for Cmd+P (files) / Cmd+Shift+F (content). The shortcuts
  // dispatch OPEN_CODE_VIEW_EVENT with a `mode` detail. We force the
  // panel open (mutually exclusive with diff/context) and push a
  // fresh searchRequest object down to CodeView so it sets the mode
  // and focuses the search input. The fresh-reference approach is
  // what makes a second press of the same shortcut still re-focus.
  React.useEffect(() => {
    function onOpenCodeView(e: Event) {
      const detail = (e as CustomEvent<{ mode?: SearchMode }>).detail;
      const mode: SearchMode = detail?.mode ?? "files";
      setCodeViewOpen(true);
      setDiffOpen(false);
      setDiffFullscreen(false);
      setContextOpen(false);
      setContextFullscreen(false);
      // Always allocate a new object so the prop reference changes
      // and CodeView's effect re-fires. `setCodeViewSearchRequest`
      // is allowed to be called with the same value object only when
      // the user genuinely hasn't pressed the shortcut again — but
      // in practice the press IS the trigger, so always-new is
      // correct and cheaper than equality checks.
      setCodeViewSearchRequest({ mode });
    }
    window.addEventListener(OPEN_CODE_VIEW_EVENT, onOpenCodeView);
    return () =>
      window.removeEventListener(OPEN_CODE_VIEW_EVENT, onOpenCodeView);
  }, [
    setCodeViewOpen,
    setDiffOpen,
    setDiffFullscreen,
    setContextOpen,
    setContextFullscreen,
  ]);

  // Shift+Esc — toggle fullscreen on whichever side panel is open.
  // Fires regardless of focus (chat composer, code editor, diff
  // viewer all qualify) since it's a layout-level action. Mirrors
  // the per-pane Shift+Esc that already lives in the standalone
  // /code route — when the code view is mounted as a panel here,
  // its internal Shift+Esc handler is suppressed via the `embedded`
  // prop and this handler takes over with the more useful "expand
  // the whole panel" semantic.
  React.useEffect(() => {
    function onKeyDown(e: KeyboardEvent) {
      if (e.key !== "Escape" || !e.shiftKey) return;
      if (codeViewOpen) {
        e.preventDefault();
        setCodeViewFullscreen((v) => !v);
      } else if (diffOpen) {
        e.preventDefault();
        setDiffFullscreen((v) => !v);
      } else if (contextOpen) {
        e.preventDefault();
        setContextFullscreen((v) => !v);
      }
    }
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [codeViewOpen, diffOpen, contextOpen]);

  // Bridge for the global ⌘⇧D shortcut: useGlobalShortcuts dispatches
  // a window-level CustomEvent rather than calling into this component
  // directly, so the diff state stays owned here without lifting it
  // into a context. Whichever ChatView is currently mounted (one per
  // active /chat/$sessionId route) is the one that responds, so the
  // shortcut naturally targets the open thread.
  React.useEffect(() => {
    window.addEventListener(TOGGLE_DIFF_EVENT, toggleDiff);
    return () => window.removeEventListener(TOGGLE_DIFF_EVENT, toggleDiff);
  }, [toggleDiff]);

  // Look up the active session first, then fall back to the archived
  // list so the chat view can render read-only history for an archived
  // thread without the caller having to know which table it lives in.
  const session =
    state.sessions.get(sessionId) ??
    state.archivedSessions.find((s) => s.sessionId === sessionId);

  // Re-clamp composer settings whenever the active model changes, so
  // X-High / Adaptive / etc. don't silently linger when the user
  // switches to a model that doesn't support them. Covers all three
  // ways the model field can change:
  //   1. user picks a new model via `ModelSelector`,
  //   2. the Claude SDK emits `model_resolved` on turn 1 (which
  //      replaces `session.model` with a pinned id that has no
  //      catalog entry — so we prefer the cached picked-alias for
  //      the capability lookup, see `lib/model-settings.ts`), and
  //   3. session hydration from sessionStorage after an app restart —
  //      the stored effort/mode may outlive a later model change.
  //
  // Skipped when we can't find a catalog entry at all (neither the
  // picked alias nor `session.model` match). That happens briefly
  // during bootstrap before `state.providers` lands, or on imported
  // sessions whose alias we never cached. In both cases we prefer
  // the user's stored preference over a premature flip — the effect
  // re-runs when `state.providers` updates, so the clamp eventually
  // fires once the catalog is available.
  React.useEffect(() => {
    if (!session?.model) return;
    const pickedModel = readPickedModel(sessionId);
    const { entry } = resolveModelDisplay(
      pickedModel ?? session.model,
      session.provider,
      state.providers,
    );
    if (!entry) return;
    const nextEffort = clampEffortToModel(effort, entry);
    if (nextEffort !== effort) setEffort(nextEffort);
    const nextMode = clampThinkingModeToModel(thinkingMode, entry);
    if (nextMode !== thinkingMode) setThinkingMode(nextMode);
  }, [
    sessionId,
    session?.model,
    session?.provider,
    state.providers,
    effort,
    thinkingMode,
    setEffort,
    setThinkingMode,
  ]);

  // Currently selected model's catalog entry. Same lookup as
  // `chat-toolbar.tsx` — prefer the picked-alias cache over
  // `session.model` because the SDK's `model_resolved` event replaces
  // `session.model` with a pinned id that has no catalog entry on
  // turn 1. Used to gate the per-model "Auto" plan-exit option on
  // `entry.supportsAutoMode` (Claude Agent SDK exposes this as a
  // per-model flag — a Claude provider can list models that don't
  // carry it). Memoized so PermissionPrompt's `supportsAutoMode`
  // prop is referentially stable across renders.
  const modelEntry = React.useMemo(() => {
    if (!session?.model) return undefined;
    const pickedModel = readPickedModel(sessionId);
    return resolveModelDisplay(
      pickedModel ?? session.model,
      session.provider,
      state.providers,
    ).entry;
  }, [sessionId, session?.model, session?.provider, state.providers]);

  // Provider feature flags — drives capability-gated UI (mode
  // selector, effort selector, etc.). Used locally to exclude `auto`
  // from the Shift+Tab cycle when the provider doesn't support a
  // model-classifier permission mode.
  const providerFeatures = useProviderFeatures(session?.provider);
  const excludedModes = React.useMemo<PermissionMode[]>(
    () =>
      providerFeatures.supportsAutoPermissionMode === true ? [] : ["auto"],
    [providerFeatures.supportsAutoPermissionMode],
  );
  const isArchived = React.useMemo(
    () => state.archivedSessions.some((s) => s.sessionId === sessionId),
    [state.archivedSessions, sessionId],
  );
  // Runtime enablement lookup for the session's provider. When the
  // user disables a provider from Settings, existing sessions stay
  // visible (history preserved) but the chat header shows a badge
  // and the composer's send button is forced off. Uses the app-level
  // enabled state, not the SDK's `ProviderStatus.enabled` flag.
  const { isProviderEnabled } = useProviderEnabled();
  const providerDisabled = React.useMemo(() => {
    if (!session) return false;
    return !isProviderEnabled(session.provider);
  }, [session, isProviderEnabled]);

  // Per-session slash-command catalog. Populated by the daemon via
  // `session_command_catalog_updated` on session start / load. The
  // catalog is static for the session's lifetime — to pick up new
  // disk SKILL.md files or a provider CLI upgrade, create a new
  // thread. Nothing in the composer auto-refreshes the catalog.
  const commandCatalog = useSessionCommandCatalog(sessionId);
  const slashCommands = React.useMemo(
    () => mergeCommandsWithCatalog(commandCatalog),
    [commandCatalog],
  );

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
  // Resolve the git root for the parent project — when the project
  // directory is a submodule or linked worktree, the raw path may
  // differ from what git considers the repo root. Worktree
  // operations (create/remove/list) need the resolved root.
  const parentGitRootQuery = useQuery(gitRootQueryOptions(parentProjectPath));
  const parentGitRoot = parentGitRootQuery.data ?? parentProjectPath;
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

      // Cycle to next mode. Always update local state so the toolbar
      // reflects the choice and the next `send_turn` sends it. If a
      // turn is in flight, also push `update_permission_mode` so the
      // in-flight adapter picks up the change immediately — without
      // this, toggling bypass mid-turn would still prompt for every
      // subsequent tool call until the turn ends.
      const newMode = cycleMode(permissionMode, "forward", excludedModes);
      setPermissionMode(newMode);
      if (session?.status === "running") {
        void sendMessage({
          type: "update_permission_mode",
          session_id: sessionId,
          permission_mode: newMode,
        });
      }

      toast({
        description: `Mode: ${MODE_LABELS[newMode]}`,
        duration: 2000,
      });
    };

    window.addEventListener("keydown", handleKeyDown);
    return () => window.removeEventListener("keydown", handleKeyDown);
  }, [session, permissionMode, excludedModes]);

  // Escape interrupts the in-flight turn — but requires a *double* press
  // within 2s to actually fire. A single Esc only "arms" the gesture and
  // shows a toast hint; the second press inside the window does the
  // interrupt. This guards against accidental presses (reaching for Esc
  // to dismiss something else, OS habit, etc.) silently killing a long
  // agent run. Mouse clicks on the working-indicator button and the
  // composer stop button stay single-click — clicking a target is
  // already deliberate. The title-rename Escape handler is scoped to
  // its own input element, so this window-level listener doesn't
  // clobber it when a rename is in progress.
  const escArmedRef = React.useRef(false);
  const escResetTimerRef = React.useRef<number | null>(null);
  const escToastDismissRef = React.useRef<(() => void) | null>(null);

  React.useEffect(() => {
    if (!session) return;

    const handleKeyDown = (event: KeyboardEvent) => {
      if (event.key !== "Escape") return;
      if (session.status !== "running") return;
      event.preventDefault();

      if (escArmedRef.current) {
        // Second press within the window — actually interrupt. Disarm
        // *before* sendMessage so a third press in the same tick can't
        // double-fire interrupt_turn.
        escArmedRef.current = false;
        if (escResetTimerRef.current != null) {
          clearTimeout(escResetTimerRef.current);
          escResetTimerRef.current = null;
        }
        escToastDismissRef.current?.();
        escToastDismissRef.current = null;
        sendMessage({ type: "interrupt_turn", session_id: sessionId }).catch(
          (err) => {
            console.error("Failed to interrupt turn", err);
          },
        );
        return;
      }

      // First press — arm and show the hint. Toast duration matches the
      // arming window so the visible cue and the keyboard handler's
      // internal state expire at the same instant.
      escArmedRef.current = true;
      escToastDismissRef.current = toast({
        description: "Press Esc again to interrupt",
        duration: 2000,
      }).dismiss;
      escResetTimerRef.current = window.setTimeout(() => {
        escArmedRef.current = false;
        escToastDismissRef.current = null;
        escResetTimerRef.current = null;
      }, 2000);
    };

    window.addEventListener("keydown", handleKeyDown);
    return () => {
      window.removeEventListener("keydown", handleKeyDown);
      if (escResetTimerRef.current != null) {
        clearTimeout(escResetTimerRef.current);
        escResetTimerRef.current = null;
      }
      escToastDismissRef.current?.();
      escToastDismissRef.current = null;
      escArmedRef.current = false;
    };
  }, [sessionId, session]);

  // If the turn finishes naturally between the first and second Esc
  // press, the arming state and its toast become misleading ("Press Esc
  // again to interrupt" when there's nothing to interrupt anymore).
  // Reactively disarm whenever status leaves "running".
  const sessionStatus = session?.status;
  React.useEffect(() => {
    if (sessionStatus === "running") return;
    if (escResetTimerRef.current != null) {
      clearTimeout(escResetTimerRef.current);
      escResetTimerRef.current = null;
    }
    escToastDismissRef.current?.();
    escToastDismissRef.current = null;
    escArmedRef.current = false;
  }, [sessionStatus]);

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

    // Scan last turn for an unmatched EnterPlanMode (entered plan but
    // didn't exit). This handles agent-initiated plan mode changes that
    // happened while the user was viewing a different session — the
    // tool_call_completed handler only runs for the active session.
    const lastTurn = data.detail.turns[data.detail.turns.length - 1];
    const tools = lastTurn.toolCalls ?? [];
    let planModeActive = false;
    for (const tc of tools) {
      if (tc.name === "EnterPlanMode" && !tc.error) planModeActive = true;
      if (tc.name === "ExitPlanMode" && !tc.error) planModeActive = false;
    }
    if (planModeActive) {
      setPermissionMode("plan");
      return;
    }

    // Original: restore from turn's permissionMode when no sessionStorage.
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
  //
  // Why this is one big block, not per-field cleanup at the call
  // site: `ChatView` never remounts on session switch (only its
  // `sessionId` prop changes — see the `key={sessionId}` on
  // `<ChatInput>` below for why the composer does remount). So any
  // useState here is, by default, shared across every thread the
  // user visits in this ChatView lifetime. The complete-isolation
  // rule is: if a piece of state was set from a stream event for a
  // specific `session_id`, or reflects a user gesture made while
  // looking at thread A, it must be zeroed before the user sees
  // thread B. Missing even one field means that field bleeds into
  // every subsequent thread the user switches to.
  React.useEffect(() => {
    // Watchdog / optimistic composer bookkeeping.
    setPendingInput(null);
    setLastEventAt(Date.now());
    setStuckSince(null);
    // Running-turn chrome that's driven by stream events gated on
    // the active sessionId — without resetting, thread A's phase
    // label or retry banner keeps rendering over thread B's
    // composer until a new event happens to fire and overwrite it.
    setTurnPhase(undefined);
    setRetryState(null);
    // Ghost text from `prompt_suggested` is the source of the
    // "Tab-complete bleeds across tabs" bug: the composer remounts
    // via `key={sessionId}` but still receives `promptSuggestion`
    // as a prop from here, so thread A's predicted prompt would
    // render as ghost text on thread B and Tab would insert it.
    setPromptSuggestion(null);
    // Open-on-top-of-everything overlay for a persisted attachment.
    // If thread A had the lightbox open, switching to B would keep
    // A's image pinned over B's entire view.
    setPersistedLightboxRef(null);
    // Async "Load older" spinner — belongs to the in-flight request
    // for the thread we're leaving; a new thread click wants a
    // clean slate, and the previous fetch resolves into its own
    // cache entry regardless.
    setLoadingOlder(false);
    // Resync per-thread panel open flags from the module-level maps.
    // ChatView doesn't remount on session switch, so the lazy
    // useState initializers only ran for the very first session —
    // this effect brings the mirror state into sync with the map on
    // every subsequent switch. Fullscreen is intentionally reset
    // (it's a momentary intent, not a thread preference).
    const nextDiffOpen = sessionDiffOpen.get(sessionId) ?? false;
    setDiffOpenState(nextDiffOpen);
    setContextOpenState(sessionContextOpen.get(sessionId) ?? false);
    // Code-view panel: same per-thread restore the diff/context flags
    // get above. Without this, opening the code view on thread A and
    // switching to thread B would leave B mounted with A's panel
    // visible, since ChatView doesn't remount on sessionId changes.
    setCodeViewOpenState(sessionTransient.getCodeViewOpen(sessionId));
    setCodeViewFullscreen(false);
    setDiffFullscreen(false);
    setContextFullscreen(false);
    // Diff subscription lifecycle on thread switch. Previous
    // revision killed the subscription here ("don't run git diff
    // subscriptions the user never asked for") but that left the
    // streaming hook's committed `diffs` pinned to thread A's
    // numbers — visible on thread B's action-bar badge until the
    // user re-opened the panel. The hook now clears state on
    // path change (see useStreamedGitDiffSummary), so a stale
    // badge can't bleed across threads even if we do nothing here.
    //
    // When the user has ever opened the diff panel in this
    // ChatView OR the target thread remembers the panel as open,
    // kick off a fresh subscription for the new project so both
    // the badge and (if it's open) the panel show current numbers
    // without requiring a hover. `refreshDiffs({force:true})`
    // bypasses the 400ms debounce since this is a one-shot gesture
    // tied to the thread-switch intent.
    if (diffPanelEverOpenedRef.current || nextDiffOpen) {
      activateDiffSubscription();
      refreshDiffs({ force: true });
    }
  }, [sessionId, activateDiffSubscription, refreshDiffs]);

  // Per-view stream subscription. Routes through the store's single
  // `connectStream` channel via `addServerMessageListener` — there is
  // exactly one Tauri channel open for the whole app, regardless of
  // how many ChatView instances mount. The hook owns all the
  // routing/dispatch logic that used to live inline here; chat-view
  // just hands it the local state setters and refs.
  useSessionStreamSubscription({
    sessionId,
    sessionIdRef,
    // Worktree threads pass their own worktree path here so
    // reindex-on-turn-completion targets the right fff-search
    // FilePicker (each worktree has its own index, keyed by
    // canonicalised path on the Rust side).
    projectPath,
    setPendingInput,
    setLastEventAt,
    setStuckSince,
    setTurnPhase,
    setRetryState,
    setPromptSuggestion,
    setPermissionMode,
    activateDiffSubscription,
    refreshDiffs,
  });

  // --- Draft persistence callbacks ---
  const handleDraftChange = React.useCallback(
    (draft: string) => {
      if (draft.length > 0) {
        sessionDrafts.set(sessionId, draft);
      } else {
        sessionDrafts.delete(sessionId);
      }
    },
    [sessionId],
  );

  // --- Queue persistence callbacks ---
  const handleQueueChange = React.useCallback(
    (queue: QueuedMessage[]) => {
      if (queue.length > 0) {
        sessionQueues.set(sessionId, queue);
      } else {
        sessionQueues.delete(sessionId);
      }
    },
    [sessionId],
  );

  async function handleSend(input: string, images: AttachedImage[] = []) {
    // Clear the persisted draft on send — the message is gone from
    // the composer and we don't want it to reappear if the user
    // navigates away and back before the component remounts.
    sessionDrafts.delete(sessionId);
    sessionQueues.delete(sessionId);
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
    const resolved = resolveCommand(input, slashCommands, session?.provider);
    if (resolved) {
      if (resolved.kind === "unknown") {
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
      if (resolved.kind === "core") {
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
      // resolved.kind === "skill" — rewrite to the canonical invocation
      // form (e.g. "/compact args" or "$skill args" for Codex) and fall
      // through to the normal send_turn path. The provider itself
      // interprets the invocation; we just forward it verbatim.
      input = resolved.args
        ? `${resolved.invocation} ${resolved.args}`
        : resolved.invocation;
    }

    // --- Normal message flow ---

    // First-turn auto-title. Mirrors what the SDK's orchestration layer
    // used to do before display metadata moved app-side; see the zenui
    // reference in rs-agent-sdk/apps/zenui/frontend/src/state/appStore.ts.
    // Truncation rule lives in `@/lib/auto-title` so MCP-spawned
    // threads (which never reach this handler) can use the same rule
    // from the app-store's `turn_started` side-effect handler.
    const existingDisplay = state.sessionDisplay.get(sessionId);
    if (
      session &&
      session.turnCount === 0 &&
      !existingDisplay?.title
    ) {
      const autoTitle = deriveAutoTitle(input);
      if (autoTitle.length > 0) {
        void renameSession(sessionId, autoTitle);
      }
    }

    // Optimistic: show the user's message immediately, then await the
    // round-trip. turn_started will clear this and replace it with the
    // real turn from the daemon.
    setPendingInput(input);
    // Signal MessageList to force-scroll to the bottom. handleSend is the
    // single funnel for actually-dispatched user messages (queued-while-
    // running submissions don't reach here, and they don't show up in the
    // list either, so they correctly don't trigger a scroll).
    setUserSendTick((n) => n + 1);
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
        thinking_mode: thinkingMode,
      });
    } catch (err) {
      setPendingInput(null);
      throw err;
    }
  }

  async function handleInterrupt() {
    await sendMessage({ type: "interrupt_turn", session_id: sessionId });
  }

  /** Atomic steer: cooperatively interrupt the current turn (if any)
   *  AND dispatch the provided input as the next turn in a single
   *  daemon-side operation. The daemon serialises interrupt →
   *  wait-for-finalize → send so the frontend can't race itself
   *  against the bridge's `turnInProgress` guard. Single RPC, no
   *  status-transition dance on the client. */
  async function handleSteer(input: string, images: AttachedImage[] = []) {
    // Clear drafts / queues the same way handleSend does — the
    // steered message counts as an actually-dispatched user turn.
    sessionDrafts.delete(sessionId);
    // Note: we intentionally do NOT delete sessionQueues here —
    // only the plucked message is leaving; the rest of the queue
    // survives and will drain on the resulting turn's completion.
    // Optimistic echo so the composer collapses instantly; the
    // real `turn_started` event from the daemon replaces it.
    setPendingInput(input);
    setUserSendTick((n) => n + 1);
    try {
      await sendMessage({
        type: "steer_turn",
        session_id: sessionId,
        input,
        images: images.map((img) => ({
          media_type: img.mediaType,
          data_base64: img.dataBase64,
          name: img.name,
        })),
        permission_mode: permissionMode,
        reasoning_effort: effort,
        thinking_mode: thinkingMode,
      });
    } catch (err) {
      setPendingInput(null);
      throw err;
    }
  }

  async function handlePermissionDecision(
    decision: PermissionDecision,
    modeOverride?: PermissionMode,
    feedback?: string,
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
      // `reason` rides along with a deny to surface user feedback to
      // the model as the synthetic tool_result message. Plan-exit
      // "Send feedback" is the only caller that sets it today.
      ...(feedback ? { reason: feedback } : {}),
    });
    if (modeOverride) {
      // Mirror the chosen mode into local state so the toolbar dropdown
      // and the next send_turn pick it up. The Claude SDK side already
      // applies the mode via the bundled updatedPermissions, so this is
      // purely a UI sync — no second daemon round-trip.
      setPermissionMode(modeOverride);
    }
  }

  // (Strict-plan-mode auto-deny moved to <RoutePromptOverlay />; see
  // the comment near the deleted `strictPlanMode` state above.)

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

  // Progress badge for the Agent Context button in the header — ticks
  // live off the latest main-agent TodoWrite anywhere in the session,
  // prefers the running turn when one is in flight.
  const todoProgress = React.useMemo(() => {
    const found = findLatestMainTodoWrite(turns, runningTurn);
    const parsed = parseTodoProgress(found);
    if (!parsed) return null;
    return { completed: parsed.completed, total: parsed.total };
  }, [turns, runningTurn]);

  const handleToggleContext = React.useCallback(() => {
    setContextOpen((v) => {
      const next = !v;
      if (next) {
        setDiffOpen(false);
        setDiffFullscreen(false);
        setCodeViewOpen(false);
        setCodeViewFullscreen(false);
      } else {
        setContextFullscreen(false);
      }
      return next;
    });
  }, [setCodeViewOpen]);

  // Bridge for the global ⌘⇧K shortcut: same indirection as
  // TOGGLE_DIFF_EVENT above so the open/closed state stays owned by
  // this component (which already does the diff↔context mutual
  // exclusion). The active ChatView is the listener; popouts host
  // their own ChatView so the popout's ⌘⇧K toggles its own panel.
  React.useEffect(() => {
    window.addEventListener(TOGGLE_CONTEXT_EVENT, handleToggleContext);
    return () =>
      window.removeEventListener(TOGGLE_CONTEXT_EVENT, handleToggleContext);
  }, [handleToggleContext]);

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

  // Mode changes update local state (picked up by next `send_turn`)
  // AND push `update_permission_mode` when a turn is running so the
  // in-flight adapter honors the new mode for tools still to be
  // called in the current turn. Plan-exit mode switches go through
  // `handlePermissionDecision` / `permission_mode_override` — that
  // path bundles the mode change atomically with a tool approval via
  // the SDK's `updatedPermissions: [{setMode}]` mechanism, which is
  // cheaper than a separate RPC but only applies when there's a
  // pending permission to approve.
  const handlePermissionModeChange = React.useCallback(
    (mode: PermissionMode) => {
      setPermissionMode(mode);
      if (session?.status === "running") {
        void sendMessage({
          type: "update_permission_mode",
          session_id: sessionId,
          permission_mode: mode,
        });
      }
    },
    [session?.status, sessionId],
  );

  const toolbar = session ? (
    <ChatToolbar
      sessionId={sessionId}
      provider={session.provider}
      currentModel={session.model}
      effort={effort}
      onEffortChange={setEffort}
      thinkingMode={thinkingMode}
      onThinkingModeChange={setThinkingMode}
      permissionMode={permissionMode}
      onPermissionModeChange={handlePermissionModeChange}
    />
  ) : null;

  return (
    <div className="flex h-svh min-w-0 flex-col overflow-hidden">
      <header
        data-tauri-drag-region
        className="flex h-9 shrink-0 items-center gap-1 border-b border-border px-2 text-sm"
      >
        {/* macOS traffic-light spacer. Tagged as a drag region too so
            the cleared area still drags the window. titleBarStyle:
            Overlay overlays the buttons on top of this 64px slot. Only
            rendered when the chat header is actually leftmost (sidebar
            collapsed, or we're in a popout where the sidebar isn't
            mounted at all). When the sidebar is expanded the lights
            sit over SidebarHeader's spacer instead. */}
        {showMacTrafficSpacer && (
          <div className="w-16 shrink-0" data-tauri-drag-region />
        )}
        {/* SidebarTrigger has no sidebar to toggle when rendered in
            a thread popout (the stripped PopoutShell in router.tsx
            doesn't mount AppSidebar), so hide it there. */}
        {!isPopoutWindow() && <SidebarTrigger />}
        <div
          // Drag region on the inner wrapper too so the small gaps
          // between title / branch / badge are also draggable —
          // Tauri's drag.js reads `e.target` directly, no ancestor
          // walk, so the parent <header>'s attribute alone isn't
          // enough to drag from gaps inside this child.
          data-tauri-drag-region
          className="flex min-w-0 flex-1 items-center gap-2"
        >
          {/* Read-only title in the chat header. Renaming lives in
              the sidebar's thread row instead. With the drag-region
              attribute Tauri's drag.js handles both single click +
              drag (move window) and double-click (toggle maximize)
              natively — no JS handlers needed here. */}
          <span
            data-tauri-drag-region
            className="min-w-0 flex-1 truncate font-medium select-none"
          >
            {title}
          </span>
          {gitBranch && projectPath && session && parentProjectId && parentGitRoot && (
            <BranchSwitcher
              projectPath={projectPath}
              currentBranch={gitBranch}
              parentProjectId={parentProjectId}
              parentProjectPath={parentGitRoot}
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
        <div
          className="ml-auto flex items-center gap-2"
          data-tauri-drag-region={false}
        >
          <HeaderActions
            sessionId={sessionId}
            projectPath={projectPath}
            diffs={diffs}
            diffOpen={diffOpen}
            contextOpen={contextOpen}
            todoProgress={todoProgress}
            turns={turns}
            onToggleContext={handleToggleContext}
            // First interaction with the diff button of any kind
            // activates the streamed-diff subscription and bumps the
            // refresh tick (does not blank the badge — the streamed
            // hook keeps the previous diffs committed until the new
            // subscription's Phase 1 lands). Opening also closes the
            // mutually-exclusive agent-context pane and drops its
            // fullscreen so the split-right slot is clean. Closing
            // drops diff fullscreen for the same reason. All of that
            // lives in `toggleDiff` so the global ⌘⇧D shortcut runs
            // the exact same path.
            onToggleDiff={toggleDiff}
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
            // `min-h-0` is required so this column honours the
            // split-row's height bound — without it, flex children's
            // intrinsic content height ignores the parent's `min-h-0`,
            // and the composer below this column punches out of
            // ChatView's `overflow-hidden` clip in short windows.
            "flex min-h-0 min-w-0 flex-col",
            diffFullscreen || contextFullscreen || codeViewFullscreen
              ? "hidden"
              : "flex-1",
          )}
        >
          <SessionProvider
            value={{
              sessionId,
              provider: sessionQuery.data?.detail.summary.provider,
              model: sessionQuery.data?.detail.summary.model,
            }}
          >
            <MessageList
              sessionId={sessionId}
              turns={turns}
              loading={loading}
              pendingInput={pendingInput}
              userSendTick={userSendTick}
              hiddenOlderCount={hiddenOlderCount}
              loadingOlder={loadingOlder}
              onLoadOlder={handleLoadOlder}
              onOpenAttachment={handleOpenPersistedAttachment}
              providerKind={sessionQuery.data?.detail.summary.provider}
              sessionModel={sessionQuery.data?.detail.summary.model}
            />
          </SessionProvider>

          {isRunning && session && runningTurn && (
            <WorkingIndicator
              turnStartedAt={new Date(runningTurn.createdAt).getTime()}
              lastEventAt={lastEventAt}
              tone={toneForMode(permissionMode)}
              phase={turnPhase}
              onInterrupt={handleInterrupt}
            />
          )}

          {isRunning && retryState && <ApiRetryBanner state={retryState} />}

          {/* "Session may be stuck" warning — commented out for now.
              The watchdog still runs and sets `stuckSince`, but we don't
              surface the banner until we're confident about the
              heuristic's false-positive rate.
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
          */}
        </div>

        {diffOpen && (
          <>
            {!diffFullscreen && (
              <PanelDragHandle
                containerRef={splitContainerRef}
                width={diffWidth}
                onResize={setDiffWidth}
                storageKey={DIFF_WIDTH_KEY}
                minWidth={DIFF_MIN_WIDTH}
                ariaLabel="Resize diff panel"
              />
            )}
            <aside
              className={cn(
                // See comment on the codeView aside below: `min-w-0`
                // + `overflow-hidden` let the panel honour its saved
                // pixel width when the window is wide enough but
                // yield gracefully when it isn't, instead of
                // freezing at content's intrinsic width.
                "min-w-0 overflow-hidden border-l border-border bg-background",
                diffFullscreen ? "flex-1" : "",
              )}
              style={diffFullscreen ? undefined : { width: diffWidth }}
            >
              <DiffPanel
                projectPath={projectPath}
                sessionId={sessionId}
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

        {contextOpen && (
          <>
            {!contextFullscreen && (
              <PanelDragHandle
                containerRef={splitContainerRef}
                width={contextWidth}
                onResize={setContextWidth}
                storageKey={CONTEXT_WIDTH_KEY}
                minWidth={CONTEXT_MIN_WIDTH}
                ariaLabel="Resize agent context panel"
              />
            )}
            <aside
              className={cn(
                // See comment on the codeView aside below.
                "min-w-0 overflow-hidden border-l border-border bg-background",
                contextFullscreen ? "flex-1" : "",
              )}
              style={contextFullscreen ? undefined : { width: contextWidth }}
            >
              <AgentContextPanel
                turns={turns}
                runningTurn={runningTurn}
                onClose={() => {
                  setContextOpen(false);
                  setContextFullscreen(false);
                }}
                isFullscreen={contextFullscreen}
                onToggleFullscreen={() => setContextFullscreen((v) => !v)}
              />
            </aside>
          </>
        )}

        {codeViewOpen && (
          <>
            {!codeViewFullscreen && (
              <PanelDragHandle
                containerRef={splitContainerRef}
                width={codeViewWidth}
                onResize={setCodeViewWidth}
                storageKey={CODE_VIEW_WIDTH_KEY}
                minWidth={CODE_VIEW_MIN_WIDTH}
                ariaLabel="Resize code view panel"
              />
            )}
            <aside
              className={cn(
                // `min-w-0` + `overflow-hidden` are load-bearing:
                // without them, the default `min-width: auto` of a
                // flex item resolves to the content's intrinsic
                // width (CodeMirror's longest source line), so the
                // aside refuses to shrink as the window narrows and
                // the editor visibly clips off the right edge of the
                // viewport instead of soft-wrapping. With `min-w-0`
                // the aside honours the saved `width` as a *preferred*
                // size (flex-basis) but still yields when the parent
                // doesn't have room.
                "min-w-0 overflow-hidden border-l border-border bg-background",
                codeViewFullscreen ? "flex-1" : "",
              )}
              style={codeViewFullscreen ? undefined : { width: codeViewWidth }}
            >
              <CodeView
                sessionId={sessionId}
                embedded
                onClose={() => {
                  setCodeViewOpen(false);
                  setCodeViewFullscreen(false);
                }}
                isFullscreen={codeViewFullscreen}
                onToggleFullscreen={() => setCodeViewFullscreen((v) => !v)}
                searchRequest={codeViewSearchRequest}
              />
            </aside>
          </>
        )}
      </div>
      {/* Per-session prompts + composer — pulled out of the chat
          column so they stay visible regardless of which panel is
          taking horizontal space (half-split diff / context) or
          which one has gone fullscreen (chat column hidden). The
          previous arrangement parented them inside the chat column,
          which meant any layout state that hid or shrunk that column
          could swallow the prompts and leave the daemon blocked
          waiting for an answer the user couldn't see. /code routes
          are handled by <RoutePromptOverlay /> in the root layout. */}
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
          // Per-model gate for the plan-exit "Auto" option. Strictly
          // tighter than the toolbar ModeSelector's provider-level
          // gate — see comment on `modelEntry` above for why.
          supportsAutoMode={modelEntry?.supportsAutoMode === true}
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
        // internal state (pendingSend queue flag, slash-command
        // popup) resets cleanly. Draft text is now preserved
        // via initialValue / onDraftChange so the user's
        // in-progress message survives tab switches.
        key={sessionId}
        onSend={handleSend}
        onInterrupt={handleInterrupt}
        onSteer={handleSteer}
        sessionStatus={session?.status}
        disabled={loading}
        providerDisabled={providerDisabled}
        archived={isArchived || worktreeFolderMissing}
        toolbar={toolbar}
        commands={slashCommands}
        provider={session?.provider}
        initialValue={sessionDrafts.get(sessionId) ?? ""}
        onDraftChange={handleDraftChange}
        initialQueue={sessionQueues.get(sessionId)}
        onQueueChange={handleQueueChange}
        permissionMode={permissionMode}
        promptSuggestion={promptSuggestion}
        onPromptSuggestionDismissed={() => setPromptSuggestion(null)}
        projectPath={projectPath}
        sessionId={sessionId}
      />
      {persistedLightboxRef && (
        <ImageLightbox
          source={{ kind: "persisted", ref: persistedLightboxRef }}
          onClose={() => setPersistedLightboxRef(null)}
        />
      )}
    </div>
  );
}
