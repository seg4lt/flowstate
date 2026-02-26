import * as React from "react";
import { Loader2 } from "lucide-react";
import type { ProviderKind } from "@/lib/types";

interface WorkingIndicatorProps {
  provider: ProviderKind;
  /** ISO-8601 timestamp the turn started at (turn.createdAt). */
  startedAt: string;
}

// Claude Code's CLI cycles through a long list of cute gerunds while
// the model is thinking. We do the same here for the Claude providers
// so the wait feels less dead. Other providers get a simple "Working".
const CLAUDE_PHRASES = [
  "Cogitating",
  "Pondering",
  "Manifesting",
  "Synthesizing",
  "Conjuring",
  "Brewing",
  "Channeling",
  "Crunching",
  "Distilling",
  "Ideating",
  "Mulling",
  "Reasoning",
  "Reflecting",
  "Ruminating",
  "Speculating",
  "Theorizing",
  "Wondering",
  "Vibing",
  "Noodling",
  "Hatching",
  "Percolating",
  "Marinating",
  "Simmering",
  "Spelunking",
  "Tinkering",
  "Concocting",
  "Doodling",
  "Forging",
  "Imagining",
  "Investigating",
  "Plotting",
  "Puzzling",
  "Scheming",
  "Sleuthing",
  "Surveying",
  "Visualizing",
  "Whirring",
];

const PHRASE_ROTATION_MS = 3000;

function pickPhrase(provider: ProviderKind, elapsedMs: number): string {
  if (provider === "claude" || provider === "claude_cli") {
    const idx =
      Math.floor(elapsedMs / PHRASE_ROTATION_MS) % CLAUDE_PHRASES.length;
    return CLAUDE_PHRASES[idx];
  }
  return "Working";
}

function formatElapsed(elapsedMs: number): string {
  const seconds = Math.max(0, Math.floor(elapsedMs / 1000));
  if (seconds < 60) return `${seconds}s`;
  const minutes = Math.floor(seconds / 60);
  const rem = seconds % 60;
  return `${minutes}m ${rem}s`;
}

function WorkingIndicatorInner({ provider, startedAt }: WorkingIndicatorProps) {
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

  const elapsedMs = Date.now() - startMs;
  const phrase = pickPhrase(provider, elapsedMs);
  const elapsedLabel = formatElapsed(elapsedMs);

  return (
    <div className="flex shrink-0 items-center gap-2 border-t border-border/60 bg-muted/30 px-4 py-1.5 text-xs text-muted-foreground">
      <Loader2 className="h-3 w-3 animate-spin" />
      <span>
        {phrase}
        <span className="animate-pulse">…</span>
      </span>
      <span className="ml-auto font-mono tabular-nums">{elapsedLabel}</span>
      <button
        type="button"
        className="text-muted-foreground/60 hover:text-foreground"
        title="Esc to interrupt"
        tabIndex={-1}
      >
        esc
      </button>
    </div>
  );
}

export const WorkingIndicator = React.memo(WorkingIndicatorInner);
