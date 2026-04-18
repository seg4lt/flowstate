import { Check, ChevronDown } from "lucide-react";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import type { PermissionMode, ProviderFeatures } from "@/lib/types";
import { MODE_ORDER, MODE_LABELS } from "@/lib/mode-cycling";

const MODE_OPTIONS: { value: PermissionMode; label: string }[] =
  MODE_ORDER.map((mode) => ({ value: mode, label: MODE_LABELS[mode] }));

interface ModeSelectorProps {
  value: PermissionMode;
  onChange: (mode: PermissionMode) => void;
  /** Provider capability flags for the session's provider. The
   *  "Auto" option is hidden when `supportsAutoPermissionMode` is
   *  falsy so providers without a classifier don't expose a
   *  no-op choice. Missing features object means "older daemon" —
   *  treat every flag as false. */
  features?: ProviderFeatures;
}

export function ModeSelector({ value, onChange, features }: ModeSelectorProps) {
  const visibleOptions = MODE_OPTIONS.filter((option) => {
    if (option.value === "auto") {
      return features?.supportsAutoPermissionMode === true;
    }
    return true;
  });
  const currentLabel =
    visibleOptions.find((o) => o.value === value)?.label ??
    MODE_OPTIONS.find((o) => o.value === value)?.label ??
    "Default";

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
      <DropdownMenuContent align="start" className="min-w-44">
        <DropdownMenuLabel>Mode</DropdownMenuLabel>
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
