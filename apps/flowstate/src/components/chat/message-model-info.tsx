import * as React from "react";
import { Check, Copy, Info } from "lucide-react";
import {
  Popover,
  PopoverContent,
  PopoverTrigger,
} from "@/components/ui/popover";
import { Button } from "@/components/ui/button";
import type { ProviderKind } from "@/lib/types";
import { resolveModelDisplay } from "@/lib/model-lookup";
import { useApp } from "@/stores/app-store";
import { useCopy } from "@/hooks/use-copy";
import { cn } from "@/lib/utils";

interface MessageModelInfoProps {
  /** Raw provider-level model id. e.g. `claude-sonnet-4-5-20250929`.
   *  Pass undefined to hide the icon entirely. */
  modelId: string | undefined;
  /** Provider kind used to look up the display label for the model
   *  and provider. */
  providerKind: ProviderKind;
  className?: string;
}

/**
 * Small info icon that opens a popover describing the model used by
 * the message above. Useful when the SDK resolves a short alias
 * (e.g. "sonnet") to a pinned dated version — the toolbar dropdown
 * still shows the user's selected label, but this popover exposes
 * exactly what hit the API.
 */
export function MessageModelInfo({
  modelId,
  providerKind,
  className,
}: MessageModelInfoProps) {
  const { state } = useApp();
  const copy = useCopy();
  const [copied, setCopied] = React.useState(false);

  if (!modelId) return null;

  const { label, providerLabel, rawId } = resolveModelDisplay(
    modelId,
    providerKind,
    state.providers,
  );

  const onCopyRaw = async (e: React.MouseEvent) => {
    e.stopPropagation();
    await copy(rawId, "Copied model id");
    setCopied(true);
    window.setTimeout(() => setCopied(false), 1200);
  };

  return (
    <Popover>
      <PopoverTrigger asChild>
        <Button
          type="button"
          variant="ghost"
          size="icon-xs"
          title="Model used for this reply"
          aria-label="Model used for this reply"
          className={cn(
            "text-muted-foreground opacity-60 hover:opacity-100 focus-visible:opacity-100",
            className,
          )}
        >
          <Info className="h-3 w-3" />
        </Button>
      </PopoverTrigger>
      <PopoverContent side="top" align="start" className="w-72 p-3">
        <div className="mb-2 text-[10px] font-medium uppercase tracking-wide text-muted-foreground">
          Model for this reply
        </div>
        <dl className="space-y-1.5 text-[11px]">
          {providerLabel && (
            <div className="flex items-baseline gap-2">
              <dt className="w-16 shrink-0 text-muted-foreground">Provider</dt>
              <dd className="truncate">{providerLabel}</dd>
            </div>
          )}
          {label && label !== rawId && (
            <div className="flex items-baseline gap-2">
              <dt className="w-16 shrink-0 text-muted-foreground">Label</dt>
              <dd className="truncate">{label}</dd>
            </div>
          )}
          <div className="flex items-baseline gap-2">
            <dt className="w-16 shrink-0 text-muted-foreground">Model</dt>
            <dd className="min-w-0 flex-1 truncate">
              <button
                type="button"
                onClick={onCopyRaw}
                title="Click to copy"
                className="group inline-flex items-baseline gap-1 font-mono text-[11px] hover:text-foreground"
              >
                <span className="truncate">{rawId}</span>
                {copied ? (
                  <Check className="h-3 w-3 shrink-0 opacity-70" />
                ) : (
                  <Copy className="h-3 w-3 shrink-0 opacity-40 group-hover:opacity-80" />
                )}
              </button>
            </dd>
          </div>
        </dl>
      </PopoverContent>
    </Popover>
  );
}
