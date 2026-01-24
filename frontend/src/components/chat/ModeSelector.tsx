// TODO: confirm with user — this collapses the reference app's separate
// "Chat"/"Plan" (collaboration mode) and "Full access" (permission mode) buttons
// into zenui's single 4-value PermissionMode enum. Revisit if the user wants
// them split into two controls.
import { Check, ChevronDown, ListTodo, Shield, Unlock } from "lucide-react";
import type { LucideIcon } from "lucide-react";
import { Button } from "../ui/button";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from "../ui/dropdown-menu";
import type { PermissionMode } from "../../types";

interface Props {
  value: PermissionMode;
  onChange: (next: PermissionMode) => void;
}

interface ModeOption {
  mode: PermissionMode;
  label: string;
  description: string;
  Icon: LucideIcon;
}

const OPTIONS: ModeOption[] = [
  {
    mode: "default",
    label: "Ask before edits",
    description: "Request confirmation before tools or file edits.",
    Icon: Shield,
  },
  {
    mode: "accept_edits",
    label: "Auto-accept edits",
    description: "Auto-approve file edits, ask for other actions.",
    Icon: Check,
  },
  {
    mode: "plan",
    label: "Plan mode",
    description: "Only propose a plan — no tool execution yet.",
    Icon: ListTodo,
  },
  {
    mode: "bypass",
    label: "Full access",
    description: "Bypass all approvals. Use with care.",
    Icon: Unlock,
  },
];

export function ModeSelector({ value, onChange }: Props) {
  const active = OPTIONS.find((o) => o.mode === value) ?? OPTIONS[1];
  const ActiveIcon = active.Icon;
  return (
    <DropdownMenu>
      <DropdownMenuTrigger>
        <Button
          variant="ghost"
          size="sm"
          className="h-7 gap-1.5 px-2 text-xs text-muted-foreground hover:text-foreground"
          title={active.description}
        >
          <ActiveIcon className="h-3.5 w-3.5" />
          <span>{active.label}</span>
          <ChevronDown className="h-3 w-3 opacity-60" />
        </Button>
      </DropdownMenuTrigger>
      <DropdownMenuContent align="start" className="w-64">
        {OPTIONS.map((option) => {
          const Icon = option.Icon;
          return (
            <DropdownMenuItem
              key={option.mode}
              onClick={() => onChange(option.mode)}
              className="gap-2 py-2"
            >
              <Icon className="h-3.5 w-3.5 shrink-0 text-muted-foreground" />
              <div className="flex flex-col gap-0.5 min-w-0">
                <span className="text-sm">{option.label}</span>
                <span className="text-[11px] text-muted-foreground leading-4">
                  {option.description}
                </span>
              </div>
            </DropdownMenuItem>
          );
        })}
      </DropdownMenuContent>
    </DropdownMenu>
  );
}
