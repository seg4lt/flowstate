import type { UsageGroupRow } from "@/lib/api";

// Per-provider / per-model breakdown table. Mirrors the KPI cards
// column-for-column so the grand totals at the top reconcile to the
// sum of the rows below — by SQL construction they always do (both
// queries hit `usage_events` in the same date window with identical
// SUMs; the only difference is GROUP BY).
//
// Columns:
//   • Turns           — count of turns in this slice
//   • In              — total prompt volume (uncached + read + write)
//   • Out             — model-generated tokens
//   • Cache read      — tokens served from prompt cache
//   • Cache write     — tokens folded into prompt cache (≈ "new content / turn")
//   • Hit %           — cache_read ÷ in
//   • Avg dur         — mean wall-clock duration per turn (incl. subagents)
//   • Cost            — total spend for this slice
//   • Share           — % of grand cost
//
// Rows arrive sorted by total_cost_usd DESC from the server.

function formatCost(cost: number): string {
  if (cost === 0) return "$0.00";
  if (cost < 0.01) return "<$0.01";
  if (cost < 1) return `$${cost.toFixed(3)}`;
  return `$${cost.toFixed(2)}`;
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

// Total prompt tokens the API processed for this slice — uncached
// (Anthropic `input_tokens`) + cache reads + cache writes. Mirrors
// `totalProcessedInput` in the KPI cards so the In column there and
// here agree.
function totalProcessedInput(r: UsageGroupRow): number {
  return r.inputTokens + r.cacheReadTokens + r.cacheWriteTokens;
}

function hitRatioPct(r: UsageGroupRow): number | null {
  const denom = totalProcessedInput(r);
  if (denom === 0) return null;
  return (r.cacheReadTokens / denom) * 100;
}

function inputCellTooltip(r: UsageGroupRow): string {
  const total = totalProcessedInput(r);
  return (
    `${formatCompact(total)} total processed input\n` +
    `  ${formatCompact(r.inputTokens)} uncached (1×)\n` +
    `  ${formatCompact(r.cacheReadTokens)} cache read (~0.1×)\n` +
    `  ${formatCompact(r.cacheWriteTokens)} cache write (~1.25×)`
  );
}

export function UsageBreakdownTable({
  title,
  rows,
  keyColumnLabel,
}: {
  title: string;
  rows: UsageGroupRow[];
  keyColumnLabel: string;
}) {
  const grandCost = rows.reduce((acc, r) => acc + r.totalCostUsd, 0);

  return (
    <div className="rounded-lg border border-border bg-background">
      <div className="border-b border-border px-4 py-3 text-sm font-medium">
        {title}
      </div>
      {rows.length === 0 ? (
        <div className="px-4 py-8 text-center text-sm text-muted-foreground">
          No activity in this range yet.
        </div>
      ) : (
        <div className="overflow-x-auto">
          <table className="w-full text-sm">
            <thead>
              <tr className="border-b border-border text-xs text-muted-foreground">
                <th className="px-3 py-2 text-left font-medium">
                  {keyColumnLabel}
                </th>
                <th className="px-3 py-2 text-right font-medium">Turns</th>
                <th
                  className="px-3 py-2 text-right font-medium"
                  title={
                    "Total prompt tokens the API processed for this slice " +
                    "(uncached + cache reads + cache writes). Hover any cell " +
                    "for the breakdown."
                  }
                >
                  In
                </th>
                <th
                  className="px-3 py-2 text-right font-medium"
                  title="Tokens the model generated."
                >
                  Out
                </th>
                <th
                  className="px-3 py-2 text-right font-medium"
                  title="Tokens served from the prompt cache (~0.1× input rate)."
                >
                  Cache R
                </th>
                <th
                  className="px-3 py-2 text-right font-medium"
                  title="Tokens written to the prompt cache (~1.25× input rate). The real 'new content per turn' signal."
                >
                  Cache W
                </th>
                <th
                  className="px-3 py-2 text-right font-medium"
                  title="cache_read ÷ in"
                >
                  Hit %
                </th>
                <th
                  className="px-3 py-2 text-right font-medium"
                  title="Mean wall-clock duration per turn, including subagents."
                >
                  Avg dur
                </th>
                <th className="px-3 py-2 text-right font-medium">Cost</th>
                <th className="px-3 py-2 text-right font-medium">Share</th>
              </tr>
            </thead>
            <tbody>
              {rows.map((r) => {
                const share =
                  grandCost > 0 ? (r.totalCostUsd / grandCost) * 100 : 0;
                const hit = hitRatioPct(r);
                const avgDur =
                  r.turnCount === 0
                    ? 0
                    : Math.round(r.totalDurationMs / r.turnCount);
                return (
                  <tr
                    key={r.key}
                    className="border-b border-border/50 last:border-b-0 hover:bg-muted/40"
                  >
                    <td className="px-3 py-2 font-medium">
                      {r.label}
                      {r.costHasUnknowns ? (
                        <span
                          title="Some turns had no reported cost"
                          className="ml-2 text-[10px] text-amber-600 dark:text-amber-400"
                        >
                          partial
                        </span>
                      ) : null}
                    </td>
                    <td className="px-3 py-2 text-right tabular-nums">
                      {r.turnCount}
                    </td>
                    <td
                      className="px-3 py-2 text-right tabular-nums text-muted-foreground"
                      title={inputCellTooltip(r)}
                    >
                      {formatCompact(totalProcessedInput(r))}
                    </td>
                    <td className="px-3 py-2 text-right tabular-nums text-muted-foreground">
                      {formatCompact(r.outputTokens)}
                    </td>
                    <td className="px-3 py-2 text-right tabular-nums text-muted-foreground">
                      {formatCompact(r.cacheReadTokens)}
                    </td>
                    <td className="px-3 py-2 text-right tabular-nums text-muted-foreground">
                      {formatCompact(r.cacheWriteTokens)}
                    </td>
                    <td className="px-3 py-2 text-right tabular-nums text-muted-foreground">
                      {hit === null ? "—" : `${Math.round(hit)}%`}
                    </td>
                    <td className="px-3 py-2 text-right tabular-nums text-muted-foreground">
                      {formatDuration(avgDur)}
                    </td>
                    <td className="px-3 py-2 text-right tabular-nums">
                      {formatCost(r.totalCostUsd)}
                    </td>
                    <td className="px-3 py-2 text-right tabular-nums text-muted-foreground">
                      {share.toFixed(1)}%
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>
      )}
    </div>
  );
}
