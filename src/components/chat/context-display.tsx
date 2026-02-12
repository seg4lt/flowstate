import { Info } from "lucide-react";
import {
  Tooltip,
  TooltipContent,
  TooltipTrigger,
} from "@/components/ui/tooltip";

interface ContextDisplayProps {
  usedTokens?: number;
  maxTokens?: number;
}

function formatTokens(n: number): string {
  if (n >= 1000) return `${Math.round(n / 1000)}k`;
  return String(n);
}

export function ContextDisplay({ usedTokens, maxTokens }: ContextDisplayProps) {
  const used = usedTokens != null ? formatTokens(usedTokens) : "--";
  const max = maxTokens != null ? formatTokens(maxTokens) : "--";

  return (
    <Tooltip>
      <TooltipTrigger asChild>
        <button
          type="button"
          className="inline-flex items-center gap-1 rounded-md px-1.5 py-1 text-xs text-muted-foreground hover:text-foreground"
        >
          <Info className="h-3 w-3" />
          <span>
            {used} / {max}
          </span>
        </button>
      </TooltipTrigger>
      <TooltipContent side="top">
        Context window: {used} / {max} tokens
      </TooltipContent>
    </Tooltip>
  );
}
