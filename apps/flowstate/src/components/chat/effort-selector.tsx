import { Check, ChevronDown } from "lucide-react";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import type { ReasoningEffort } from "@/lib/types";

// Full set of effort levels flowstate knows about, in the order the
// menu renders. `xhigh` and `max` are SDK `EffortLevel` values that
// only some Anthropic models accept — the selector filters them out
// unless the active model's `supportedEffortLevels` includes them.
// `minimal` is flowstate-native (maps to `thinking: disabled` in the
// bridge) and always shows.
export const EFFORT_OPTIONS: { value: ReasoningEffort; label: string }[] = [
  { value: "max", label: "Max" },
  { value: "xhigh", label: "X-High" },
  { value: "high", label: "High" },
  { value: "medium", label: "Medium" },
  { value: "low", label: "Low" },
  { value: "minimal", label: "Minimal" },
];

// Levels whose visibility is gated by the active model's
// `ModelInfo.supportedEffortLevels`. `minimal` is flowstate-native
// and `low`/`medium`/`high` are the universal baseline, so they are
// not gated here.
const MODEL_GATED_LEVELS = new Set<ReasoningEffort>(["xhigh", "max"]);

/**
 * Filter `EFFORT_OPTIONS` by what the active model reports it can
 * handle. Shared between the in-session `<EffortSelector>` and the
 * global `Settings → Default Effort` picker so there's one source
 * of truth for the gating rule.
 *
 * @param supportedEffortLevels - values from
 *   `ProviderModel.supportedEffortLevels` (straight from the Claude
 *   Agent SDK's `ModelInfo.supportedEffortLevels`). Empty / undefined
 *   means "unknown" — `xhigh` / `max` are hidden in that case so
 *   models that don't advertise them don't expose a level they'll
 *   reject. `minimal` / `low` / `medium` / `high` always show.
 */
export function visibleEffortOptions(
  supportedEffortLevels?: readonly string[],
): { value: ReasoningEffort; label: string }[] {
  const supported = new Set(supportedEffortLevels ?? []);
  return EFFORT_OPTIONS.filter((option) => {
    if (!MODEL_GATED_LEVELS.has(option.value)) return true;
    return supported.has(option.value);
  });
}

interface EffortSelectorProps {
  value: ReasoningEffort;
  onChange: (effort: ReasoningEffort) => void;
  /** Effort levels the active model accepts, straight from the
   *  Claude Agent SDK's `ModelInfo.supportedEffortLevels`. Empty
   *  means "unknown / provider didn't enumerate" — the selector
   *  then shows only the universal baseline (hides `xhigh`/`max`).
   *  Non-empty narrows the gated levels to what's listed. */
  supportedEffortLevels?: readonly string[];
}

export function EffortSelector({
  value,
  onChange,
  supportedEffortLevels,
}: EffortSelectorProps) {
  const visibleOptions = visibleEffortOptions(supportedEffortLevels);
  const currentLabel =
    visibleOptions.find((o) => o.value === value)?.label ??
    EFFORT_OPTIONS.find((o) => o.value === value)?.label ??
    "High";

  return (
    <DropdownMenu>
      <DropdownMenuTrigger asChild>
        <button
          type="button"
          className="inline-flex items-center gap-1 rounded-md px-2 py-1 text-xs hover:bg-accent"
        >
          {currentLabel}
          <ChevronDown className="h-3 w-3 text-muted-foreground" />
        </button>
      </DropdownMenuTrigger>
      <DropdownMenuContent align="start" className="min-w-36">
        <DropdownMenuLabel>Effort</DropdownMenuLabel>
        {visibleOptions.map((option) => (
          <DropdownMenuItem
            key={option.value}
            onClick={() => onChange(option.value)}
          >
            {value === option.value ? (
              <Check className="mr-2 h-3 w-3" />
            ) : (
              <span className="mr-2 w-3" />
            )}
            {option.label}
          </DropdownMenuItem>
        ))}
      </DropdownMenuContent>
    </DropdownMenu>
  );
}
