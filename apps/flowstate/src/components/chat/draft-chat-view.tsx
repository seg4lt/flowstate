import * as React from "react";
import { useNavigate } from "@tanstack/react-router";
import { SidebarTrigger, useSidebar } from "@/components/ui/sidebar";
import { useApp } from "@/stores/app-store";
import { useDefaultProvider } from "@/hooks/use-default-provider";
import {
  readDefaultEffort,
  readDefaultModel,
  readDefaultPermissionMode,
} from "@/lib/defaults-settings";
import { rememberPickedModel } from "@/lib/model-settings";
import { sendMessage } from "@/lib/api";
import { isMacOS, isPopoutWindow } from "@/lib/popout";
import { deriveAutoTitle } from "@/lib/auto-title";
import { toast } from "@/hooks/use-toast";
import type {
  AttachedImage,
  PermissionMode,
  ProviderKind,
  ReasoningEffort,
  ThinkingMode,
} from "@/lib/types";
import { ChatInput } from "./chat-input";
import { ChatToolbar } from "./chat-toolbar";

// Per-project draft text. Backed by sessionStorage so a hard reload
// (Cmd+R) doesn't drop the user's typed message. Module-level Map
// also caches reads so the synchronous `initialValue` lookup at mount
// time doesn't pay the storage hit on every navigation. Keyed by
// projectId — the draft route is `/chat/draft/$projectId`, with the
// special key `""` standing in for the project-less variant
// (`/chat/draft`). Once the user sends, both the in-memory Map and
// the sessionStorage entry are dropped (the message has been moved
// to the real session).
const DRAFT_STORAGE_PREFIX = "flowstate:draft:";
const draftTexts = new Map<string, string>();

function draftStorageKey(projectId: string): string {
  return DRAFT_STORAGE_PREFIX + projectId;
}

function readDraft(projectId: string): string {
  const cached = draftTexts.get(projectId);
  if (cached !== undefined) return cached;
  try {
    const raw = window.sessionStorage.getItem(draftStorageKey(projectId));
    if (raw) {
      draftTexts.set(projectId, raw);
      return raw;
    }
  } catch {
    /* sessionStorage may be unavailable in the popout sandbox */
  }
  return "";
}

function writeDraft(projectId: string, value: string): void {
  if (value.length === 0) {
    draftTexts.delete(projectId);
    try {
      window.sessionStorage.removeItem(draftStorageKey(projectId));
    } catch {
      /* storage may be unavailable */
    }
    return;
  }
  draftTexts.set(projectId, value);
  try {
    window.sessionStorage.setItem(draftStorageKey(projectId), value);
  } catch {
    /* storage may be unavailable */
  }
}

interface DraftChatViewProps {
  /** Project (or worktree) the new thread will live under. Undefined
   *  routes the draft to the project-less "General" bucket — the
   *  resulting session lands with `projectId: null`, which the
   *  sidebar shows under the General header. */
  projectId?: string;
}

/**
 * Empty / "draft" chat view. Rendered at `/chat/draft/$projectId`
 * after the user picks a project (or worktree) from the sidebar
 * pencil or the ⌘⇧N picker. No backend session exists yet — provider
 * / model / effort / permissionMode are all local state seeded from
 * the user's saved defaults.
 *
 * On the user's first send we synchronously call `start_session` then
 * `send_turn`, and `replace: true` navigate to the real
 * `/chat/$sessionId`. The browser back button doesn't return to the
 * draft url, which is the right UX — the draft is consumed by the
 * send.
 *
 * If the spawn fails (provider not ready, etc.) we toast and stay on
 * the draft route so the user can pick a different provider in the
 * toolbar dropdown without losing their typed message.
 */
export function DraftChatView({ projectId }: DraftChatViewProps) {
  const { state, send, renameSession } = useApp();
  const navigate = useNavigate();

  // Stable key for draftTexts / sessionStorage — empty string for the
  // project-less variant so the read/write helpers don't need a
  // separate "no project" branch.
  const draftKey = projectId ?? "";

  // Look up project metadata for the header label and the
  // `@`-mention file picker. May be undefined briefly during
  // bootstrap; we fall back gracefully so the view doesn't flash an
  // error state for a known-good projectId.
  const project = React.useMemo(
    () =>
      projectId
        ? state.projects.find((p) => p.projectId === projectId)
        : undefined,
    [state.projects, projectId],
  );
  const projectPath = project?.path ?? null;
  const displayName = projectId
    ? state.projectDisplay.get(projectId)?.name ?? "New thread"
    : null;

  // macOS traffic-light spacer (see chat-view.tsx for full context).
  const { state: sidebarState } = useSidebar();
  const inPopoutWindow = isPopoutWindow();
  const showMacTrafficSpacer =
    isMacOS() && (inPopoutWindow || sidebarState === "collapsed");

  // ── Local toolbar state ────────────────────────────────────────
  // Provider / model start from the user's configured defaults; the
  // toolbar dropdowns let the user adjust before the first send.
  const { defaultProvider, loaded: defaultProviderLoaded } =
    useDefaultProvider();
  const [provider, setProvider] = React.useState<ProviderKind>(defaultProvider);
  const [model, setModel] = React.useState<string | undefined>(undefined);
  // Track whether the user has explicitly picked a provider in the
  // toolbar yet. Until they have, we keep `provider` in sync with the
  // resolved `defaultProvider` (which may flip from the bootstrap
  // value to the user's saved preference once SQLite read finishes).
  const userPickedProviderRef = React.useRef(false);
  React.useEffect(() => {
    if (userPickedProviderRef.current) return;
    if (!defaultProviderLoaded) return;
    setProvider(defaultProvider);
  }, [defaultProvider, defaultProviderLoaded]);

  // Resolve the model whenever the provider changes (and the user
  // hasn't picked a model yet for this provider). Same fallback
  // chain `worktree-new-thread-dropdown.tsx` uses: saved default →
  // first cached catalog entry → undefined (let the adapter pick).
  const userPickedModelRef = React.useRef(false);
  React.useEffect(() => {
    if (userPickedModelRef.current) return;
    let cancelled = false;
    void (async () => {
      const saved = await readDefaultModel(provider);
      if (cancelled) return;
      if (saved) {
        setModel(saved);
        return;
      }
      const fallback = state.providers.find((p) => p.kind === provider)
        ?.models[0]?.value;
      setModel(fallback);
    })();
    return () => {
      cancelled = true;
    };
  }, [provider, state.providers]);

  const [effort, setEffort] = React.useState<ReasoningEffort>("high");
  const [thinkingMode, setThinkingMode] =
    React.useState<ThinkingMode>("always");
  const [permissionMode, setPermissionMode] =
    React.useState<PermissionMode>("accept_edits");

  // Hydrate effort / permission-mode from saved defaults once on mount
  // (mirrors chat-view.tsx's "fresh thread" branch). Failures fall
  // through to the hard-coded initial state above — no toasts; the
  // user can fix it via the toolbar.
  React.useEffect(() => {
    let cancelled = false;
    void readDefaultEffort().then((saved) => {
      if (!cancelled && saved) setEffort(saved);
    });
    void readDefaultPermissionMode().then((saved) => {
      if (!cancelled && saved) setPermissionMode(saved);
    });
    return () => {
      cancelled = true;
    };
  }, []);

  // ── Draft text persistence ────────────────────────────────────
  // ChatInput is keyed by projectId so it remounts on navigation
  // back; `initialValue` seeds it from the saved draft, and
  // `onDraftChange` writes back on every keystroke.
  const handleDraftChange = React.useCallback(
    (value: string) => {
      writeDraft(draftKey, value);
    },
    [draftKey],
  );

  // ── Send path ─────────────────────────────────────────────────
  const handleSend = React.useCallback(
    async (input: string, images: AttachedImage[]) => {
      // 1. Spawn the real session with the picked provider / model.
      const startRes = await send({
        type: "start_session",
        provider,
        model,
        project_id: projectId,
      });
      if (!startRes || startRes.type !== "session_created") {
        const message =
          startRes?.type === "error"
            ? startRes.message
            : "Failed to start session.";
        toast({
          title: "Failed to start thread",
          description: message,
          duration: 4000,
        });
        // Re-throw so ChatInput's drain effect treats this as a
        // failed send and keeps the queued message in place — same
        // contract the active-session handleSend uses.
        throw new Error(message);
      }
      const newSessionId = startRes.session.sessionId;

      // 2. Pin the picked model alias so the toolbar capability
      //    lookup is stable across the SDK's `model_resolved`
      //    overwrite. Same trick model-selector / sidebar dropdown use.
      if (model) {
        rememberPickedModel(newSessionId, model);
      }

      // 3. Auto-title the brand-new session from the user's input
      //    (matches the active-session handleSend's auto-title path
      //    for fresh threads). Best-effort; failure here is silent.
      const autoTitle = deriveAutoTitle(input);
      if (autoTitle.length > 0) {
        void renameSession(newSessionId, autoTitle);
      }

      // 4. Dispatch the user's message as the first turn. This is
      //    deliberately serial after start_session — the session has
      //    to exist on the daemon before send_turn can find it.
      try {
        const resp = await sendMessage({
          type: "send_turn",
          session_id: newSessionId,
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
        if (resp?.type === "error") {
          throw new Error(resp.message);
        }
      } catch (err) {
        // Surface the failure to the user but DON'T un-spawn the
        // session — it exists on the daemon now and the user can
        // retry from the active route. Navigate so they're not
        // stranded on the draft route with a session-less composer.
        toast({
          title: "Failed to send first message",
          description: String(err),
          duration: 4000,
        });
        navigate({
          to: "/chat/$sessionId",
          params: { sessionId: newSessionId },
          replace: true,
        });
        throw err;
      }

      // 5. Drop the saved draft (the typed message has been promoted
      //    to a real turn) and navigate. `replace: true` so the back
      //    button doesn't bounce the user back to the empty draft.
      writeDraft(draftKey, "");
      navigate({
        to: "/chat/$sessionId",
        params: { sessionId: newSessionId },
        replace: true,
      });
    },
    [
      send,
      provider,
      model,
      projectId,
      draftKey,
      permissionMode,
      effort,
      thinkingMode,
      renameSession,
      navigate,
    ],
  );

  // No-ops for the composer's interrupt / steer paths — there's no
  // running turn to interrupt and no session to steer. ChatInput's
  // archived/disabled gating already keeps these off the UI when the
  // input is read-only, but in draft mode the composer IS active
  // (that's the whole point), and a turn isn't running, so these
  // simply never get called.
  const handleInterruptNoop = React.useCallback(async () => {}, []);
  const handleSteerNoop = React.useCallback(async () => {}, []);

  const toolbar = (
    <ChatToolbar
      mode="draft"
      sessionId=""
      provider={provider}
      currentModel={model}
      onProviderChange={(p, defaultModel) => {
        userPickedProviderRef.current = true;
        userPickedModelRef.current = false;
        setProvider(p);
        // The picker resolved a sensible default model for the new
        // provider — apply it immediately so the model chip and the
        // first-send `start_session` payload stay in sync without
        // waiting for the provider-change effect to re-run.
        if (defaultModel !== undefined) {
          setModel(defaultModel);
        }
      }}
      onModelChange={(m) => {
        userPickedModelRef.current = true;
        setModel(m);
      }}
      effort={effort}
      onEffortChange={setEffort}
      thinkingMode={thinkingMode}
      onThinkingModeChange={setThinkingMode}
      permissionMode={permissionMode}
      onPermissionModeChange={setPermissionMode}
    />
  );

  return (
    <div className="absolute inset-0 flex min-w-0 flex-col overflow-hidden">
      <header
        data-tauri-drag-region
        className="flex h-9 shrink-0 items-center gap-1 border-b border-border px-2 text-sm"
      >
        {showMacTrafficSpacer && (
          <div className="w-16 shrink-0" data-tauri-drag-region />
        )}
        {!isPopoutWindow() && <SidebarTrigger />}
        <div
          data-tauri-drag-region
          className="flex min-w-0 flex-1 items-center gap-2"
        >
          <span
            data-tauri-drag-region
            className="min-w-0 flex-1 truncate font-medium select-none text-muted-foreground"
          >
            {displayName ? `New thread · ${displayName}` : "New thread"}
          </span>
        </div>
      </header>
      {/* Empty transcript area. Centered hint text replaces the
          message list — once the user sends we navigate away and
          ChatView takes over. */}
      <div className="flex flex-1 items-center justify-center px-6 text-center">
        <div className="max-w-md">
          <h2 className="mb-2 text-lg font-medium text-foreground/90">
            Start a new thread
          </h2>
          <p className="text-sm text-muted-foreground">
            Pick a provider and model in the toolbar below, then send your
            first message to start the conversation.
          </p>
        </div>
      </div>
      <ChatInput
        // Remount when the draft target changes so the composer's
        // slash popup / queue resets cleanly. `draftKey` covers both
        // the project-bound and project-less variants.
        key={draftKey}
        onSend={handleSend}
        onInterrupt={handleInterruptNoop}
        onSteer={handleSteerNoop}
        sessionStatus={undefined}
        disabled={false}
        toolbar={toolbar}
        provider={provider}
        initialValue={readDraft(draftKey)}
        onDraftChange={handleDraftChange}
        permissionMode={permissionMode}
        projectPath={projectPath}
        // No session id yet — ChatInput already supports this for
        // pre-spawn drafts (sessionId: string | null in its prop type).
        sessionId={null}
      />
    </div>
  );
}
