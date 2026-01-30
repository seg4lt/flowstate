import { ChevronDown, Sparkles } from "lucide-react";
import { Button } from "../ui/button";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuSeparator,
  DropdownMenuSub,
  DropdownMenuSubContent,
  DropdownMenuSubTrigger,
  DropdownMenuTrigger,
} from "../ui/dropdown-menu";
import type { ProviderKind, ProviderStatus } from "../../types";
import { PROVIDER_COLORS, PROVIDER_LABELS } from "../../types";

interface Props {
  provider: ProviderKind;
  model: string | null;
  providers: ProviderStatus[];
  disabled?: boolean;
  onChange: (provider: ProviderKind, model: string) => void;
}

const COMING_SOON: Array<{ id: string; label: string }> = [
  { id: "gemini", label: "Gemini" },
  { id: "opencode", label: "OpenCode" },
  { id: "cursor", label: "Cursor" },
];

export function ProviderModelPicker({
  provider,
  model,
  providers,
  disabled,
  onChange,
}: Props) {
  const activeProvider = providers.find((p) => p.kind === provider);
  const modelLabel =
    activeProvider?.models.find((m) => m.value === model)?.label ??
    model ??
    "Select model";

  return (
    <DropdownMenu>
      <DropdownMenuTrigger disabled={disabled}>
        <Button
          variant="ghost"
          size="sm"
          className="h-7 gap-1.5 px-2 text-xs text-muted-foreground hover:text-foreground"
        >
          <div className={`w-2 h-2 rounded-full shrink-0 ${PROVIDER_COLORS[provider]}`} />
          <span className="truncate max-w-[140px]">{modelLabel}</span>
          <ChevronDown className="h-3 w-3 opacity-60" />
        </Button>
      </DropdownMenuTrigger>
      <DropdownMenuContent align="start" className="w-60">
        {providers.map((liveProvider) => {
          const isReady = liveProvider.status === "ready" && liveProvider.installed;
          if (!isReady) {
            return (
              <DropdownMenuItem key={liveProvider.kind} disabled className="gap-2">
                <div
                  className={`w-2 h-2 rounded-full shrink-0 ${PROVIDER_COLORS[liveProvider.kind]}`}
                />
                <span>{liveProvider.label}</span>
                <span className="ml-auto text-[10px] uppercase tracking-wider text-muted-foreground">
                  {liveProvider.installed ? "Not ready" : "Not installed"}
                </span>
              </DropdownMenuItem>
            );
          }
          return (
            <DropdownMenuSub key={liveProvider.kind}>
              <DropdownMenuSubTrigger className="gap-2">
                <div
                  className={`w-2 h-2 rounded-full shrink-0 ${PROVIDER_COLORS[liveProvider.kind]}`}
                />
                {liveProvider.label}
              </DropdownMenuSubTrigger>
              <DropdownMenuSubContent>
                {liveProvider.models.length === 0 ? (
                  <DropdownMenuItem disabled>No models cached</DropdownMenuItem>
                ) : (
                  liveProvider.models.map((m) => (
                    <DropdownMenuItem
                      key={m.value}
                      onClick={() => onChange(liveProvider.kind, m.value)}
                    >
                      {m.label}
                    </DropdownMenuItem>
                  ))
                )}
              </DropdownMenuSubContent>
            </DropdownMenuSub>
          );
        })}
        <DropdownMenuSeparator />
        {COMING_SOON.map((item) => (
          <DropdownMenuItem key={item.id} disabled className="gap-2">
            <Sparkles className="h-3.5 w-3.5 opacity-60" />
            <span>{item.label}</span>
            <span className="ml-auto text-[10px] uppercase tracking-wider text-muted-foreground">
              Coming soon
            </span>
          </DropdownMenuItem>
        ))}
      </DropdownMenuContent>
    </DropdownMenu>
  );
}

void PROVIDER_LABELS;
