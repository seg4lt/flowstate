import { invoke } from "@tauri-apps/api/core";

// Usage analytics — reads of the flowstate-app-owned
// `<app_data_dir>/usage.sqlite`. Writes happen on the Rust side
// via a subscriber task on `RuntimeEvent::TurnCompleted`; the
// frontend only reads aggregates. The SDK's daemon database is
// never touched by these queries — analytics are display-only
// and live entirely in the app's store. See
// `src-tauri/src/usage.rs` for schema and boundary rationale.

// Mirrors `flowstate_app_layer::usage::UsageRange` (serde
// snake_case, externally-tagged enum). Presets are bare strings;
// the custom variant carries `from`/`to` as `YYYY-MM-DD` UTC day
// strings — the dashboard's date input only resolves to whole
// days, and the SQL filter is `>= from AND <= to` on day strings,
// so the user's selected range fully covers from the start of `from`
// (00:00:00) through the end of `to` (23:59:59) without ever sending
// a time-of-day value over the wire.
export type UsageRange =
  | "last7_days"
  | "last30_days"
  | "last90_days"
  | "last120_days"
  | "last180_days"
  | "all_time"
  | { custom: { from: string; to: string } };

export function isCustomRange(
  r: UsageRange,
): r is { custom: { from: string; to: string } } {
  return typeof r === "object" && r !== null && "custom" in r;
}

/// Construct a Custom range from two `YYYY-MM-DD` day strings. The
/// caller is responsible for normalisation upstream (the date input
/// emits the canonical form already); the Rust side validates and
/// re-formats anyway, so this is a pure type wrapper.
export function customRange(from: string, to: string): UsageRange {
  return { custom: { from, to } };
}

export type UsageGroupBy = "by_provider" | "by_model";

export type UsageBucket = "daily";

export interface UsageTotals {
  turnCount: number;
  inputTokens: number;
  outputTokens: number;
  cacheReadTokens: number;
  cacheWriteTokens: number;
  totalCostUsd: number;
  costHasUnknowns: boolean;
  totalDurationMs: number;
  distinctSessions: number;
  distinctModels: number;
}

export interface UsageGroupRow {
  key: string;
  label: string;
  turnCount: number;
  inputTokens: number;
  outputTokens: number;
  cacheReadTokens: number;
  cacheWriteTokens: number;
  totalCostUsd: number;
  costHasUnknowns: boolean;
  totalDurationMs: number;
}

export interface UsageSummaryPayload {
  range: UsageRange;
  totals: UsageTotals;
  byProvider: UsageGroupRow[];
  groups: UsageGroupRow[];
  generatedAt: string;
}

export interface UsageTimeseriesPoint {
  bucketStart: string;
  totals: UsageTotals;
}

export interface UsageSeries {
  key: string;
  label: string;
  points: UsageTimeseriesPoint[];
}

export interface UsageTimeseriesPayload {
  range: UsageRange;
  bucket: UsageBucket;
  points: UsageTimeseriesPoint[];
  series: UsageSeries[];
  generatedAt: string;
}

export interface TopSessionRow {
  sessionId: string;
  provider: string;
  providerLabel: string;
  model: string | null;
  projectId: string | null;
  turnCount: number;
  totalCostUsd: number;
  costHasUnknowns: boolean;
  lastActivityAt: string;
}

export function getUsageSummary(
  range: UsageRange,
  groupBy: UsageGroupBy = "by_provider",
): Promise<UsageSummaryPayload> {
  return invoke<UsageSummaryPayload>("get_usage_summary", { range, groupBy });
}

export function getUsageTimeseries(
  range: UsageRange,
  bucket: UsageBucket = "daily",
  splitBy?: UsageGroupBy,
): Promise<UsageTimeseriesPayload> {
  return invoke<UsageTimeseriesPayload>("get_usage_timeseries", {
    range,
    bucket,
    splitBy: splitBy ?? null,
  });
}

export function getTopSessions(
  range: UsageRange,
  limit: number = 10,
): Promise<TopSessionRow[]> {
  return invoke<TopSessionRow[]>("get_top_sessions", { range, limit });
}

// Per-agent dashboard breakdown. Main (parent) agent is keyed as
// "main"; sub-agents carry their catalog type ("Explore",
// "general-purpose", ...). Cost is pre-allocated at insert time so
// the sum across rows matches the parent turn's `total_cost_usd`.
export interface UsageAgentGroupRow {
  key: string;
  label: string;
  turnCount: number;
  invocationCount: number;
  inputTokens: number;
  outputTokens: number;
  cacheReadTokens: number;
  cacheWriteTokens: number;
  totalCostUsd: number;
  costHasUnknowns: boolean;
}

export interface UsageAgentPayload {
  range: UsageRange;
  groups: UsageAgentGroupRow[];
  generatedAt: string;
}

export function getUsageByAgent(
  range: UsageRange,
): Promise<UsageAgentPayload> {
  return invoke<UsageAgentPayload>("get_usage_by_agent", { range });
}

// Main-vs-Subagents rollup. Same payload shape, but the SQL CASE
// collapses every non-NULL `agent_type` into a single `"subagent"`
// row so the dashboard can answer the binary "how much is going to
// subagent dispatches?" question without scanning the detailed table.
export function getUsageByAgentRole(
  range: UsageRange,
): Promise<UsageAgentPayload> {
  return invoke<UsageAgentPayload>("get_usage_by_agent_role", { range });
}
