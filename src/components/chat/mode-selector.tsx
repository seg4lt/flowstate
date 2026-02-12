import { Check, ChevronDown } from "lucide-react";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import type { PermissionMode } from "@/lib/types";

const MODE_OPTIONS: { value: PermissionMode; label: string }[] = [
  { value: "default", label: "Default" },
  { value: "accept_edits", label: "Auto-edit" },
  { value: "plan", label: "Plan" },
  { value: "bypass", label: "Full access" },
];

interface ModeSelectorProps {
  value: PermissionMode;
  onChange: (mode: PermissionMode) => void;
}

export function ModeSelector({ value, onChange }: ModeSelectorProps) {
  const currentLabel =
    MODE_OPTIONS.find((o) => o.value === value)?.label ?? "Default";

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
        <DropdownMenuLabel>Mode</DropdownMenuLabel>
        {MODE_OPTIONS.map((option) => (
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
