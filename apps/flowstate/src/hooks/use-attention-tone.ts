import * as React from "react";
import { useApp } from "@/stores/app-store";

export type AttentionTone = "awaiting" | "done" | null;

/**
 * Aggregates the two "wants attention" sets from the app store into
 * a single tone, excluding the active session (the user is already
 * looking at it). "awaiting" (blue) beats "done" (green) because a
 * paused agent waiting on input is strictly more urgent than a turn
 * that just finished.
 *
 * Returns null when there is nothing attention-worthy outside the
 * active thread — render sites should short-circuit on null.
 */
export function useAttentionTone(): AttentionTone {
  const { state } = useApp();
  const activeId = state.activeSessionId;

  const hasAwaiting = React.useMemo(() => {
    for (const id of state.awaitingInputSessionIds) {
      if (id !== activeId) return true;
    }
    return false;
  }, [state.awaitingInputSessionIds, activeId]);

  const hasDone = React.useMemo(() => {
    if (hasAwaiting) return false;
    for (const id of state.doneSessionIds) {
      if (id !== activeId) return true;
    }
    return false;
  }, [state.doneSessionIds, activeId, hasAwaiting]);

  if (hasAwaiting) return "awaiting";
  if (hasDone) return "done";
  return null;
}
