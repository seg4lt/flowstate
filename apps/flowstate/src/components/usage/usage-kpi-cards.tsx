import * as React from "react";
import { cn } from "@/lib/utils";
import type { UsageGroupRow, UsageTotals } from "@/lib/api";
import { estimateCacheReadSavingsUsd } from "@/lib/api";

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

// Cache hit rate uses the *standard* denominator: every input
// token the API saw, whether it was new, written to cache, or
// served from cache. The pre-fix formula omitted cache writes,
// which inflated the apparent hit rate on bursty days that wrote
// fresh prefixes mid-session.
function cacheHitRatio(totals: UsageTotals): number | null {
  const totalInput =
    totals.inputTokens + totals.cacheReadTokens + totals.cacheWriteTokens;
  if (totalInput === 0) return null;
  return totals.cacheReadTokens / totalInput;
}

// What the dashboard *should* call "input": the full prompt volume
// the API processed, not just the new-tokens slice that the
// Anthropic SDK reports as `input_tokens`. Without this, the card
// reads "6.2k in" when the actual processed prompt was 1.2M+
// tokens — a 200× understatement on cache-heavy workloads.
function totalProcessedInput(totals: UsageTotals): number {
  return totals.inputTokens + totals.cacheReadTokens + totals.cacheWriteTokens;
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

// Sum a per-model savings estimate across the per-model breakdown.
// Splitting per model (rather than using grand totals + a single
// price) keeps the math honest on multi-model ranges where one
// Opus turn would otherwise smear its expensive rate over Haiku
// reads. Returns null when no group has known pricing — the card
// then hides the dollar figure rather than guessing.
function aggregateCacheSavings(groups: UsageGroupRow[] | undefined): {
  savedUsd: number | null;
  unknownGroupCount: number;
} {
  if (!groups || groups.length === 0) {
    return { savedUsd: null, unknownGroupCount: 0 };
  }
  let saved = 0;
  let known = 0;
  let unknown = 0;
  for (const g of groups) {
    if (g.cacheReadTokens === 0) continue;
    const slice = estimateCacheReadSavingsUsd(g.label, g.cacheReadTokens);
    if (slice === null) {
      unknown += 1;
      continue;
    }
    saved += slice;
    known += 1;
  }
  return {
    savedUsd: known === 0 ? null : saved,
    unknownGroupCount: unknown,
  };
}

export interface UsageKpiCardsProps {
  totals: UsageTotals;
  /// Per-model breakdown for the same range, used to estimate
  /// cache savings at family-specific rates. Optional — when
  /// omitted (e.g. in unit tests), the Cache card omits the
  /// dollar figure and shows just counts + hit rate.
  modelGroups?: UsageGroupRow[];
}

export function UsageKpiCards({ totals, modelGroups }: UsageKpiCardsProps) {
  const hitRatio = cacheHitRatio(totals);
  const processedIn = totalProcessedInput(totals);
  const avgDurationMs =
    totals.turnCount === 0
      ? 0
      : Math.round(totals.totalDurationMs / totals.turnCount);
  const { savedUsd, unknownGroupCount } = aggregateCacheSavings(modelGroups);
  const hasAnyCache = totals.cacheReadTokens > 0 || totals.cacheWriteTokens > 0;

  return (
    <div className="grid grid-cols-1 gap-3 sm:grid-cols-2 lg:grid-cols-3 xl:grid-cols-5">
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
        <div
          className="flex items-baseline gap-2 text-lg font-semibold tabular-nums"
          title={
            "Total prompt tokens the API processed (new + cached reads + cache writes), " +
            "vs total tokens the model generated."
          }
        >
          <span>{formatCompact(processedIn)}</span>
          <span className="text-muted-foreground">/</span>
          <span>{formatCompact(totals.outputTokens)}</span>
        </div>
        <div className="text-xs text-muted-foreground">
          in / out
        </div>
        <div
          className="text-xs text-muted-foreground tabular-nums"
          title={
            "New: first-time tokens billed at 1×.\n" +
            "Cached: read from prompt cache, billed at ~0.1×.\n" +
            "Writes: first-time tokens written to the cache, billed at ~1.25×."
          }
        >
          <span>{formatCompact(totals.inputTokens)} new</span>
          <span className="px-1">·</span>
          <span>{formatCompact(totals.cacheReadTokens)} cached</span>
          <span className="px-1">·</span>
          <span>{formatCompact(totals.cacheWriteTokens)} writes</span>
        </div>
      </Card>

      <Card title="Cache">
        <div className="text-2xl font-semibold tabular-nums">
          {hitRatio === null ? "—" : `${Math.round(hitRatio * 100)}%`}
          <span className="ml-1 text-xs font-normal text-muted-foreground">
            hit
          </span>
        </div>
        <div
          className="text-xs text-muted-foreground tabular-nums"
          title={
            "Cache hit rate = cache_read / (input + cache_read + cache_write)."
          }
        >
          {hasAnyCache ? (
            <>
              <span>{formatCompact(totals.cacheReadTokens)} read</span>
              <span className="px-1">·</span>
              <span>{formatCompact(totals.cacheWriteTokens)} write</span>
            </>
          ) : (
            <span>no cache activity</span>
          )}
        </div>
        <div
          className="text-xs text-muted-foreground"
          title={
            unknownGroupCount > 0
              ? `Estimate excludes ${unknownGroupCount} model${
                  unknownGroupCount === 1 ? "" : "s"
                } with unknown pricing.`
              : "Estimated savings from prompt caching at Anthropic list rates."
          }
        >
          {savedUsd === null
            ? hasAnyCache
              ? "savings: —"
              : "savings: $0"
            : `~${formatCost(savedUsd)}${unknownGroupCount > 0 ? "+" : ""} saved`}
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
