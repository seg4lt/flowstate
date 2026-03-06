import * as React from "react";
import { Loader2 } from "lucide-react";

interface WorkingIndicatorProps {
  /**
   * Epoch ms of the most recent stream event for this session. The
   * displayed timer is `now - lastActivityAt` so it represents idle
   * time since the provider last said anything -- it ticks back to
   * "0s" every time a text delta, tool start, reasoning chunk, or
   * any other event arrives. The full turn duration isn't useful;
   * what the user wants to know is "is something stuck right now".
   */
  lastActivityAt: number;
  onInterrupt: () => void;
}

function formatElapsed(elapsedMs: number): string {
  const seconds = Math.max(0, Math.floor(elapsedMs / 1000));
  if (seconds < 60) return `${seconds}s`;
  const minutes = Math.floor(seconds / 60);
  const rem = seconds % 60;
  return `${minutes}m ${rem}s`;
}

function WorkingIndicatorInner({ lastActivityAt, onInterrupt }: WorkingIndicatorProps) {
  // Re-render every second so the timer ticks. We deliberately don't
  // useState(now) because that re-creates a closure on every render —
  // useReducer with a counter is the cheapest way to force a tick.
  const [, tick] = React.useReducer((n: number) => n + 1, 0);
  React.useEffect(() => {
    const id = setInterval(() => tick(), 1000);
    return () => clearInterval(id);
  }, []);

  const elapsedLabel = formatElapsed(Date.now() - lastActivityAt);

  return (
    <div className="flex shrink-0 items-center gap-2 border-t border-border/60 bg-muted/30 px-4 py-1.5 text-xs text-muted-foreground">
      <Loader2 className="h-3 w-3 animate-spin" />
      <span>
        Thinking<span className="animate-pulse">…</span>{" "}
        <span className="font-mono tabular-nums">{elapsedLabel}</span>
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
