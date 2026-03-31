import * as React from "react";
import { Loader2 } from "lucide-react";

interface WorkingIndicatorProps {
  /**
   * Epoch ms when the current turn started — taken from the daemon's
   * `turn.createdAt` so client and server don't drift. This is the
   * anchor for the main "Working… 1m 23s" counter, which is monotonic:
   * it ticks from turn start to turn completion without resetting on
   * individual stream events. That's the total wall-clock time the
   * turn has been in flight, which is what the user actually wants to
   * know.
   */
  turnStartedAt: number;
  /**
   * Epoch ms of the most recent stream event for this session. Drives
   * the "last updated Xs ago" sub-label so the user can still see
   * whether the provider is actively streaming or has gone silent
   * mid-turn — a signal the old indicator carried implicitly by
   * resetting the counter to 0 every event.
   */
  lastEventAt: number;
  onInterrupt: () => void;
}

function formatElapsed(elapsedMs: number): string {
  const seconds = Math.max(0, Math.floor(elapsedMs / 1000));
  if (seconds < 60) return `${seconds}s`;
  const minutes = Math.floor(seconds / 60);
  const rem = seconds % 60;
  return `${minutes}m ${rem}s`;
}

function WorkingIndicatorInner({
  turnStartedAt,
  lastEventAt,
  onInterrupt,
}: WorkingIndicatorProps) {
  // Re-render every second so both counters tick. We deliberately
  // don't useState(now) because that re-creates a closure on every
  // render — useReducer with a counter is the cheapest way to force
  // a tick.
  const [, tick] = React.useReducer((n: number) => n + 1, 0);
  React.useEffect(() => {
    const id = setInterval(() => tick(), 1000);
    return () => clearInterval(id);
  }, []);

  const now = Date.now();
  const totalLabel = formatElapsed(now - turnStartedAt);
  const idleLabel = formatElapsed(now - lastEventAt);

  return (
    <div className="flex shrink-0 items-center gap-2 border-t border-border/60 bg-muted/30 px-4 py-1.5 text-xs text-muted-foreground">
      <Loader2 className="h-3 w-3 animate-spin" />
      <span>
        Working<span className="animate-pulse">…</span>{" "}
        <span className="font-mono tabular-nums">{totalLabel}</span>
      </span>
      <span className="text-muted-foreground/50">·</span>
      <span className="text-muted-foreground/70">
        last updated{" "}
        <span className="font-mono tabular-nums">{idleLabel}</span> ago
      </span>
      <button
        type="button"
        onClick={onInterrupt}
        className="ml-auto rounded px-1 text-muted-foreground/60 hover:bg-destructive/10 hover:text-destructive"
        title="Interrupt (Esc)"
      >
        esc
      </button>
    </div>
  );
}

export const WorkingIndicator = React.memo(WorkingIndicatorInner);
