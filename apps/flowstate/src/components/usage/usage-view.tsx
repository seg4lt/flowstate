import { SidebarTrigger } from "@/components/ui/sidebar";
import { UsageKpiCards } from "./usage-kpi-cards";
import { UsageCostChart } from "./usage-cost-chart";
import { UsageTokensChart } from "./usage-tokens-chart";
import { UsageBreakdownTable } from "./usage-breakdown-table";
import { UsageTopSessionsTable } from "./usage-top-sessions-table";
import { UsageRangePicker, useUsageRange } from "./usage-range-picker";
import {
  useTopSessions,
  useUsageSummary,
  useUsageTimeseries,
} from "./hooks/use-usage";

// Usage page. Four KPI cards across the top, two charts side by
// side, then three tables stacked below. Reads from the
// flowstate-app-owned `usage.sqlite` via the three Tauri commands
// registered in `src-tauri/src/lib.rs`.

function EmptyState() {
  return (
    <div className="rounded-lg border border-dashed border-border bg-background p-12 text-center">
      <div className="text-sm font-medium">No usage recorded yet</div>
      <div className="mt-2 text-sm text-muted-foreground">
        Usage tracking begins with your next turn on any provider.
      </div>
    </div>
  );
}

function LoadingState() {
  return (
    <div className="flex items-center justify-center p-12 text-sm text-muted-foreground">
      Loading usage…
    </div>
  );
}

function ErrorState({ error }: { error: unknown }) {
  const message =
    error instanceof Error ? error.message : String(error ?? "unknown error");
  return (
    <div className="rounded-lg border border-destructive/30 bg-destructive/5 p-4 text-sm text-destructive">
      Failed to load usage: {message}
    </div>
  );
}

export function UsageView() {
  const [range, setRange] = useUsageRange();

  const summaryQuery = useUsageSummary(range, "by_provider");
  const modelsQuery = useUsageSummary(range, "by_model");
  const timeseriesQuery = useUsageTimeseries(range, "daily", "by_provider");
  const topSessionsQuery = useTopSessions(range, 10);

  // First-load gating: show the big "no usage yet" message only
  // when every query has resolved and the grand totals are empty.
  // Intermediate states show their own inline placeholders.
  const summary = summaryQuery.data;
  const isEmpty =
    summary !== undefined &&
    summary.totals.turnCount === 0 &&
    timeseriesQuery.data !== undefined;

  return (
    <div className="flex h-svh flex-col">
      <header className="flex h-12 items-center gap-2 border-b border-border px-2">
        <SidebarTrigger />
        <div className="flex-1 text-sm font-medium">Usage</div>
        <UsageRangePicker value={range} onChange={setRange} />
      </header>

      <div className="flex-1 overflow-auto">
        <div className="mx-auto max-w-6xl space-y-4 p-4">
          {summaryQuery.isLoading ? (
            <LoadingState />
          ) : summaryQuery.isError ? (
            <ErrorState error={summaryQuery.error} />
          ) : isEmpty ? (
            <EmptyState />
          ) : summary ? (
            <>
              <UsageKpiCards totals={summary.totals} />

              <div className="grid grid-cols-1 gap-4 xl:grid-cols-2">
                {timeseriesQuery.data ? (
                  <UsageCostChart data={timeseriesQuery.data} />
                ) : (
                  <LoadingState />
                )}
                {timeseriesQuery.data ? (
                  <UsageTokensChart data={timeseriesQuery.data} />
                ) : (
                  <LoadingState />
                )}
              </div>

              <div className="grid grid-cols-1 gap-4 xl:grid-cols-2">
                <UsageBreakdownTable
                  title="By provider"
                  keyColumnLabel="Provider"
                  rows={summary.byProvider}
                />
                <UsageBreakdownTable
                  title="By model"
                  keyColumnLabel="Model"
                  rows={modelsQuery.data?.groups ?? []}
                />
              </div>

              {topSessionsQuery.data ? (
                <UsageTopSessionsTable rows={topSessionsQuery.data} />
              ) : null}
            </>
          ) : null}
        </div>
      </div>
    </div>
  );
}
