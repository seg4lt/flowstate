import { Check, ChevronDown } from "lucide-react";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import type { ReasoningEffort } from "@/lib/types";

const EFFORT_OPTIONS: { value: ReasoningEffort; label: string }[] = [
  { value: "high", label: "High" },
  { value: "medium", label: "Medium" },
  { value: "low", label: "Low" },
  { value: "minimal", label: "Minimal" },
];

interface EffortSelectorProps {
  value: ReasoningEffort;
  onChange: (effort: ReasoningEffort) => void;
}

export function EffortSelector({ value, onChange }: EffortSelectorProps) {
  const currentLabel =
    EFFORT_OPTIONS.find((o) => o.value === value)?.label ?? "High";

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
        {EFFORT_OPTIONS.map((option) => (
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
