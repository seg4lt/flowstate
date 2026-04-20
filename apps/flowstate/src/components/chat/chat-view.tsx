import * as React from "react";
import { useNavigate } from "@tanstack/react-router";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { SidebarTrigger } from "@/components/ui/sidebar";
import { cn } from "@/lib/utils";
import { useApp, useSessionCommandCatalog } from "@/stores/app-store";
import type {
  AttachedImage,
  AttachmentRef,
  ContentBlock,
  PermissionDecision,
  PermissionMode,
  ReasoningEffort,
  RuntimeEvent,
  ThinkingMode,
  TurnRecord,
  UserInputAnswer,
  UserInputQuestion,
} from "@/lib/types";
import { connectStream, sendMessage } from "@/lib/api";
import {
  gitBranchQueryOptions,
  gitRootQueryOptions,
  loadFullSession,
  pathExistsQueryOptions,
  sessionQueryKey,
  sessionQueryOptions,
  type SessionPage,
} from "@/lib/queries";
import { useStreamedGitDiffSummary } from "@/lib/git-diff-stream";
import { cycleMode, MODE_LABELS } from "@/lib/mode-cycling";
import { toneForMode } from "@/lib/mode-tone";
import {
  readDefaultEffort,
  readDefaultPermissionMode,
  readStrictPlanMode,
} from "@/lib/defaults-settings";
import {
  mergeCommandsWithCatalog,
  resolveCommand,
  type SlashCommandContext,
} from "@/lib/slash-commands";
import { PLAN_MODE_MUTATING_TOOLS } from "@/lib/tool-policy";
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
import { SessionSettingsDialog } from "./session-settings-dialog";
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

// Per-session draft text. Module-level so it survives ChatView
// re-renders and ChatInput remounts (keyed by sessionId). Cleared
// on send so completed messages don't linger as stale drafts.
const sessionDrafts = new Map<string, string>();

// Per-session message queue. Module-level so it survives ChatInput
// remounts (keyed by sessionId). Same pattern as sessionDrafts —
// preserves queued messages when the user switches threads mid-turn.
const sessionQueues = new Map<string, QueuedMessage[]>();

// Per-session diff / context panel open flags. Module-level so they
// survive thread switches (ChatView does NOT remount on sessionId
// change — sessionId is a prop, not a key). Lost on reload, which is
// intentional: "did I leave the diff open" is transient UI state, not
// a persisted preference. Width / style live in localStorage below
// because those ARE preferences. Fullscreen is deliberately NOT
// per-thread (plain useState) — it's a momentary intent.
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

// Merge-or-append for compaction blocks. Runtime-core already pairs
// up `compact_boundary` + `compact_summary` into one block, but the
// frontend receives incremental updates as either event arrives. If
// the last block is a Compact whose payload is compatible (same
// trigger, no newer-than-stream regressions) we fold the fresh
// fields in; otherwise we append a new block. Two compactions in
// one turn (rare, but possible on very long turns) show as two
// separate blocks.
function applyCompactUpdate(
  blocks: ContentBlock[] | undefined,
  update: {
    trigger: "auto" | "manual";
    preTokens?: number;
    postTokens?: number;
    durationMs?: number;
    summary?: string;
  },
): ContentBlock[] {
  const list = blocks ?? [];
  const last = list[list.length - 1];
  if (last && last.kind === "compact") {
    const merged: ContentBlock = {
      kind: "compact",
      trigger: update.trigger,
      preTokens: update.preTokens ?? last.preTokens,
      postTokens: update.postTokens ?? last.postTokens,
      durationMs: update.durationMs ?? last.durationMs,
      summary: update.summary ?? last.summary,
    };
    return [...list.slice(0, -1), merged];
  }
  return [
    ...list,
    {
      kind: "compact",
      trigger: update.trigger,
      preTokens: update.preTokens,
      postTokens: update.postTokens,
      durationMs: update.durationMs,
      summary: update.summary,
    },
  ];
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
    case "compact_updated":
      return prev.map((t) =>
        t.turnId === event.turn_id
          ? {
              ...t,
              blocks: applyCompactUpdate(t.blocks, {
                trigger: event.trigger,
                preTokens: event.pre_tokens,
                postTokens: event.post_tokens,
                durationMs: event.duration_ms,
                summary: event.summary,
              }),
            }
          : t,
      );
    case "memory_recalled":
      return prev.map((t) =>
        t.turnId === event.turn_id
          ? {
              ...t,
              blocks: [
                ...(t.blocks ?? []),
                {
                  kind: "memory_recall",
                  mode: event.mode,
                  memories: event.memories,
                },
              ],
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
    // Per-tool heartbeat from a provider that opted into
    // ProviderFeatures.toolProgress (Claude SDK today). We just
    // stamp lastProgressAt on the matching tool call; the
    // tool-call card watches that field against wall time and
    // shows a "no progress · Ns" pip when it goes stale, while
    // the stuck banner stays out of the way for tools that are
    // still ticking. Unknown call_ids are silently ignored —
    // usually means the heartbeat raced ahead of
    // tool_call_started by a frame.
    case "tool_progress":
      return prev.map((t) => {
        if (t.turnId !== event.turn_id || !t.toolCalls) return t;
        return {
          ...t,
          toolCalls: t.toolCalls.map((tc) =>
            tc.callId === event.call_id
              ? { ...tc, lastProgressAt: event.occurred_at }
              : tc,
          ),
        };
      });
    // Subagent lifecycle. Previously these only landed via the
    // whole-turn refetch triggered by turn_completed, so the
    // subagent box stayed empty during long-running dispatches.
    // Handling them here lets the UI stream the subagent's state
    // (including its per-agent model, once observed) live.
    case "subagent_started":
      return prev.map((t) =>
        t.turnId === event.turn_id
          ? {
              ...t,
              subagents: [
                ...(t.subagents ?? []),
                {
                  agentId: event.agent_id,
                  parentCallId: event.parent_call_id,
                  agentType: event.agent_type,
                  prompt: event.prompt,
                  model: event.model,
                  events: [],
                  status: "running" as const,
                },
              ],
            }
          : t,
      );
    case "subagent_event":
      return prev.map((t) => {
        if (t.turnId !== event.turn_id || !t.subagents) return t;
        return {
          ...t,
          subagents: t.subagents.map((s) =>
            s.agentId === event.agent_id
              ? { ...s, events: [...s.events, event.event] }
              : s,
          ),
        };
      });
    case "subagent_completed":
      return prev.map((t) => {
        if (t.turnId !== event.turn_id || !t.subagents) return t;
        return {
          ...t,
          subagents: t.subagents.map((s) =>
            s.agentId === event.agent_id
              ? {
                  ...s,
                  output: event.output,
                  error: event.error,
                  status: event.error
                    ? ("failed" as const)
                    : ("completed" as const),
                }
              : s,
          ),
        };
      });
    case "subagent_model_observed":
      return prev.map((t) => {
        if (t.turnId !== event.turn_id || !t.subagents) return t;
        return {
          ...t,
          subagents: t.subagents.map((s) =>
            s.agentId === event.agent_id ? { ...s, model: event.model } : s,
          ),
        };
      });
    // Incremental usage snapshots land on the in-flight turn so the
    // ContextDisplay popover updates as each API call in the turn's
    // tool loop completes. Without this, `turn.usage` only gets set
    // on `turn_completed` — on an 11-minute turn that means 11
    // minutes of a frozen numerator. See provider-claude-sdk bridge
    // which now emits `turn_usage` per assistant message carrying
    // the LATEST call's input/cache (not the aggregated sum that
    // inflated the display past the window).
    case "turn_usage_updated":
      return prev.map((t) =>
        t.turnId === event.turn_id ? { ...t, usage: event.usage } : t,
      );
    default:
      return prev;
  }
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

  // Strict Plan Mode preference — opt-in frontend policy that auto-
  // denies any mutating-tool permission request while the session is
  // in plan mode (see `PLAN_MODE_MUTATING_TOOLS` + the enforcement
  // useEffect below). We refresh on window focus so flipping the
  // toggle in Settings takes effect without a reload.
  const [strictPlanMode, setStrictPlanMode] = React.useState(false);
  React.useEffect(() => {
    let cancelled = false;
    const refresh = () => {
      readStrictPlanMode().then((saved) => {
        if (!cancelled) setStrictPlanMode(saved);
      });
    };
    refresh();
    window.addEventListener("focus", refresh);
    return () => {
      cancelled = true;
      window.removeEventListener("focus", refresh);
    };
  }, []);

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
  // Open-state for the per-session settings dialog (gear icon in
  // header). Local to chat-view because dialog content reads from
  // the React Query cache that lives here.
  const [sessionSettingsOpen, setSessionSettingsOpen] =
    React.useState(false);

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

  // Look up the active session first, then fall back to the archived
  // list so the chat view can render read-only history for an archived
  // thread without the caller having to know which table it lives in.
  const session =
    state.sessions.get(sessionId) ??
    state.archivedSessions.find((s) => s.sessionId === sessionId);

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
    // Inline title editor — if the user had opened the rename input
    // on A's header and switched away, B's header would mount with
    // an open editor bound to A's draft.
    setEditingTitle(false);
    // Async "Load older" spinner — belongs to the in-flight request
    // for the thread we're leaving; a new thread click wants a
    // clean slate, and the previous fetch resolves into its own
    // cache entry regardless.
    setLoadingOlder(false);
    // Per-session settings dialog (gear icon). Modal is tied to the
    // thread whose settings are being edited; carrying it open into
    // another thread would show the wrong session's data.
    setSessionSettingsOpen(false);
    // Diff subscription gate. Opening the diff on thread A latched
    // the hook's `enabled` and the "ever opened" ref to true — both
    // are per-ChatView lifetime flags, but ChatView's lifetime
    // spans every thread the user visits. Reset so thread B
    // doesn't run `git diff` subscriptions the user never asked
    // for.
    setDiffSubscriptionActive(false);
    diffPanelEverOpenedRef.current = false;
    // Resync per-thread panel open flags from the module-level maps.
    // ChatView doesn't remount on session switch, so the lazy
    // useState initializers only ran for the very first session —
    // this effect brings the mirror state into sync with the map on
    // every subsequent switch. Fullscreen is intentionally reset
    // (it's a momentary intent, not a thread preference).
    setDiffOpenState(sessionDiffOpen.get(sessionId) ?? false);
    setContextOpenState(sessionContextOpen.get(sessionId) ?? false);
    setDiffFullscreen(false);
    setContextFullscreen(false);
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
          setTurnPhase(undefined);
          setRetryState(null);
          // Clear any stale suggestion from the previous turn —
          // the new turn will emit its own `prompt_suggested`
          // if the SDK has a prediction.
          setPromptSuggestion(null);
          break;

        case "turn_completed":
          setPendingInput(null);
          setTurnPhase(undefined);
          setRetryState(null);
          // Every completed turn activates the diff subscription
          // (idempotent after the first call) and restarts it so
          // the badge reflects what this turn left on disk. The
          // git work runs entirely on the Rust side via Tauri IPC
          // — non-blocking for the UI.
          activateDiffSubscription();
          refreshDiffs();
          break;

        case "content_delta":
          // First token of the turn clears any in-flight retry
          // banner — if the provider was retrying and the model
          // started responding, the retry succeeded. Always
          // dispatch: React short-circuits a same-value set, so
          // we don't need to gate on the current retryState.
          setRetryState(null);
          break;

        case "turn_status_changed":
          setTurnPhase(event.phase);
          break;

        case "turn_retrying":
          setRetryState({
            turnId: event.turn_id,
            attempt: event.attempt,
            maxRetries: event.max_retries,
            retryDelayMs: event.retry_delay_ms,
            errorStatus: event.error_status,
            error: event.error,
            startedAt: Date.now(),
          });
          break;

        case "prompt_suggested":
          // Latest prediction wins — the SDK may emit several over
          // the life of a turn and we only show the freshest.
          setPromptSuggestion(event.suggestion);
          break;

        case "tool_call_completed": {
          // Detect auto-approved EnterPlanMode completing successfully.
          // When it goes through the permission prompt, PlanEnterPrompt
          // already sets the mode via modeOverride. This catches the
          // bypass/allow-always case where no permission_requested fires.
          if (!event.error) {
            const cached = queryClient.getQueryData<SessionPage>(
              sessionQueryKey(sessionIdRef.current),
            );
            if (cached) {
              const turn = cached.detail.turns.find(
                (t) => t.turnId === event.turn_id,
              );
              const tc = turn?.toolCalls?.find(
                (c) => c.callId === event.call_id,
              );
              if (tc?.name === "EnterPlanMode" && !tc.parentCallId) {
                setPermissionMode("plan");
                toast({
                  description: "Agent switched to Plan mode",
                  duration: 3000,
                });
              }
            }
          }
          break;
        }

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

  // Strict Plan Mode enforcement. When enabled AND the session is in
  // plan mode, any pending permission request for a mutating tool
  // (Bash / Write / Edit / NotebookEdit) is auto-denied before the
  // PermissionPrompt UI has a chance to render. This prevents an
  // accidental Allow click from exiting plan mode mid-investigation.
  //
  // Provider-agnostic: operates on the queue any adapter feeds into
  // `pendingPermissions`, so every provider gets the behaviour for
  // free. The `autoDeniedRef` guards against double-answers during
  // the dispatch → sendMessage round-trip (the queue head can still
  // be the same request_id on the next render tick until the
  // `consume_pending_permission` dispatch settles).
  const autoDeniedRef = React.useRef<Set<string>>(new Set());
  React.useEffect(() => {
    if (!strictPlanMode) return;
    if (permissionMode !== "plan") return;
    const head = pendingPermissions[0];
    if (!head) return;
    if (!PLAN_MODE_MUTATING_TOOLS.has(head.toolName)) return;
    if (autoDeniedRef.current.has(head.requestId)) return;
    autoDeniedRef.current.add(head.requestId);
    void handlePermissionDecision("deny");
  }, [
    strictPlanMode,
    permissionMode,
    pendingPermissions,
    // handlePermissionDecision is defined in component scope and
    // closes over dispatch/sessionId; not memoized, but stable
    // across renders for our purposes (the autoDeniedRef guard
    // prevents re-triggering anyway).
  ]);

  // Prune `autoDeniedRef` of request ids no longer in the queue so
  // it can't grow unbounded over a long session.
  React.useEffect(() => {
    const live = new Set(pendingPermissions.map((p) => p.requestId));
    for (const id of autoDeniedRef.current) {
      if (!live.has(id)) autoDeniedRef.current.delete(id);
    }
  }, [pendingPermissions]);

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
      } else {
        setContextFullscreen(false);
      }
      return next;
    });
  }, []);

  // Is there at least one tool call on the running turn still waiting
  // for its completion event? That's the precondition for the
  // stuck-watchdog: we don't care about ordinary model thinking
  // latency, only about cases where a tool is visibly in "pending"
  // and nothing is moving.
  const hasPendingToolCall = React.useMemo(() => {
    if (!runningTurn) return false;
    return (runningTurn.toolCalls ?? []).some((tc) => tc.status === "pending");
  }, [runningTurn]);

  // Per-session settings affordance gate. Reads the current
  // provider's feature flags; the gear button (and dialog) only
  // surface when at least one settable field is supported. As
  // future per-session fields land they should OR-in here.
  const sessionFeatures = useProviderFeatures(
    sessionQuery.data?.detail.summary.provider,
  );
  const hasSessionSettings = !!sessionFeatures.compactCustomInstructions;

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
        </div>
        <div className="ml-auto flex items-center gap-2">
          <HeaderActions
            sessionId={sessionId}
            projectPath={projectPath}
            diffs={diffs}
            diffOpen={diffOpen}
            contextOpen={contextOpen}
            todoProgress={todoProgress}
            onToggleContext={handleToggleContext}
            // Only show the gear when the current provider has at
            // least one per-session-settable feature. Today that's
            // gated on `compact_custom_instructions`; as more
            // session-scoped fields land, OR them in here.
            onOpenSessionSettings={
              hasSessionSettings
                ? () => setSessionSettingsOpen(true)
                : undefined
            }
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
                  // Mutual exclusion with the agent-context pane:
                  // opening diff closes context and drops its
                  // fullscreen so the split-right slot is clean.
                  setContextOpen(false);
                  setContextFullscreen(false);
                } else {
                  setDiffFullscreen(false);
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
            diffFullscreen || contextFullscreen ? "hidden" : "flex-1",
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
          />
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
                "border-l border-border bg-background",
                contextFullscreen ? "flex-1" : "shrink-0",
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
      </div>
      {persistedLightboxRef && (
        <ImageLightbox
          source={{ kind: "persisted", ref: persistedLightboxRef }}
          onClose={() => setPersistedLightboxRef(null)}
        />
      )}
      {hasSessionSettings && sessionQuery.data?.detail.summary.provider && (
        <SessionSettingsDialog
          open={sessionSettingsOpen}
          onOpenChange={setSessionSettingsOpen}
          sessionId={sessionId}
          provider={sessionQuery.data.detail.summary.provider}
          session={sessionQuery.data?.detail}
        />
      )}
    </div>
  );
}
