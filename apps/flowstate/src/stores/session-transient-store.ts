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

// Per-session diff / context panel open flags. Module-level so they
// survive thread switches (ChatView does NOT remount on sessionId
// change — sessionId is a prop, not a key). Lost on reload, which is
// intentional: "did I leave the diff open" is transient UI state, not
// a persisted preference. Width / style live in localStorage in the
// panel hosts because those ARE preferences. Fullscreen is
// deliberately NOT per-thread — it's a momentary intent.
const sessionDiffOpen = new Map<string, boolean>();
const sessionContextOpen = new Map<string, boolean>();

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
};
