import * as React from "react";
import { cn } from "@/lib/utils";
import type { UsageTotals } from "@/lib/api";

// KPI card grid that sits above the fold of the Usage dashboard.
// Plain Tailwind — no shadcn Card primitive is installed in this
// project so we keep the markup inline with the same
// border-border / bg-background / muted-foreground tokens used
// across the rest of the app.

function formatCost(cost: number): string {
  if (cost === 0) return "$0.00";
  if (cost < 0.01) return "<$0.01";
  return `$${cost.toFixed(2)}`;
}

function formatCompact(n: number): string {
  if (n < 1_000) return n.toString();
  if (n < 1_000_000) return `${(n / 1_000).toFixed(1)}k`;
  if (n < 1_000_000_000) return `${(n / 1_000_000).toFixed(1)}m`;
  return `${(n / 1_000_000_000).toFixed(1)}b`;
}

function formatDuration(ms: number): string {
  if (ms === 0) return "—";
  if (ms < 1_000) return `${ms}ms`;
  const seconds = ms / 1_000;
  if (seconds < 60) return `${seconds.toFixed(1)}s`;
  const minutes = seconds / 60;
  if (minutes < 60) return `${minutes.toFixed(1)}m`;
  return `${(minutes / 60).toFixed(1)}h`;
}

function cacheReadRatio(totals: UsageTotals): number | null {
  const totalInput = totals.inputTokens + totals.cacheReadTokens;
  if (totalInput === 0) return null;
  return totals.cacheReadTokens / totalInput;
}

function PartialBadge() {
  return (
    <span
      title="Some turns in this range reported no cost. Totals may be incomplete."
      className="ml-2 inline-flex items-center rounded bg-amber-500/15 px-1.5 py-0.5 text-[10px] font-medium text-amber-600 dark:text-amber-400"
    >
      partial
    </span>
  );
}

function Card({
  title,
  children,
  className,
}: {
  title: string;
  children: React.ReactNode;
  className?: string;
}) {
  return (
    <div
      data-slot="usage-kpi-card"
      className={cn(
        "rounded-lg border border-border bg-background p-4 shadow-sm",
        className,
      )}
    >
      <div className="text-xs font-medium tracking-wide text-muted-foreground uppercase">
        {title}
      </div>
      <div className="mt-2 space-y-1">{children}</div>
    </div>
  );
}

export function UsageKpiCards({ totals }: { totals: UsageTotals }) {
  const ratio = cacheReadRatio(totals);
  const avgDurationMs =
    totals.turnCount === 0
      ? 0
      : Math.round(totals.totalDurationMs / totals.turnCount);

  return (
    <div className="grid grid-cols-1 gap-3 sm:grid-cols-2 lg:grid-cols-4">
      <Card title="Total spend">
        <div className="flex items-baseline">
          <div className="text-2xl font-semibold tabular-nums">
            {formatCost(totals.totalCostUsd)}
          </div>
          {totals.costHasUnknowns ? <PartialBadge /> : null}
        </div>
        <div className="text-xs text-muted-foreground">
          across {totals.distinctModels} model
          {totals.distinctModels === 1 ? "" : "s"}
        </div>
      </Card>

      <Card title="Turns">
        <div className="text-2xl font-semibold tabular-nums">
          {formatCompact(totals.turnCount)}
        </div>
        <div className="text-xs text-muted-foreground">
          across {totals.distinctSessions} session
          {totals.distinctSessions === 1 ? "" : "s"}
        </div>
      </Card>

      <Card title="Tokens">
        <div className="flex items-baseline gap-2 text-lg font-semibold tabular-nums">
          <span>{formatCompact(totals.inputTokens)}</span>
          <span className="text-muted-foreground">/</span>
          <span>{formatCompact(totals.outputTokens)}</span>
        </div>
        <div className="text-xs text-muted-foreground">
          in / out
          {ratio !== null
            ? ` · ${Math.round(ratio * 100)}% cache read`
            : ""}
        </div>
      </Card>

      <Card title="Avg turn duration">
        <div className="text-2xl font-semibold tabular-nums">
          {formatDuration(avgDurationMs)}
        </div>
        <div className="text-xs text-muted-foreground">
          total {formatDuration(totals.totalDurationMs)}
        </div>
      </Card>
    </div>
  );
}
