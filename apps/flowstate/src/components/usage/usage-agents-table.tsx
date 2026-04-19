import type { UsageAgentGroupRow } from "@/lib/api";

// Per-agent breakdown table. Shows the share of each agent role
// (main parent + each Task/Agent subagent type) in the selected
// range. Mirrors the provider / model tables column-for-column
// except for two differences:
//
//   • "Invocations" column replaces the duration / hit-% columns:
//     the dashboard tracks how many times each agent actually ran
//     (a turn with 3 Explore dispatches contributes 3 invocations
//     but 1 turn count) which is the single most useful signal
//     when debugging "why is Explore so expensive?".
//   • No Avg dur — per-agent wall clock isn't meaningful since
//     subagents run in parallel with the parent's tool loop.
//
// Data comes from `get_usage_by_agent`, which reads the
// `usage_event_agents` table in the flowstate usage sqlite. Cost is
// allocated at insert time proportionally to each agent's billable
// token weight, so the sum across rows reconciles exactly with the
// KPI-card total cost for the same range.

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

function totalProcessedInput(r: UsageAgentGroupRow): number {
  return r.inputTokens + r.cacheReadTokens + r.cacheWriteTokens;
}

function hitRatioPct(r: UsageAgentGroupRow): number | null {
  const denom = totalProcessedInput(r);
  if (denom === 0) return null;
  return (r.cacheReadTokens / denom) * 100;
}

function inputCellTooltip(r: UsageAgentGroupRow): string {
  const total = totalProcessedInput(r);
  return (
    `${formatCompact(total)} total processed input\n` +
    `  ${formatCompact(r.inputTokens)} uncached (1×)\n` +
    `  ${formatCompact(r.cacheReadTokens)} cache read (~0.1×)\n` +
    `  ${formatCompact(r.cacheWriteTokens)} cache write (~1.25×)`
  );
}

export function UsageAgentsTable({
  rows,
}: {
  rows: UsageAgentGroupRow[];
}) {
  const grandCost = rows.reduce((acc, r) => acc + r.totalCostUsd, 0);

  return (
    <div className="rounded-lg border border-border bg-background">
      <div className="border-b border-border px-4 py-3 text-sm font-medium">
        By agent
        <span
          className="ml-2 text-xs font-normal text-muted-foreground"
          title={
            "Breaks the range's spend down across the parent agent (‘Main’) " +
            "and each Task/Agent subagent the SDK dispatched. Cost is allocated " +
            "proportionally to per-agent token weight, so the sum reconciles to " +
            "the top-line cost card."
          }
        >
          main vs. subagents
        </span>
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
                <th className="px-3 py-2 text-left font-medium">Agent</th>
                <th
                  className="px-3 py-2 text-right font-medium"
                  title="Turns where this agent role did at least one unit of work."
                >
                  Turns
                </th>
                <th
                  className="px-3 py-2 text-right font-medium"
                  title={
                    "Individual agent invocations. A turn dispatching two Explore " +
                    "subagents contributes 2 here and 1 to Turns."
                  }
                >
                  Runs
                </th>
                <th
                  className="px-3 py-2 text-right font-medium"
                  title={
                    "Total prompt tokens the API processed for this agent " +
                    "(uncached + cache reads + cache writes). Hover any cell " +
                    "for the breakdown."
                  }
                >
                  In
                </th>
                <th
                  className="px-3 py-2 text-right font-medium"
                  title="Tokens the model generated for this agent."
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
                  title="Tokens written to the prompt cache (~1.25× input rate)."
                >
                  Cache W
                </th>
                <th
                  className="px-3 py-2 text-right font-medium"
                  title="cache_read ÷ in"
                >
                  Hit %
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
                    <td className="px-3 py-2 text-right tabular-nums">
                      {r.invocationCount}
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
