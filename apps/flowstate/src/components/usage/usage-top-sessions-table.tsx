import * as React from "react";
import { useNavigate } from "@tanstack/react-router";
import type { TopSessionRow } from "@/lib/api";
import { listSessionDisplay, type SessionDisplay } from "@/lib/api";

// Top 10 sessions by cost. Clicking a row navigates to the
// matching chat. Session titles come from the flowstate-app store
// (`session_display`) — the analytics payload deliberately only
// carries session ids, per the persistence boundary rules (titles
// are display-only and live on the app side).

function formatCost(cost: number): string {
  if (cost === 0) return "$0.00";
  if (cost < 0.01) return "<$0.01";
  return `$${cost.toFixed(2)}`;
}

function formatWhen(rfc3339: string): string {
  try {
    const date = new Date(rfc3339);
    return date.toLocaleDateString(undefined, {
      month: "short",
      day: "numeric",
      hour: "numeric",
      minute: "2-digit",
    });
  } catch {
    return rfc3339;
  }
}

function shortId(id: string): string {
  // Session ids are opaque; show the last 6 chars if we have no
  // title to lean on.
  return id.length > 6 ? `…${id.slice(-6)}` : id;
}

function useSessionTitles() {
  const [titles, setTitles] = React.useState<Record<string, SessionDisplay>>(
    {},
  );
  React.useEffect(() => {
    let cancelled = false;
    listSessionDisplay()
      .then((map) => {
        if (!cancelled) setTitles(map);
      })
      .catch(() => {
        // Non-fatal: we fall back to the short id. No toast — this
        // would fire on every /usage mount and the user already
        // sees the shortened id.
      });
    return () => {
      cancelled = true;
    };
  }, []);
  return titles;
}

export function UsageTopSessionsTable({ rows }: { rows: TopSessionRow[] }) {
  const navigate = useNavigate();
  const titles = useSessionTitles();

  return (
    <div className="rounded-lg border border-border bg-background">
      <div className="border-b border-border px-4 py-3 text-sm font-medium">
        Top sessions
      </div>
      {rows.length === 0 ? (
        <div className="px-4 py-8 text-center text-sm text-muted-foreground">
          No sessions in this range yet.
        </div>
      ) : (
        <div className="overflow-x-auto">
          <table className="w-full text-sm">
            <thead>
              <tr className="border-b border-border text-xs text-muted-foreground">
                <th className="px-4 py-2 text-left font-medium">Session</th>
                <th className="px-4 py-2 text-left font-medium">Provider</th>
                <th className="px-4 py-2 text-left font-medium">Model</th>
                <th className="px-4 py-2 text-right font-medium">Turns</th>
                <th className="px-4 py-2 text-right font-medium">Cost</th>
                <th className="px-4 py-2 text-right font-medium">Last</th>
              </tr>
            </thead>
            <tbody>
              {rows.map((r) => {
                const title =
                  titles[r.sessionId]?.title ?? null;
                return (
                  <tr
                    key={r.sessionId}
                    className="cursor-pointer border-b border-border/50 last:border-b-0 hover:bg-muted/40"
                    onClick={() =>
                      navigate({
                        to: "/chat/$sessionId",
                        params: { sessionId: r.sessionId },
                      })
                    }
                  >
                    <td className="max-w-[240px] truncate px-4 py-2 font-medium">
                      {title ?? shortId(r.sessionId)}
                      {r.costHasUnknowns ? (
                        <span
                          title="Some turns had no reported cost"
                          className="ml-2 text-[10px] text-amber-600 dark:text-amber-400"
                        >
                          partial
                        </span>
                      ) : null}
                    </td>
                    <td className="px-4 py-2 text-muted-foreground">
                      {r.providerLabel}
                    </td>
                    <td className="max-w-[200px] truncate px-4 py-2 text-muted-foreground">
                      {r.model ?? "—"}
                    </td>
                    <td className="px-4 py-2 text-right tabular-nums">
                      {r.turnCount}
                    </td>
                    <td className="px-4 py-2 text-right tabular-nums">
                      {formatCost(r.totalCostUsd)}
                    </td>
                    <td className="px-4 py-2 text-right tabular-nums text-muted-foreground">
                      {formatWhen(r.lastActivityAt)}
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
