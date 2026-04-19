import * as React from "react";
import { cn } from "@/lib/utils";
import type { UsageGroupRow, UsageTotals } from "@/lib/api";
import { estimateCacheReadSavingsUsd } from "@/lib/pricing";

// KPI card grid that sits above the fold of the Usage dashboard.
//
// Layout: 4-col × 2-row grid.
//
//   Row 1 (cost / activity):
//     Spend │ Turns │ Avg turn duration │ Cache hit %
//
//   Row 2 (token breakdown — what the model saw vs. produced):
//     Tokens in │ Tokens out │ Cache read │ Cache write
//
// All numbers come from `usage_events.total_cost_usd / *_tokens /
// duration_ms`, which the Claude Agent SDK reports as the *aggregate
// across the whole turn including subagent Task calls* (see bridge
// comment at provider-claude-sdk/bridge/src/index.ts ~L1576). So
// these totals already roll subagent activity in — there is no
// double-counting, and there is no missing subagent slice.
//
// Naming rationale: the Anthropic API field `input_tokens` does NOT
// mean "fresh content the user sent". With cache_control on the
// latest message (which the SDK does by default), the user's new
// content lands in `cache_creation_input_tokens` (= cache write),
// and `input_tokens` is just the trailing scaffold bytes after the
// last cache breakpoint — typically 1–100 tokens per turn even on
// hour-long sessions. We surface it as "Uncached" with a tooltip
// instead of the misleading "new", which historically led people to
// think the dashboard was undercounting.

function formatCost(cost: number): string {
  if (cost === 0) return "$0.00";
  if (cost < 0.01) return "<$0.01";
  if (cost < 1) return `$${cost.toFixed(3)}`;
  if (cost < 100) return `$${cost.toFixed(2)}`;
  return `$${cost.toFixed(0)}`;
}

function formatCompact(n: number): string {
  if (n < 1_000) return n.toString();
  if (n < 1_000_000) return `${(n / 1_000).toFixed(1)}k`;
  if (n < 1_000_000_000) return `${(n / 1_000_000).toFixed(1)}m`;
  return `${(n / 1_000_000_000).toFixed(2)}b`;
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

// Cache hit rate denominator is the *total prompt volume the API
// processed* — every input-side token whether new, written to cache,
// or served from cache. Excluding cache writes here would inflate
// the apparent hit rate on bursty days that wrote fresh prefixes
// mid-session.
function cacheHitRatio(totals: UsageTotals): number | null {
  const denom =
    totals.inputTokens + totals.cacheReadTokens + totals.cacheWriteTokens;
  if (denom === 0) return null;
  return totals.cacheReadTokens / denom;
}

// Total prompt volume the model actually received — sum of every
// input-side token the API processed, regardless of caching tier.
// The Anthropic SDK's `input_tokens` is *only* the post-breakpoint
// uncached tail; on cache-heavy workloads (the default for the
// Agent SDK) it's typically <1% of the real input volume.
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
  titleHint,
}: {
  title: string;
  children: React.ReactNode;
  className?: string;
  titleHint?: string;
}) {
  return (
    <div
      data-slot="usage-kpi-card"
      className={cn(
        "rounded-lg border border-border bg-background p-4 shadow-sm",
        className,
      )}
    >
      <div
        className="text-xs font-medium tracking-wide text-muted-foreground uppercase"
        title={titleHint}
      >
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
  const avgCostPerTurn =
    totals.turnCount === 0 ? 0 : totals.totalCostUsd / totals.turnCount;
  const { savedUsd, unknownGroupCount } = aggregateCacheSavings(modelGroups);
  const hasAnyCache = totals.cacheReadTokens > 0 || totals.cacheWriteTokens > 0;

  return (
    <div className="grid grid-cols-2 gap-3 lg:grid-cols-4">
      {/* ─── Row 1: cost / activity ───────────────────────── */}

      <Card
        title="Total spend"
        titleHint="Sum of total_cost_usd from every turn in the selected range. Includes subagent Task-tool cost (the SDK aggregates it into the parent turn's cost)."
      >
        <div className="flex items-baseline">
          <div className="text-2xl font-semibold tabular-nums">
            {formatCost(totals.totalCostUsd)}
          </div>
          {totals.costHasUnknowns ? <PartialBadge /> : null}
        </div>
        <div className="text-xs text-muted-foreground tabular-nums">
          {formatCost(avgCostPerTurn)} / turn ·{" "}
          {totals.distinctModels} model
          {totals.distinctModels === 1 ? "" : "s"}
        </div>
      </Card>

      <Card
        title="Turns"
        titleHint="Number of completed agent turns recorded in usage_events. One turn = one user message → final assistant response, including any subagent calls within."
      >
        <div className="text-2xl font-semibold tabular-nums">
          {formatCompact(totals.turnCount)}
        </div>
        <div className="text-xs text-muted-foreground tabular-nums">
          {totals.distinctSessions} session
          {totals.distinctSessions === 1 ? "" : "s"}
        </div>
      </Card>

      <Card
        title="Avg turn duration"
        titleHint="Wall-clock time from turn start to final result, including time spent in subagent Task calls and tool execution. Mean across all turns in the range."
      >
        <div className="text-2xl font-semibold tabular-nums">
          {formatDuration(avgDurationMs)}
        </div>
        <div className="text-xs text-muted-foreground tabular-nums">
          total {formatDuration(totals.totalDurationMs)}
        </div>
      </Card>

      <Card
        title="Cache hit"
        titleHint={
          "cache_read ÷ (uncached + cache_read + cache_write).\n\n" +
          "On the Agent SDK with default cache_control breakpoints, " +
          "this should sit above ~95% on any session past its first turn."
        }
      >
        <div className="text-2xl font-semibold tabular-nums">
          {hitRatio === null ? "—" : `${Math.round(hitRatio * 100)}%`}
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
              : "no cache activity"
            : `~${formatCost(savedUsd)}${unknownGroupCount > 0 ? "+" : ""} saved`}
        </div>
      </Card>

      {/* ─── Row 2: token volumes ─────────────────────────── */}

      <Card
        title="Tokens in"
        titleHint={
          "Total prompt-side tokens the model received this range.\n\n" +
          "  = uncached + cache read + cache write\n\n" +
          "i.e. every input token the API processed, regardless of " +
          "whether it was billed at the new (1×), cache-read (0.1×), " +
          "or cache-write (1.25×) tier."
        }
      >
        <div className="text-2xl font-semibold tabular-nums">
          {formatCompact(processedIn)}
        </div>
        <div
          className="text-xs text-muted-foreground tabular-nums"
          title={
            "Uncached: bytes after the last cache_control breakpoint " +
            "(typically a few tokens of scaffold per turn — NOT 'new content you sent', " +
            "which lands in cache write).\n\n" +
            "Cached: read from prompt cache (~0.1× input price).\n" +
            "Writes: new content folded into cache (~1.25× input price)."
          }
        >
          <span>{formatCompact(totals.inputTokens)} uncached</span>
        </div>
      </Card>

      <Card
        title="Tokens out"
        titleHint="Tokens the model generated (assistant output, including tool-call args). Billed at the model's output rate."
      >
        <div className="text-2xl font-semibold tabular-nums">
          {formatCompact(totals.outputTokens)}
        </div>
        <div className="text-xs text-muted-foreground tabular-nums">
          {processedIn === 0
            ? "—"
            : `${(totals.outputTokens / processedIn).toFixed(2)}× of input`}
        </div>
      </Card>

      <Card
        title="Cache read"
        titleHint="Tokens served from the prompt cache, billed at ~0.1× the input rate. This is where steady-state agent traffic lives — the bigger this is, the less you pay per turn."
      >
        <div className="text-2xl font-semibold tabular-nums">
          {formatCompact(totals.cacheReadTokens)}
        </div>
        <div className="text-xs text-muted-foreground tabular-nums">
          {totals.turnCount === 0
            ? "—"
            : `${formatCompact(Math.round(totals.cacheReadTokens / totals.turnCount))} / turn`}
        </div>
      </Card>

      <Card
        title="Cache write"
        titleHint={
          "Tokens written to the prompt cache, billed at ~1.25× the " +
          "input rate.\n\n" +
          "This is the *real* 'new content per turn' signal: your " +
          "message + tool results that the SDK is folding into the " +
          "cache prefix for the next turn to read."
        }
      >
        <div className="text-2xl font-semibold tabular-nums">
          {formatCompact(totals.cacheWriteTokens)}
        </div>
        <div className="text-xs text-muted-foreground tabular-nums">
          {totals.turnCount === 0
            ? "—"
            : `${formatCompact(Math.round(totals.cacheWriteTokens / totals.turnCount))} / turn`}
        </div>
      </Card>
    </div>
  );
}
