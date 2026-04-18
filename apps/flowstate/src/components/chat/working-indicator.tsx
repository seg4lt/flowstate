import * as React from "react";
import { BrailleSpinner, type SpinnerTone } from "./braille-spinner";
import type { TurnPhase } from "@/lib/types";

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
  /**
   * Colour signal for the spinner. Blue when the turn is running in
   * plan mode (no writes yet, just thinking), green otherwise —
   * matches the visual language users already read from the mode
   * dropdown. Decoupled from `permissionMode` directly so this
   * component stays oblivious to how tones map to modes.
   */
  tone: SpinnerTone;
  /**
   * Coarse provider-reported turn phase. Only providers that set
   * `ProviderFeatures.statusLabels` emit these; for others this prop
   * stays `undefined` and no secondary label renders. Explicitly
   * `"streaming"` or `"idle"` also produces no label — the main
   * counter + spinner already carry that signal — so only
   * `requesting`, `compacting`, and `awaiting_input` render text.
   */
  phase?: TurnPhase;
  onInterrupt: () => void;
}

function formatElapsed(elapsedMs: number): string {
  const seconds = Math.max(0, Math.floor(elapsedMs / 1000));
  if (seconds < 60) return `${seconds}s`;
  const minutes = Math.floor(seconds / 60);
  const rem = seconds % 60;
  return `${minutes}m ${rem}s`;
}

function phaseLabel(phase: TurnPhase | undefined): string | null {
  switch (phase) {
    case "requesting":
      return "requesting…";
    case "compacting":
      return "compacting…";
    case "awaiting_input":
      return "awaiting input…";
    // "streaming" and "idle" are the normal states — the spinner +
    // main counter already convey them, so no secondary label.
    default:
      return null;
  }
}

function WorkingIndicatorInner({
  turnStartedAt,
  lastEventAt,
  tone,
  phase,
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
  const phaseText = phaseLabel(phase);

  return (
    <div className="flex shrink-0 items-center gap-2 border-t border-border/60 bg-muted/30 px-4 py-1.5 text-xs text-muted-foreground">
      <BrailleSpinner tone={tone} className="text-sm" label="Working" />
      <span>
        Working<span className="animate-pulse">…</span>{" "}
        <span className="font-mono tabular-nums">{totalLabel}</span>
      </span>
      {phaseText && (
        <>
          <span className="text-muted-foreground/50">·</span>
          <span className="text-muted-foreground/80">{phaseText}</span>
        </>
      )}
      <span className="text-muted-foreground/50">·</span>
      <span className="text-muted-foreground/70">
        last updated{" "}
        <span className="font-mono tabular-nums">{idleLabel}</span> ago
      </span>
      <button
        type="button"
        onClick={onInterrupt}
        className="ml-auto rounded px-1 text-muted-foreground/60 hover:bg-destructive/10 hover:text-destructive"
        title="Interrupt (Esc Esc)"
      >
        esc esc
      </button>
    </div>
  );
}

export const WorkingIndicator = React.memo(WorkingIndicatorInner);
