import { useQuery } from "@tanstack/react-query";
import {
  getTopSessions,
  getUsageSummary,
  getUsageTimeseries,
  type TopSessionRow,
  type UsageBucket,
  type UsageGroupBy,
  type UsageRange,
  type UsageSummaryPayload,
  type UsageTimeseriesPayload,
} from "@/lib/api";

// TanStack Query wrappers around the three analytics Tauri
// commands. All three are cheap SELECTs (thousands of rows,
// indexed by day) — we use a short stale time so switching
// ranges re-fetches promptly, but a modest cache time so
// flicking back to the Usage tab doesn't re-hit the store.

const STALE_MS = 30_000;
const CACHE_MS = 5 * 60_000;

export function useUsageSummary(
  range: UsageRange,
  groupBy: UsageGroupBy = "by_provider",
) {
  return useQuery<UsageSummaryPayload>({
    queryKey: ["usage", "summary", range, groupBy],
    queryFn: () => getUsageSummary(range, groupBy),
    staleTime: STALE_MS,
    gcTime: CACHE_MS,
  });
}

export function useUsageTimeseries(
  range: UsageRange,
  bucket: UsageBucket = "daily",
  splitBy?: UsageGroupBy,
) {
  return useQuery<UsageTimeseriesPayload>({
    queryKey: ["usage", "timeseries", range, bucket, splitBy ?? "none"],
    queryFn: () => getUsageTimeseries(range, bucket, splitBy),
    staleTime: STALE_MS,
    gcTime: CACHE_MS,
  });
}

export function useTopSessions(range: UsageRange, limit: number = 10) {
  return useQuery<TopSessionRow[]>({
    queryKey: ["usage", "topSessions", range, limit],
    queryFn: () => getTopSessions(range, limit),
    staleTime: STALE_MS,
    gcTime: CACHE_MS,
  });
}
