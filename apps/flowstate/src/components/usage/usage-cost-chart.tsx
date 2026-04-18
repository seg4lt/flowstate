import * as React from "react";
import {
  Area,
  AreaChart,
  CartesianGrid,
  ResponsiveContainer,
  Tooltip,
  XAxis,
  YAxis,
} from "recharts";
import type { UsageTimeseriesPayload } from "@/lib/api";

// Stacked area chart of daily cost, one stack per provider (or
// model, when `series` carries a split). When no split is
// provided, renders a single "total" area.

const PROVIDER_COLORS: Record<string, string> = {
  claude: "#f59e0b",
  claude_cli: "#a855f7",
  codex: "#10b981",
  github_copilot: "#3b82f6",
  github_copilot_cli: "#06b6d4",
};

// Stable palette for model splits (fallback for anything not in
// the provider table). Visually distinct without being loud.
const FALLBACK_PALETTE = [
  "#f59e0b",
  "#3b82f6",
  "#10b981",
  "#a855f7",
  "#ef4444",
  "#06b6d4",
  "#f97316",
  "#8b5cf6",
];

function colorForKey(key: string, index: number): string {
  return PROVIDER_COLORS[key] ?? FALLBACK_PALETTE[index % FALLBACK_PALETTE.length];
}

function formatDay(day: string): string {
  const date = new Date(`${day}T00:00:00Z`);
  return date.toLocaleDateString(undefined, {
    month: "short",
    day: "numeric",
  });
}

function formatCost(cost: number): string {
  if (cost === 0) return "$0";
  if (cost < 0.01) return "<$0.01";
  return `$${cost.toFixed(2)}`;
}

export function UsageCostChart({
  data,
}: {
  data: UsageTimeseriesPayload;
}) {
  const rows = React.useMemo(() => {
    if (data.series.length === 0) {
      return data.points.map((p) => ({
        day: p.bucketStart,
        total: p.totals.totalCostUsd,
      }));
    }
    // When split by key: one row per day, one field per key.
    const byDay: Record<string, Record<string, number>> = {};
    for (const s of data.series) {
      for (const p of s.points) {
        if (!byDay[p.bucketStart]) byDay[p.bucketStart] = {};
        byDay[p.bucketStart][s.key] = p.totals.totalCostUsd;
      }
    }
    return data.points.map((p) => ({
      day: p.bucketStart,
      ...(byDay[p.bucketStart] ?? {}),
    }));
  }, [data]);

  const keys = React.useMemo(() => {
    if (data.series.length === 0) return ["total"];
    return data.series.map((s) => s.key);
  }, [data.series]);

  const labelByKey = React.useMemo(() => {
    const map: Record<string, string> = { total: "Total" };
    for (const s of data.series) map[s.key] = s.label;
    return map;
  }, [data.series]);

  return (
    <div className="rounded-lg border border-border bg-background p-4">
      <div className="mb-3 text-sm font-medium">Cost over time</div>
      <div className="h-64 w-full">
        <ResponsiveContainer width="100%" height="100%">
          <AreaChart data={rows} margin={{ top: 8, right: 8, left: 0, bottom: 0 }}>
            <CartesianGrid strokeDasharray="3 3" className="stroke-muted" />
            <XAxis
              dataKey="day"
              tickFormatter={formatDay}
              tick={{ fontSize: 11 }}
              minTickGap={24}
            />
            <YAxis
              tickFormatter={(v: number) => `$${v.toFixed(2)}`}
              tick={{ fontSize: 11 }}
              width={56}
            />
            <Tooltip
              labelFormatter={(label) => formatDay(String(label))}
              formatter={(value, name) => [
                formatCost(Number(value ?? 0)),
                labelByKey[String(name)] ?? String(name),
              ]}
              contentStyle={{
                background: "hsl(var(--background))",
                border: "1px solid hsl(var(--border))",
                borderRadius: 8,
                fontSize: 12,
              }}
            />
            {keys.map((key, index) => (
              <Area
                key={key}
                type="monotone"
                dataKey={key}
                stackId="1"
                stroke={colorForKey(key, index)}
                fill={colorForKey(key, index)}
                fillOpacity={0.35}
                strokeWidth={1.5}
                isAnimationActive={false}
              />
            ))}
          </AreaChart>
        </ResponsiveContainer>
      </div>
    </div>
  );
}
