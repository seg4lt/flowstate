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
import { cn } from "@/lib/utils";

// Stacked bar chart of daily token volume. Two views:
//
//   • "by kind" — four-segment stack per day: input, output, cache
//     read, cache write. Useful for spotting heavy-cache vs
//     heavy-input days.
//   • "by model" — one stack per day with one segment per model,
//     showing total tokens (input + output + cache_read +
//     cache_write) so you can see which models are doing the work.
//
// The view-mode toggle sits in the chart header so both shapes
// stay one click apart.

type ViewMode = "by_kind" | "by_model";

const TOKEN_COLORS = {
  input: "#3b82f6",
  output: "#10b981",
  cacheRead: "#64748b",
  cacheWrite: "#f59e0b",
} as const;

// Label for the `input` segment is intentionally "Uncached" rather
// than "New input": with default Agent SDK cache_control, Anthropic's
// `input_tokens` field counts only the trailing scaffold bytes after
// the last cache breakpoint — typically 1–100 tokens per turn. The
// user's actually-new content lands in cache_creation_input_tokens
// (= "Cache write" segment), so calling this slice "new" misleads.
const TOKEN_LABELS: Record<keyof typeof TOKEN_COLORS, string> = {
  input: "Uncached",
  output: "Output",
  cacheRead: "Cache read",
  cacheWrite: "Cache write",
};

// Stable palette for per-model series. Picked to be visually
// distinct without dominating the chart; falls back by index when
// more models than colors are present.
const MODEL_PALETTE = [
  "#3b82f6",
  "#10b981",
  "#f59e0b",
  "#a855f7",
  "#ef4444",
  "#06b6d4",
  "#f97316",
  "#8b5cf6",
];

function colorForIndex(i: number): string {
  return MODEL_PALETTE[i % MODEL_PALETTE.length];
}

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

function ViewToggle({
  mode,
  onChange,
}: {
  mode: ViewMode;
  onChange: (next: ViewMode) => void;
}) {
  const opts: { value: ViewMode; label: string }[] = [
    { value: "by_kind", label: "By kind" },
    { value: "by_model", label: "By model" },
  ];
  return (
    <div className="inline-flex rounded-md border border-border bg-background p-0.5 text-xs">
      {opts.map((o) => (
        <button
          key={o.value}
          type="button"
          onClick={() => onChange(o.value)}
          className={cn(
            "rounded px-2 py-0.5 transition-colors",
            mode === o.value
              ? "bg-muted text-foreground"
              : "text-muted-foreground hover:text-foreground",
          )}
        >
          {o.label}
        </button>
      ))}
    </div>
  );
}

export function UsageTokensChart({
  data,
}: {
  data: UsageTimeseriesPayload;
}) {
  const [mode, setMode] = React.useState<ViewMode>("by_kind");

  // The "by model" view needs `data.series` to be populated. If
  // the caller fetched without a split, fall back to "by kind"
  // automatically — better than rendering an empty chart.
  const hasModelSeries = data.series.length > 0;
  const effectiveMode: ViewMode = hasModelSeries ? mode : "by_kind";

  const byKindRows = React.useMemo(
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

  const { byModelRows, modelKeys, modelLabelByKey } = React.useMemo(() => {
    // Per-model: one row per day, one numeric field per model. Sum
    // every token kind for that model on that day so the bar
    // height represents total token volume — same scale as the
    // by-kind view.
    const labels: Record<string, string> = {};
    const byDay: Record<string, Record<string, number>> = {};
    for (const s of data.series) {
      labels[s.key] = s.label;
      for (const p of s.points) {
        const total =
          p.totals.inputTokens +
          p.totals.outputTokens +
          p.totals.cacheReadTokens +
          p.totals.cacheWriteTokens;
        if (total === 0) continue;
        if (!byDay[p.bucketStart]) byDay[p.bucketStart] = {};
        byDay[p.bucketStart][s.key] = total;
      }
    }
    const rows = data.points.map((p) => ({
      day: p.bucketStart,
      ...(byDay[p.bucketStart] ?? {}),
    }));
    return {
      byModelRows: rows,
      modelKeys: data.series.map((s) => s.key),
      modelLabelByKey: labels,
    };
  }, [data]);

  const rows = effectiveMode === "by_kind" ? byKindRows : byModelRows;

  return (
    <div className="rounded-lg border border-border bg-background p-4">
      <div className="mb-3 flex items-center justify-between gap-2">
        <div className="text-sm font-medium">Tokens over time</div>
        {hasModelSeries ? (
          <ViewToggle mode={effectiveMode} onChange={setMode} />
        ) : null}
      </div>
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
              formatter={(value, name) => {
                const raw = String(name);
                const label =
                  effectiveMode === "by_kind"
                    ? TOKEN_LABELS[raw as keyof typeof TOKEN_LABELS] ?? raw
                    : modelLabelByKey[raw] ?? raw;
                return [formatTokens(Number(value ?? 0)), label];
              }}
              contentStyle={{
                background: "hsl(var(--background))",
                border: "1px solid hsl(var(--border))",
                borderRadius: 8,
                fontSize: 12,
              }}
            />
            <Legend
              wrapperStyle={{ fontSize: 11 }}
              formatter={(value) => {
                const raw = String(value);
                return effectiveMode === "by_kind"
                  ? TOKEN_LABELS[raw as keyof typeof TOKEN_LABELS] ?? raw
                  : modelLabelByKey[raw] ?? raw;
              }}
            />
            {effectiveMode === "by_kind" ? (
              <>
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
              </>
            ) : (
              modelKeys.map((key, i) => (
                <Bar
                  key={key}
                  dataKey={key}
                  stackId="t"
                  fill={colorForIndex(i)}
                  isAnimationActive={false}
                />
              ))
            )}
          </BarChart>
        </ResponsiveContainer>
      </div>
    </div>
  );
}
