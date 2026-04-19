import * as React from "react";

// Trip the watchdog after this many seconds of silence while a tool
// call is pending. Picked to be well past a normal tool round-trip
// (even a slow Bash / Git command rarely exceeds 15–20s) but short
// enough that a user who just clicked Allow doesn't sit for a minute
// wondering if anything is happening.
const STUCK_TIMEOUT_MS = 45_000;

// Arm the stuck-watchdog. We only trip it when the session is
// running *and* at least one tool call is pending, so idle
// pre-tool "Thinking…" periods don't falsely flag as stuck. The
// timer is rearmed by `lastEventAt` bumping on each event.
export function useStuckWatchdog(params: {
  isRunning: boolean;
  hasPendingToolCall: boolean;
  lastEventAt: number;
}): {
  stuckSince: number | null;
  setStuckSince: React.Dispatch<React.SetStateAction<number | null>>;
} {
  const { isRunning, hasPendingToolCall, lastEventAt } = params;
  const [stuckSince, setStuckSince] = React.useState<number | null>(null);

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

  return { stuckSince, setStuckSince };
}
