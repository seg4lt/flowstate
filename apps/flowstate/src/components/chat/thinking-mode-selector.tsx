import { cn } from "@/lib/utils";
import type { ThinkingMode } from "@/lib/types";

// Per-turn dial (orthogonal to effort) controlling *when* Claude
// thinks: `always` forces a concrete `budgetTokens` so the model
// reasons every turn (deterministic — the pre-`11232b3` behaviour
// users asked for back), `adaptive` keeps the SDK's `{ type:
// 'adaptive' }` where Claude decides per-turn. Rendered as a two-pill
// segmented radio so the state is glanceable inline in the toolbar;
// each press is the full per-turn commit (next `send_turn` carries
// the chosen value, and the bridge triggers a Query reopen if it
// differs from the last turn's value).
const OPTIONS: { value: ThinkingMode; label: string; title: string }[] = [
  {
    value: "always",
    label: "Always",
    title: "Always think (deterministic reasoning every turn)",
  },
  {
    value: "adaptive",
    label: "Adaptive",
    title: "Claude decides per-turn whether to think",
  },
];

interface ThinkingModeSelectorProps {
  value: ThinkingMode;
  onChange: (mode: ThinkingMode) => void;
}

export function ThinkingModeSelector({
  value,
  onChange,
}: ThinkingModeSelectorProps) {
  return (
    <div
      role="radiogroup"
      aria-label="Thinking mode"
      className="inline-flex items-center rounded-md border border-border bg-background p-0.5 text-xs"
    >
      {OPTIONS.map((option) => {
        const selected = value === option.value;
        return (
          <button
            key={option.value}
            type="button"
            role="radio"
            aria-checked={selected}
            title={option.title}
            onClick={() => onChange(option.value)}
            className={cn(
              "rounded px-2 py-0.5 transition-colors",
              selected
                ? "bg-accent text-accent-foreground"
                : "text-muted-foreground hover:text-foreground",
            )}
          >
            {option.label}
          </button>
        );
      })}
    </div>
  );
}
