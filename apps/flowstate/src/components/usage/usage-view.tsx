import { SidebarTrigger, useSidebar } from "@/components/ui/sidebar";
import { isMacOS } from "@/lib/popout";
import { UsageKpiCards } from "./usage-kpi-cards";
import { UsageCostChart } from "./usage-cost-chart";
import { UsageTokensChart } from "./usage-tokens-chart";
import { UsageBreakdownTable } from "./usage-breakdown-table";
import { UsageAgentsTable } from "./usage-agents-table";
import { UsageAgentRoleTable } from "./usage-agent-role-table";
import { UsageTopSessionsTable } from "./usage-top-sessions-table";
import { UsageRangePicker, useUsageRange } from "./usage-range-picker";
import {
  useTopSessions,
  useUsageByAgent,
  useUsageByAgentRole,
  useUsageSummary,
  useUsageTimeseries,
} from "./hooks/use-usage";

// Usage page. Five KPI cards (spend / turns / tokens / cache /
// avg duration) across the top, two charts side by side (cost
// over time, tokens over time with a by-kind ↔ by-model toggle),
// then three tables stacked below. Reads from the
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
  const { state: sidebarState } = useSidebar();
  const showMacTrafficSpacer = isMacOS() && sidebarState === "collapsed";

  const summaryQuery = useUsageSummary(range, "by_provider");
  const modelsQuery = useUsageSummary(range, "by_model");
  // Cost chart stays split by provider (its colors come from the
  // provider table). The tokens chart needs its own query so it
  // can offer a per-model split alongside the by-kind view —
  // see UsageTokensChart for the toggle.
  const costTimeseriesQuery = useUsageTimeseries(range, "daily", "by_provider");
  const tokensTimeseriesQuery = useUsageTimeseries(range, "daily", "by_model");
  const topSessionsQuery = useTopSessions(range, 10);
  // Per-agent breakdown (Main + each subagent role). Distinct query
  // from summary so a refetch doesn't invalidate the top-line cards.
  const agentsQuery = useUsageByAgent(range);
  // Main-vs-subagents two-row rollup. Separate cache key from
  // `agentsQuery` so the two tables refetch independently.
  const agentRoleQuery = useUsageByAgentRole(range);

  // First-load gating: show the big "no usage yet" message only
  // when every query has resolved and the grand totals are empty.
  // Intermediate states show their own inline placeholders.
  const summary = summaryQuery.data;
  const isEmpty =
    summary !== undefined &&
    summary.totals.turnCount === 0 &&
    costTimeseriesQuery.data !== undefined;

  return (
    <div className="flex h-full flex-col">
      <header
        data-tauri-drag-region
        className="flex h-9 items-center gap-1 border-b border-border px-2"
      >
        {showMacTrafficSpacer && (
          <div className="w-16 shrink-0" data-tauri-drag-region />
        )}
        <SidebarTrigger />
        <div className="flex-1 text-sm font-medium">Usage</div>
        <div data-tauri-drag-region={false}>
          <UsageRangePicker value={range} onChange={setRange} />
        </div>
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
              <UsageKpiCards
                totals={summary.totals}
                modelGroups={modelsQuery.data?.groups}
              />

              <div className="grid grid-cols-1 gap-4 xl:grid-cols-2">
                {costTimeseriesQuery.data ? (
                  <UsageCostChart data={costTimeseriesQuery.data} />
                ) : (
                  <LoadingState />
                )}
                {tokensTimeseriesQuery.data ? (
                  <UsageTokensChart data={tokensTimeseriesQuery.data} />
                ) : (
                  <LoadingState />
                )}
              </div>

              {/* Tables stack vertically — each has 10 columns
                  (Turns / In / Out / Cache R / Cache W / Hit / Avg
                  dur / Cost / Share) and squeezing two side-by-side
                  forces unreadable horizontal scrolling at typical
                  desktop widths. */}
              <div className="space-y-4">
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
                <UsageAgentRoleTable
                  rows={agentRoleQuery.data?.groups ?? []}
                />
                <UsageAgentsTable rows={agentsQuery.data?.groups ?? []} />
              </div>

              {topSessionsQuery.data ? (
                <UsageTopSessionsTable rows={topSessionsQuery.data} />
              ) : null}

              {/* Pricing-data freshness disclosure. The per-agent
                  cost split in the tables above relies on a
                  hardcoded Anthropic rate table on the Rust side
                  (see crates/app-layer/src/usage.rs::RATES_*). When
                  Anthropic changes pricing the absolute totals stay
                  correct (they're the provider's own numbers), but
                  the per-agent SHARE drifts until the table is
                  updated. Surfacing the verification date lets
                  attentive users cross-check rather than assume. */}
              <div className="pt-2 text-center text-[11px] text-muted-foreground/70">
                Per-agent cost allocation uses Anthropic public rates
                verified{" "}
                <span className="tabular-nums">
                  {summary.pricingTableDate}
                </span>
                .{" "}
                <a
                  href="https://www.anthropic.com/pricing"
                  target="_blank"
                  rel="noopener noreferrer"
                  className="underline hover:text-foreground"
                >
                  Verify current rates
                </a>
                .
              </div>
            </>
          ) : null}
        </div>
      </div>
    </div>
  );
}
