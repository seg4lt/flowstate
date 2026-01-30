import { ChevronDown, Gauge } from "lucide-react";
import { Button } from "../ui/button";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from "../ui/dropdown-menu";
import { REASONING_EFFORT_LABELS, type ReasoningEffort } from "../../types";

interface Props {
  value: ReasoningEffort;
  onChange: (next: ReasoningEffort) => void;
}

const OPTIONS: ReasoningEffort[] = ["minimal", "low", "medium", "high"];

export function ReasoningEffortPicker({ value, onChange }: Props) {
  return (
    <DropdownMenu>
      <DropdownMenuTrigger>
        <Button
          variant="ghost"
          size="sm"
          className="h-7 gap-1.5 px-2 text-xs text-muted-foreground hover:text-foreground"
          title="Reasoning effort"
        >
          <Gauge className="h-3.5 w-3.5" />
          <span>{REASONING_EFFORT_LABELS[value]}</span>
          <ChevronDown className="h-3 w-3 opacity-60" />
        </Button>
      </DropdownMenuTrigger>
      <DropdownMenuContent align="start">
        {OPTIONS.map((option) => (
          <DropdownMenuItem key={option} onClick={() => onChange(option)}>
            {REASONING_EFFORT_LABELS[option]}
          </DropdownMenuItem>
        ))}
      </DropdownMenuContent>
    </DropdownMenu>
  );
}
