import * as React from "react";
import {
  Bar,
  BarChart,
  CartesianGrid,
  Legend,
  ResponsiveContainer,
  Tooltip,
  XAxis,
  YAxis,
} from "recharts";
import type { UsageTimeseriesPayload } from "@/lib/api";

// Stacked bar chart of daily token volume broken down into the
// four token "kinds" the provider-api reports: plain input,
// output, cache reads (free/cheap), and cache writes. Cache reads
// are intentionally rendered separately so a heavy-cache day
// doesn't disguise itself as a heavy-input day.

const TOKEN_COLORS = {
  input: "#3b82f6",
  output: "#10b981",
  cacheRead: "#64748b",
  cacheWrite: "#f59e0b",
} as const;

const TOKEN_LABELS: Record<keyof typeof TOKEN_COLORS, string> = {
  input: "Input",
  output: "Output",
  cacheRead: "Cache read",
  cacheWrite: "Cache write",
};

function formatDay(day: string): string {
  const date = new Date(`${day}T00:00:00Z`);
  return date.toLocaleDateString(undefined, {
    month: "short",
    day: "numeric",
  });
}

function formatTokens(n: number): string {
  if (n < 1_000) return n.toString();
  if (n < 1_000_000) return `${(n / 1_000).toFixed(1)}k`;
  return `${(n / 1_000_000).toFixed(1)}m`;
}

export function UsageTokensChart({
  data,
}: {
  data: UsageTimeseriesPayload;
}) {
  const rows = React.useMemo(
    () =>
      data.points.map((p) => ({
        day: p.bucketStart,
        input: p.totals.inputTokens,
        output: p.totals.outputTokens,
        cacheRead: p.totals.cacheReadTokens,
        cacheWrite: p.totals.cacheWriteTokens,
      })),
    [data.points],
  );

  return (
    <div className="rounded-lg border border-border bg-background p-4">
      <div className="mb-3 text-sm font-medium">Tokens over time</div>
      <div className="h-64 w-full">
        <ResponsiveContainer width="100%" height="100%">
          <BarChart data={rows} margin={{ top: 8, right: 8, left: 0, bottom: 0 }}>
            <CartesianGrid strokeDasharray="3 3" className="stroke-muted" />
            <XAxis
              dataKey="day"
              tickFormatter={formatDay}
              tick={{ fontSize: 11 }}
              minTickGap={24}
            />
            <YAxis
              tickFormatter={formatTokens}
              tick={{ fontSize: 11 }}
              width={56}
            />
            <Tooltip
              labelFormatter={(label) => formatDay(String(label))}
              formatter={(value, name) => [
                formatTokens(Number(value ?? 0)),
                TOKEN_LABELS[String(name) as keyof typeof TOKEN_LABELS] ??
                  String(name),
              ]}
              contentStyle={{
                background: "hsl(var(--background))",
                border: "1px solid hsl(var(--border))",
                borderRadius: 8,
                fontSize: 12,
              }}
            />
            <Legend
              wrapperStyle={{ fontSize: 11 }}
              formatter={(value) =>
                TOKEN_LABELS[value as keyof typeof TOKEN_LABELS] ?? value
              }
            />
            <Bar
              dataKey="input"
              stackId="t"
              fill={TOKEN_COLORS.input}
              isAnimationActive={false}
            />
            <Bar
              dataKey="output"
              stackId="t"
              fill={TOKEN_COLORS.output}
              isAnimationActive={false}
            />
            <Bar
              dataKey="cacheRead"
              stackId="t"
              fill={TOKEN_COLORS.cacheRead}
              isAnimationActive={false}
            />
            <Bar
              dataKey="cacheWrite"
              stackId="t"
              fill={TOKEN_COLORS.cacheWrite}
              isAnimationActive={false}
            />
          </BarChart>
        </ResponsiveContainer>
      </div>
    </div>
  );
}
