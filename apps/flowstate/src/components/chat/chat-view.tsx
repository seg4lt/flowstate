import * as React from "react";
import { useNavigate } from "@tanstack/react-router";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { SidebarTrigger } from "@/components/ui/sidebar";
import { cn } from "@/lib/utils";
import { useApp, useSessionCommandCatalog } from "@/stores/app-store";
import type {
  AttachedImage,
  AttachmentRef,
  PermissionDecision,
  PermissionMode,
  ReasoningEffort,
  RetryState,
  TurnPhase,
  TurnRecord,
  UserInputAnswer,
  UserInputQuestion,
} from "@/lib/types";
import { sendMessage } from "@/lib/api";
import {
  gitBranchQueryOptions,
  gitRootQueryOptions,
  loadFullSession,
  pathExistsQueryOptions,
  sessionQueryKey,
  sessionQueryOptions,
} from "@/lib/queries";
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
import { useModeCycleShortcut } from "@/hooks/useModeCycleShortcut";
import { useDoubleEscInterrupt } from "@/hooks/useDoubleEscInterrupt";
import { useStuckWatchdog } from "@/hooks/useStuckWatchdog";
import { useSessionStreamSubscription } from "@/hooks/useSessionStreamSubscription";
import { useSessionRestoration } from "@/hooks/useSessionRestoration";
import { sessionTransient } from "@/stores/session-transient-store";
import { MessageList } from "./messages/message-list";
import { ChatInput } from "./chat-input";
import { PermissionPrompt } from "./permission-prompt";
import { QuestionPrompt } from "./question-prompt";
import { ChatToolbar } from "./chat-toolbar";
import { HeaderActions } from "./header-actions";
import { SessionSettingsDialog } from "./session-settings-dialog";
import { RevertFilesDialog } from "./messages/revert-files-dialog";
import { BranchSwitcher } from "./branch-switcher";
import { WorkingIndicator } from "./working-indicator";
import { ApiRetryBanner } from "./api-retry-banner";
import { toneForMode } from "@/lib/mode-tone";
import {
  findLatestMainTodoWrite,
  parseTodoProgress,
} from "@/lib/todo-extract";
import { StuckBanner } from "./stuck-banner";
import { ImageLightbox } from "./image-lightbox";
import { DiffPanelHost, type DiffPanelHostApi } from "./diff-panel-host";
import { AgentContextPanelHost } from "./agent-context-panel-host";
import { SessionProvider } from "./session-context";

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
  const [effort, setEffort] = React.useState<ReasoningEffort>(
    () =>
      (sessionStorage.getItem(effortStorageKey) as ReasoningEffort) ?? "high",
  );
  const permissionStorageKey = `flowstate:permissionMode:${sessionId}`;
  const [permissionMode, setPermissionMode] =
    React.useState<PermissionMode>(
      () =>
        (sessionStorage.getItem(permissionStorageKey) as PermissionMode) ??
        "accept_edits",
    );

  // Load user-configured defaults from Settings for fresh sessions.
  // Only override when sessionStorage has no value (i.e. the session
  // hasn't been touched yet). Once the user changes a value in-session
  // it sticks — navigating away and back won't reset it.
  React.useEffect(() => {
    if (sessionStorage.getItem(effortStorageKey)) return;
    let cancelled = false;
    readDefaultEffort().then((saved) => {
      if (!cancelled && saved) setEffort(saved);
    });
    return () => {
      cancelled = true;
    };
  }, [effortStorageKey]);

  React.useEffect(() => {
    if (sessionStorage.getItem(permissionStorageKey)) return;
    let cancelled = false;
    readDefaultPermissionMode().then((saved) => {
      if (!cancelled && saved) setPermissionMode(saved);
    });
    return () => {
      cancelled = true;
    };
  }, [permissionStorageKey]);

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

  // Persist effort and permission mode to sessionStorage so they
  // survive navigation (e.g. Settings → back) without losing the
  // user's in-session choice.
  React.useEffect(() => {
    sessionStorage.setItem(effortStorageKey, effort);
  }, [effortStorageKey, effort]);

  React.useEffect(() => {
    sessionStorage.setItem(permissionStorageKey, permissionMode);
    // Mirror to the app-store so the sidebar thread spinner can tint
    // by the live composer mode (not the running turn's starting
    // mode). Reducer short-circuits when unchanged, so firing on
    // every sessionStorage write is cheap.
    dispatch({
      type: "set_session_permission_mode",
      sessionId,
      mode: permissionMode,
    });
  }, [dispatch, permissionStorageKey, permissionMode, sessionId]);

  const [pendingInput, setPendingInput] = React.useState<string | null>(null);
  // Monotonically-increasing tick bumped each time the user dispatches a
  // message via handleSend. MessageList watches this to force a scroll-
  // to-bottom on every send, regardless of current scroll position. A
  // counter (rather than a boolean) ensures every send fires the effect
  // even when consecutive sends would otherwise debounce to the same value.
  const [userSendTick, setUserSendTick] = React.useState(0);
  // Watchdog state: `lastEventAt` bumps on every stream event for this
  // session so the 45s inactivity timer resets. `stuckSince` is set
  // (inside the useStuckWatchdog hook) when the timer fires and a
  // pending tool call exists.
  const [lastEventAt, setLastEventAt] = React.useState<number>(() =>
    Date.now(),
  );
  // Coarse turn phase ("requesting" / "compacting" / …). Provider-
  // driven; only Claude SDK emits today. Cleared on turn_completed
  // so the stale label doesn't linger onto the next turn.
  const [turnPhase, setTurnPhase] = React.useState<TurnPhase | undefined>(
    undefined,
  );
  // In-flight auto-retry banner state. Set from `turn_retrying`
  // events; cleared on the first subsequent `content_delta` (model
  // started responding, retry succeeded) or `turn_completed` /
  // `session_interrupted`.
  const [retryState, setRetryState] = React.useState<RetryState | null>(null);
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
  const [sessionSettingsOpen, setSessionSettingsOpen] = React.useState(false);
  // Anchor turn for the per-user-message revert dialog. `null`
  // means closed; setting to a turn id opens the dialog with that
  // turn as the rewind target.
  const [revertAnchorTurnId, setRevertAnchorTurnId] = React.useState<
    string | null
  >(null);

  // Diff panel + agent-context panel state. Open flags live in the
  // transient session store so they follow the thread across
  // navigations; ChatView holds mirror state so rendering stays
  // reactive. Width / style live inside DiffPanelHost (persisted to
  // localStorage there).
  const splitContainerRef = React.useRef<HTMLDivElement | null>(null);
  const [diffOpen, setDiffOpenState] = React.useState<boolean>(() =>
    sessionTransient.getDiffOpen(sessionId),
  );
  const [diffFullscreen, setDiffFullscreen] = React.useState(false);
  const setDiffOpen = React.useCallback(
    (value: boolean) => {
      setDiffOpenState(value);
      sessionTransient.setDiffOpen(sessionId, value);
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

  // Agent-context pane state — mirrors the diff pane state. The two
  // panes are mutually exclusive (enforced in the toggle handlers
  // below); they share the split-right slot inside splitContainerRef.
  const [contextOpen, setContextOpenState] = React.useState<boolean>(() =>
    sessionTransient.getContextOpen(sessionId),
  );
  const [contextFullscreen, setContextFullscreen] = React.useState(false);
  const setContextOpen = React.useCallback(
    (value: boolean) => {
      setContextOpenState(value);
      sessionTransient.setContextOpen(sessionId, value);
    },
    [sessionId],
  );

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
  // Branch fetched via project-scoped queries, so switching between
  // threads in the same project reuses the cached values rather than
  // re-shelling out to git on every navigation.
  const gitBranchQuery = useQuery(gitBranchQueryOptions(projectPath));
  const gitBranch = gitBranchQuery.data ?? null;
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

  // Expose the diff-panel host's imperative API (refresh / activate
  // / toggle / close / diffs) so the header button, the hover
  // prefetch, and the stream subscription can all reach the same
  // instance without duplicating state.
  const diffHostRef = React.useRef<DiffPanelHostApi | null>(null);
  const activateDiffSubscription = React.useCallback(() => {
    diffHostRef.current?.activate();
  }, []);
  const refreshDiffs = React.useCallback((opts?: { force?: boolean }) => {
    diffHostRef.current?.refresh(opts);
  }, []);
  const diffs = diffHostRef.current?.diffs ?? [];

  // Keyboard shortcut for mode cycling (Shift+Tab)
  useModeCycleShortcut({
    session,
    sessionId,
    permissionMode,
    excludedModes,
    setPermissionMode,
  });

  // Escape interrupts the in-flight turn — but requires a *double* press
  // within 2s to actually fire.
  useDoubleEscInterrupt({ session, sessionId });

  // Set active session
  React.useEffect(() => {
    dispatch({ type: "set_active_session", sessionId });
    return () => {
      dispatch({ type: "set_active_session", sessionId: null });
    };
  }, [sessionId, dispatch]);

  // Collapsed restoration effects — permission-mode replay from last
  // turn AND per-view transient reset on thread switch.
  useSessionRestoration({
    sessionId,
    permissionStorageKey,
    sessionQuery,
    setPermissionMode,
    setPendingInput,
    setLastEventAt,
    setStuckSince: (v) => setStuckSince(v),
    setDiffOpenState,
    setContextOpenState,
    setDiffFullscreen,
    setContextFullscreen,
  });

  // Single stream listener for the lifetime of ChatView. Forwards
  // handlers to the store's shared subscription rather than opening
  // its own Tauri channel.
  useSessionStreamSubscription({
    sessionId,
    sessionIdRef,
    setPendingInput,
    setLastEventAt,
    setStuckSince: (v) => setStuckSince(v),
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
      sessionTransient.setDraft(sessionId, draft);
    },
    [sessionId],
  );

  // --- Queue persistence callbacks ---
  const handleQueueChange = React.useCallback(
    (queue: import("./chat-input").QueuedMessage[]) => {
      sessionTransient.setQueue(sessionId, queue);
    },
    [sessionId],
  );

  async function handleSend(input: string, images: AttachedImage[] = []) {
    // Clear the persisted draft on send — the message is gone from
    // the composer and we don't want it to reappear if the user
    // navigates away and back before the component remounts.
    sessionTransient.clearDraft(sessionId);
    sessionTransient.clearQueue(sessionId);
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
    // Mutual exclusion with the diff pane: opening context closes
    // diff so the split-right slot is clean.
    if (contextOpen) {
      setContextOpen(false);
      setContextFullscreen(false);
    } else {
      setContextOpen(true);
      setDiffOpen(false);
      setDiffFullscreen(false);
    }
  }, [contextOpen, setContextOpen, setDiffOpen]);

  const handleToggleDiff = React.useCallback(() => {
    // Mutual exclusion with the agent-context pane.
    if (diffOpen) {
      setDiffOpen(false);
      setDiffFullscreen(false);
    } else {
      setDiffOpen(true);
      setContextOpen(false);
      setContextFullscreen(false);
      diffHostRef.current?.activate();
      diffHostRef.current?.refresh({ force: true });
    }
  }, [diffOpen, setDiffOpen, setContextOpen]);

  // Is there at least one tool call on the running turn still waiting
  // for its completion event? That's the precondition for the
  // stuck-watchdog: we don't care about ordinary model thinking
  // latency, only about cases where a tool is visibly in "pending"
  // and nothing is moving.
  const hasPendingToolCall = React.useMemo(() => {
    if (!runningTurn) return false;
    return (runningTurn.toolCalls ?? []).some((tc) => tc.status === "pending");
  }, [runningTurn]);

  // Arm the stuck-watchdog.
  const { stuckSince, setStuckSince } = useStuckWatchdog({
    isRunning,
    hasPendingToolCall,
    lastEventAt,
  });

  // Per-session settings affordance gate. Reads the current
  // provider's feature flags; the gear button (and dialog) only
  // surface when at least one settable field is supported. As
  // future per-session fields land they should OR-in here.
  const sessionFeatures = useProviderFeatures(
    sessionQuery.data?.detail.summary.provider,
  );
  const hasSessionSettings = !!sessionFeatures.compactCustomInstructions;
  // Per-user-message revert affordance — gated on the same
  // ProviderFeatures lookup; only surfaces a click handler when
  // the provider opted in. Undefined here propagates through
  // MessageList → TurnView → UserMessage and the button stays
  // hidden.
  const handleRevertFiles = React.useMemo(() => {
    if (!sessionFeatures.fileCheckpoints) return undefined;
    return (turnId: string) => setRevertAnchorTurnId(turnId);
  }, [sessionFeatures.fileCheckpoints]);

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
      permissionMode={permissionMode}
      onPermissionModeChange={handlePermissionModeChange}
    />
  ) : null;

  return (
    <SessionProvider
      value={{
        sessionId,
        provider: session?.provider,
        model:
          sessionQuery.data?.detail.summary.model ?? session?.model ?? undefined,
      }}
    >
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
              onToggleDiff={handleToggleDiff}
              // Hover arms the subscription (exactly once, via the
              // React setState bail-out). No tick bump, no refetch —
              // the subscription fires as soon as it's activated and
              // subsequent hovers are no-ops.
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
              sessionModel={sessionQuery.data?.detail.summary.model}
              onRevertFiles={handleRevertFiles}
            />

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
              // internal state (pendingSend queue flag, slash-command
              // popup) resets cleanly. Draft text is now preserved
              // via initialValue / onDraftChange so the user's
              // in-progress message survives tab switches.
              key={sessionId}
              onSend={handleSend}
              onInterrupt={handleInterrupt}
              sessionStatus={session?.status}
              disabled={loading}
              providerDisabled={providerDisabled}
              archived={isArchived || worktreeFolderMissing}
              toolbar={toolbar}
              commands={slashCommands}
              provider={session?.provider}
              initialValue={sessionTransient.getDraft(sessionId)}
              onDraftChange={handleDraftChange}
              initialQueue={sessionTransient.getQueue(sessionId)}
              onQueueChange={handleQueueChange}
              permissionMode={permissionMode}
              promptSuggestion={promptSuggestion}
              onPromptSuggestionDismissed={() => setPromptSuggestion(null)}
            />
          </div>

          <DiffPanelHost
            ref={diffHostRef}
            sessionId={sessionId}
            projectPath={projectPath}
            containerRef={splitContainerRef}
            open={diffOpen}
            onOpenChange={setDiffOpen}
            fullscreen={diffFullscreen}
            onFullscreenChange={setDiffFullscreen}
          />

          <AgentContextPanelHost
            containerRef={splitContainerRef}
            open={contextOpen}
            onOpenChange={setContextOpen}
            fullscreen={contextFullscreen}
            onFullscreenChange={setContextFullscreen}
            turns={turns}
            runningTurn={runningTurn}
          />
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
        {sessionFeatures.fileCheckpoints && (
          <RevertFilesDialog
            open={revertAnchorTurnId !== null}
            onOpenChange={(next) => {
              if (!next) setRevertAnchorTurnId(null);
            }}
            sessionId={sessionId}
            anchorTurnId={revertAnchorTurnId}
            turns={turns}
          />
        )}
      </div>
    </SessionProvider>
  );
}
