import * as React from "react";
import { Loader2 } from "lucide-react";

interface WorkingIndicatorProps {
  /** ISO-8601 timestamp the turn started at (turn.createdAt). */
  startedAt: string;
  onInterrupt: () => void;
}

function formatElapsed(elapsedMs: number): string {
  const seconds = Math.max(0, Math.floor(elapsedMs / 1000));
  if (seconds < 60) return `${seconds}s`;
  const minutes = Math.floor(seconds / 60);
  const rem = seconds % 60;
  return `${minutes}m ${rem}s`;
}

function WorkingIndicatorInner({ startedAt, onInterrupt }: WorkingIndicatorProps) {
  // Re-render every second so the timer ticks. We deliberately don't
  // useState(now) because that re-creates a closure on every render —
  // useReducer with a counter is the cheapest way to force a tick.
  const [, tick] = React.useReducer((n: number) => n + 1, 0);
  React.useEffect(() => {
    const id = setInterval(() => tick(), 1000);
    return () => clearInterval(id);
  }, []);

  const startMs = React.useMemo(() => {
    const parsed = Date.parse(startedAt);
    return Number.isFinite(parsed) ? parsed : Date.now();
  }, [startedAt]);

  const elapsedLabel = formatElapsed(Date.now() - startMs);

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
