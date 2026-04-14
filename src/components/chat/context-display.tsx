import * as React from "react";
import { useQuery } from "@tanstack/react-query";
import { Info } from "lucide-react";
import {
  Popover,
  PopoverContent,
  PopoverTrigger,
} from "@/components/ui/popover";
import { sessionQueryOptions } from "@/lib/queries";
import type { TokenUsage, TurnRecord } from "@/lib/types";

interface ContextDisplayProps {
  sessionId: string;
}

function formatTokens(n: number | undefined | null): string {
  if (n == null) return "--";
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(1)}M`;
  if (n >= 1000) return `${Math.round(n / 1000)}k`;
  return String(n);
}

function formatCost(cost: number | undefined): string | null {
  if (cost == null) return null;
  if (cost < 0.01) return `$${cost.toFixed(4)}`;
  return `$${cost.toFixed(2)}`;
}

function formatDuration(ms: number | undefined): string | null {
  if (ms == null) return null;
  if (ms < 1000) return `${ms}ms`;
  return `${(ms / 1000).toFixed(1)}s`;
}

function findLatestUsage(turns: TurnRecord[] | undefined): TokenUsage | null {
  if (!turns) return null;
  for (let i = turns.length - 1; i >= 0; i--) {
    if (turns[i].usage) return turns[i].usage!;
  }
  return null;
}

export function ContextDisplay({ sessionId }: ContextDisplayProps) {
  const { data } = useQuery(sessionQueryOptions(sessionId));
  const usage = React.useMemo(
    () => findLatestUsage(data?.detail.turns),
    [data?.detail.turns],
  );

  const used = usage
    ? usage.inputTokens + usage.outputTokens
    : null;
  const total = usage?.contextWindow ?? null;
  const pct =
    used != null && total != null && total > 0
      ? Math.min(100, Math.round((used / total) * 100))
      : null;

  const usedLabel = formatTokens(used);
  const totalLabel = formatTokens(total);
  const costLabel = formatCost(usage?.totalCostUsd);
  const durationLabel = formatDuration(usage?.durationMs);
  const cacheRead = usage?.cacheReadInputTokens ?? 0;
  const cacheCreate = usage?.cacheCreationInputTokens ?? 0;
  const hasCache = cacheRead > 0 || cacheCreate > 0;

  const barFillClass =
    pct == null
      ? "bg-foreground/60"
      : pct >= 80
        ? "bg-destructive"
        : pct >= 50
          ? "bg-amber-500"
          : "bg-foreground/60";

  return (
    <Popover>
      <PopoverTrigger asChild>
        <button
          type="button"
          className="inline-flex items-center gap-1 rounded-md px-1.5 py-1 text-xs text-muted-foreground hover:text-foreground"
          title="Context window & usage"
        >
          <Info className="h-3 w-3" />
          <span className="tabular-nums">
            {usedLabel} / {totalLabel}
          </span>
        </button>
      </PopoverTrigger>
      <PopoverContent side="top" align="end" className="w-80 p-3">
        <div className="flex items-center gap-2">
          <span className="text-[10px] font-medium uppercase tracking-wide text-muted-foreground">
            Context window
          </span>
          {pct != null && (
            <span className="ml-auto text-[11px] tabular-nums text-muted-foreground">
              {usedLabel} / {totalLabel} · {pct}%
            </span>
          )}
          {pct == null && usage && (
            <span className="ml-auto text-[11px] tabular-nums text-muted-foreground">
              {usedLabel} / --
            </span>
          )}
        </div>
        {pct != null && (
          <div className="mt-2 h-0.5 w-full overflow-hidden rounded-full bg-muted/40">
            <div
              className={`h-full transition-all ${barFillClass}`}
              style={{ width: `${pct}%` }}
            />
          </div>
        )}
        {!usage && (
          <div className="mt-2 italic text-[11px] text-muted-foreground/70">
            No usage data yet — run a turn to populate.
          </div>
        )}
        {usage && hasCache && (
          <div className="mt-2 text-[11px] text-muted-foreground/80">
            cache read: <span className="tabular-nums">{formatTokens(cacheRead)}</span>
            {" · "}cache write:{" "}
            <span className="tabular-nums">{formatTokens(cacheCreate)}</span>
          </div>
        )}
        {usage && (costLabel || durationLabel) && (
          <div className="mt-1 text-[11px] text-muted-foreground/80">
            {costLabel && <span className="tabular-nums">{costLabel}</span>}
            {costLabel && durationLabel && " · "}
            {durationLabel && <span className="tabular-nums">{durationLabel}</span>}
          </div>
        )}
        <div className="mt-3 border-t border-border/60 pt-2 italic text-[11px] text-muted-foreground/60">
          Plan usage bars coming soon.
        </div>
      </PopoverContent>
    </Popover>
  );
}
