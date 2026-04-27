// Typed replacement for the module-level Map<string, …> instances that
// used to sit at the top of chat-view.tsx. Kept in memory (not
// persisted); survives ChatView re-renders and ChatInput remounts so
// draft text / queued messages / panel-open flags follow the session
// as the user switches threads. Lost on reload — all four are
// transient UI state, not preferences.
//
// Why a module here instead of a React context or the global store?
//
//   * Reads and writes happen outside React's render cycle (from
//     send-handler callbacks, panel toggle callbacks, etc.). A
//     context would add rerender pressure.
//   * These aren't part of the global reducer's state tree — the
//     sidebar / toolbar / header never read them. Keeping them
//     component-local-but-module-scoped mirrors the old shape
//     exactly.
//   * Typing the surface makes it obvious when a new transient
//     field lands (previously the maps were anonymous at the top
//     of a 2k-line file).

import type { QueuedMessage } from "@/components/chat/chat-input";

// Per-session draft text. Module-level so it survives ChatView
// re-renders and ChatInput remounts (keyed by sessionId). Cleared
// on send so completed messages don't linger as stale drafts.
const sessionDrafts = new Map<string, string>();

// Per-session message queue. Module-level so it survives ChatInput
// remounts (keyed by sessionId). Same pattern as sessionDrafts —
// preserves queued messages when the user switches threads mid-turn.
const sessionQueues = new Map<string, QueuedMessage[]>();

// Per-session diff / context / code-view panel open flags, plus the
// per-session git-mode toggle inside the code view. Module-level so
// they survive thread switches (ChatView does NOT remount on
// sessionId change — sessionId is a prop, not a key). Lost on reload,
// which is intentional: "did I leave the diff open" / "was git mode
// on for this thread" is transient UI state, not a persisted
// preference. Width / style live in localStorage in the panel hosts
// because those ARE preferences. Fullscreen is deliberately NOT
// per-thread — it's a momentary intent.
const sessionDiffOpen = new Map<string, boolean>();
const sessionContextOpen = new Map<string, boolean>();
const sessionCodeViewOpen = new Map<string, boolean>();
const sessionGitMode = new Map<string, boolean>();

// Subscribers for sessionGitMode. The editor-prefs hook reads this
// map through `useSyncExternalStore`, so flips made from one mounted
// view (e.g. the code-view header toggle) propagate to anything else
// rendering the same thread's git-mode in the same render pass. The
// other Maps don't need this because their consumers always own the
// authoritative React state and write through to the map (one-way),
// whereas git-mode wants the map to be the source of truth so the
// hook can stay shaped like the existing vim/wrap singleton.
const gitModeSubscribers = new Set<() => void>();

export const sessionTransient = {
  getDraft: (sessionId: string): string =>
    sessionDrafts.get(sessionId) ?? "",
  setDraft: (sessionId: string, draft: string): void => {
    if (draft.length > 0) sessionDrafts.set(sessionId, draft);
    else sessionDrafts.delete(sessionId);
  },
  clearDraft: (sessionId: string): void => {
    sessionDrafts.delete(sessionId);
  },

  getQueue: (sessionId: string): QueuedMessage[] | undefined =>
    sessionQueues.get(sessionId),
  setQueue: (sessionId: string, queue: QueuedMessage[]): void => {
    if (queue.length > 0) sessionQueues.set(sessionId, queue);
    else sessionQueues.delete(sessionId);
  },
  clearQueue: (sessionId: string): void => {
    sessionQueues.delete(sessionId);
  },

  getDiffOpen: (sessionId: string): boolean =>
    sessionDiffOpen.get(sessionId) ?? false,
  setDiffOpen: (sessionId: string, open: boolean): void => {
    if (open) sessionDiffOpen.set(sessionId, true);
    else sessionDiffOpen.delete(sessionId);
  },

  getContextOpen: (sessionId: string): boolean =>
    sessionContextOpen.get(sessionId) ?? false,
  setContextOpen: (sessionId: string, open: boolean): void => {
    if (open) sessionContextOpen.set(sessionId, true);
    else sessionContextOpen.delete(sessionId);
  },

  getCodeViewOpen: (sessionId: string): boolean =>
    sessionCodeViewOpen.get(sessionId) ?? false,
  setCodeViewOpen: (sessionId: string, open: boolean): void => {
    if (open) sessionCodeViewOpen.set(sessionId, true);
    else sessionCodeViewOpen.delete(sessionId);
  },

  getGitMode: (sessionId: string | null | undefined): boolean => {
    if (!sessionId) return false;
    return sessionGitMode.get(sessionId) ?? false;
  },
  setGitMode: (
    sessionId: string | null | undefined,
    enabled: boolean,
  ): void => {
    if (!sessionId) return;
    if (enabled) sessionGitMode.set(sessionId, true);
    else sessionGitMode.delete(sessionId);
    for (const fn of gitModeSubscribers) fn();
  },
  subscribeGitMode: (notify: () => void): (() => void) => {
    gitModeSubscribers.add(notify);
    return () => {
      gitModeSubscribers.delete(notify);
    };
  },
};
