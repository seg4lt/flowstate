import * as React from "react";
import type { UseQueryResult } from "@tanstack/react-query";
import type { PermissionMode } from "@/lib/types";
import type { SessionPage } from "@/lib/queries";
import { sessionTransient } from "@/stores/session-transient-store";

// Collapses the two "restore per-view state on thread switch" effects
// that used to live inline in chat-view:
//
//  - Rederive permission mode from the last persisted turn when the
//    user lands on a session with no sessionStorage entry, and
//    detect unmatched EnterPlanMode tool calls so plan mode survives
//    a cross-session agent-initiated change.
//  - Reset per-view transient UI state (pending input, watchdog,
//    panel open flags) on every thread switch, pulling panel open
//    flags from the transient session store so they follow the
//    thread.
export function useSessionRestoration(params: {
  sessionId: string;
  permissionStorageKey: string;
  sessionQuery: UseQueryResult<SessionPage>;
  setPermissionMode: (mode: PermissionMode) => void;
  setPendingInput: (value: string | null) => void;
  setLastEventAt: (ts: number) => void;
  setStuckSince: (value: number | null) => void;
  setDiffOpenState: (open: boolean) => void;
  setContextOpenState: (open: boolean) => void;
  setDiffFullscreen: (v: boolean) => void;
  setContextFullscreen: (v: boolean) => void;
}): void {
  const {
    sessionId,
    permissionStorageKey,
    sessionQuery,
    setPermissionMode,
    setPendingInput,
    setLastEventAt,
    setStuckSince,
    setDiffOpenState,
    setContextOpenState,
    setDiffFullscreen,
    setContextFullscreen,
  } = params;

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
  }, [sessionQuery.data, permissionStorageKey, setPermissionMode]);

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
    // Resync per-thread panel open flags from the module-level store.
    // ChatView doesn't remount on session switch, so the lazy
    // useState initializers only ran for the very first session —
    // this effect brings the mirror state into sync with the store on
    // every subsequent switch. Fullscreen is intentionally reset
    // (it's a momentary intent, not a thread preference).
    setDiffOpenState(sessionTransient.getDiffOpen(sessionId));
    setContextOpenState(sessionTransient.getContextOpen(sessionId));
    setDiffFullscreen(false);
    setContextFullscreen(false);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [sessionId]);
}
