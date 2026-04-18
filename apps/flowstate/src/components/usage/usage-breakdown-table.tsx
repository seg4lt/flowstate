import type { UsageGroupRow } from "@/lib/api";

// Breakdown table used for both provider and model splits. The
// design mirrors the chat view's other tabular surfaces — plain
// tailwind, no extra shadcn primitive (the project doesn't ship
// one). Rows are sorted server-side by total_cost_usd DESC.

function formatCost(cost: number): string {
  if (cost === 0) return "$0.00";
  if (cost < 0.01) return "<$0.01";
  return `$${cost.toFixed(2)}`;
}

function formatCompact(n: number): string {
  if (n < 1_000) return n.toString();
  if (n < 1_000_000) return `${(n / 1_000).toFixed(1)}k`;
  return `${(n / 1_000_000).toFixed(1)}m`;
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
                <th className="px-4 py-2 text-left font-medium">
                  {keyColumnLabel}
                </th>
                <th className="px-4 py-2 text-right font-medium">Turns</th>
                <th className="px-4 py-2 text-right font-medium">In</th>
                <th className="px-4 py-2 text-right font-medium">Out</th>
                <th className="px-4 py-2 text-right font-medium">Cost</th>
                <th className="px-4 py-2 text-right font-medium">Share</th>
              </tr>
            </thead>
            <tbody>
              {rows.map((r) => {
                const share =
                  grandCost > 0 ? (r.totalCostUsd / grandCost) * 100 : 0;
                return (
                  <tr
                    key={r.key}
                    className="border-b border-border/50 last:border-b-0 hover:bg-muted/40"
                  >
                    <td className="px-4 py-2 font-medium">
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
                    <td className="px-4 py-2 text-right tabular-nums">
                      {r.turnCount}
                    </td>
                    <td className="px-4 py-2 text-right tabular-nums text-muted-foreground">
                      {formatCompact(r.inputTokens)}
                    </td>
                    <td className="px-4 py-2 text-right tabular-nums text-muted-foreground">
                      {formatCompact(r.outputTokens)}
                    </td>
                    <td className="px-4 py-2 text-right tabular-nums">
                      {formatCost(r.totalCostUsd)}
                    </td>
                    <td className="px-4 py-2 text-right tabular-nums text-muted-foreground">
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
