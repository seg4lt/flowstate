import * as React from "react";
import { useQuery } from "@tanstack/react-query";
import { Info } from "lucide-react";
import {
  Popover,
  PopoverContent,
  PopoverTrigger,
} from "@/components/ui/popover";
import { sessionQueryOptions } from "@/lib/queries";
import { useApp } from "@/stores/app-store";
import { useProviderFeatures } from "@/hooks/use-provider-features";
import { getContextUsage } from "@/lib/api";
import type {
  ContextBreakdown,
  RateLimitInfo,
  RateLimitStatus,
  TokenUsage,
  TurnRecord,
} from "@/lib/types";

interface ContextDisplayProps {
  sessionId: string;
}

function formatTokens(n: number | undefined | null): string {
  if (n == null) return "--";
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(1)}M`;
  if (n >= 1000) return `${Math.round(n / 1000)}k`;
  return String(n);
}

function formatCost(cost: number | undefined): string | null {
  if (cost == null) return null;
  if (cost < 0.01) return `$${cost.toFixed(4)}`;
  return `$${cost.toFixed(2)}`;
}

function formatDuration(ms: number | undefined): string | null {
  if (ms == null) return null;
  if (ms < 1000) return `${ms}ms`;
  return `${(ms / 1000).toFixed(1)}s`;
}

function formatResetIn(resetsAt: number | undefined): string | null {
  if (resetsAt == null) return null;
  const diff = resetsAt - Date.now();
  if (diff <= 0) return "now";
  const seconds = Math.floor(diff / 1000);
  if (seconds < 60) return `${seconds}s`;
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${minutes}m`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours}h ${minutes % 60}m`;
  const days = Math.floor(hours / 24);
  return `${days}d`;
}

function findLatestUsage(turns: TurnRecord[] | undefined): TokenUsage | null {
  if (!turns) return null;
  for (let i = turns.length - 1; i >= 0; i--) {
    if (turns[i].usage) return turns[i].usage!;
  }
  return null;
}

function barClassForStatus(status: RateLimitStatus, pct: number): string {
  if (status === "rejected" || pct >= 95) return "bg-destructive";
  if (status === "allowed_warning" || pct >= 80) return "bg-amber-500";
  return "bg-foreground/60";
}

function RateLimitRow({ info }: { info: RateLimitInfo }) {
  const pct = Math.min(100, Math.round(info.utilization * 100));
  const resetIn = formatResetIn(info.resetsAt);
  const barClass = barClassForStatus(info.status, pct);
  return (
    <div className="space-y-1">
      <div className="flex items-center gap-2">
        <span className="truncate text-xs text-foreground/80">
          {info.label}
        </span>
        <span className="ml-auto text-[11px] tabular-nums text-muted-foreground">
          {pct}%
          {resetIn && (
            <>
              {" · resets "}
              <span className="tabular-nums">{resetIn}</span>
            </>
          )}
        </span>
      </div>
      <div className="h-0.5 w-full overflow-hidden rounded-full bg-muted/40">
        <div
          className={`h-full transition-all ${barClass}`}
          style={{ width: `${pct}%` }}
        />
      </div>
    </div>
  );
}

/**
 * Compact inline rate-limit indicator shown next to the context-window
 * chip in the chat toolbar — the user-facing answer to "am I about to
 * hit my 5-hour or weekly limit?" without forcing a popover open.
 *
 * Renders only the single highest-utilization bucket within the given
 * label prefix; the popover ("Plan usage" section) remains the place
 * to inspect every bucket. Hidden entirely when no bucket has been
 * reported yet (fresh install, API-key user, non-Claude provider) so
 * the toolbar doesn't show a misleading "0%" placeholder.
 *
 * Reuses `barClassForStatus` and `formatResetIn` so the inline chip
 * and the popover row stay color- and copy-aligned.
 */
function InlineRateLimitChip({
  info,
  shortLabel,
}: {
  info: RateLimitInfo;
  shortLabel: string;
}) {
  const pct = Math.min(100, Math.round(info.utilization * 100));
  const resetIn = formatResetIn(info.resetsAt);
  const barClass = barClassForStatus(info.status, pct);
  return (
    <span
      className="inline-flex items-center gap-1.5 rounded-md px-1.5 py-1 text-xs text-muted-foreground"
      title={`${info.label} — ${pct}% used${
        resetIn ? `, resets ${resetIn}` : ""
      }`}
    >
      <span className="text-[11px] uppercase tracking-wide text-muted-foreground/70">
        {shortLabel}
      </span>
      <span className="h-0.5 w-10 overflow-hidden rounded-full bg-muted/40">
        <span
          className={`block h-full ${barClass}`}
          style={{ width: `${pct}%` }}
        />
      </span>
      <span className="tabular-nums">{pct}%</span>
      {resetIn && (
        <span className="tabular-nums text-muted-foreground/70">
          · {resetIn}
        </span>
      )}
    </span>
  );
}

/**
 * Pick the bucket to surface for a given short label. Strategy:
 *
 *  - "5h"  → the `five_hour` bucket exactly. There's only one.
 *  - "Wk"  → highest-utilization of the weekly buckets
 *           (`seven_day` / `seven_day_opus` / `seven_day_sonnet`).
 *           A user with high Opus utilization still wants to see
 *           that bar before they hit a hard cap, even if the
 *           "all models" bucket is comfortably below.
 *
 * Returns `null` when none of the candidate buckets has been
 * reported yet — the inline chip then renders nothing rather than
 * a bogus 0% indicator.
 */
function pickFiveHourBucket(
  rateLimits: Record<string, RateLimitInfo>,
): RateLimitInfo | null {
  return rateLimits["five_hour"] ?? null;
}

function pickWeeklyBucket(
  rateLimits: Record<string, RateLimitInfo>,
): RateLimitInfo | null {
  const candidates = [
    rateLimits["seven_day"],
    rateLimits["seven_day_opus"],
    rateLimits["seven_day_sonnet"],
  ].filter((b): b is RateLimitInfo => !!b);
  if (candidates.length === 0) return null;
  return candidates.reduce((max, cur) =>
    cur.utilization > max.utilization ? cur : max,
  );
}

/**
 * Per-category context breakdown lazily loaded from the provider's
 * live SDK Query via the mid-turn RPC plumbing in
 * `CachedBridge.pending_rpcs`. Fetch fires on popover open; the
 * section is entirely hidden when the active provider doesn't
 * support context introspection (feature flag off) or when no turn
 * is in flight (the SDK's `getContextUsage()` only exists on a
 * live Query — unavailable between turns).
 */
function ContextBreakdownSection({
  sessionId,
  visible,
}: {
  sessionId: string;
  visible: boolean;
}) {
  const [data, setData] = React.useState<ContextBreakdown | null>(null);
  const [loading, setLoading] = React.useState(false);
  const [error, setError] = React.useState<string | null>(null);

  React.useEffect(() => {
    if (!visible) return;
    let cancelled = false;
    setLoading(true);
    setError(null);
    getContextUsage(sessionId)
      .then((b) => {
        if (!cancelled) setData(b);
      })
      .catch((err: Error) => {
        if (!cancelled) setError(err.message ?? "failed");
      })
      .finally(() => {
        if (!cancelled) setLoading(false);
      });
    return () => {
      cancelled = true;
    };
  }, [sessionId, visible]);

  if (!visible) return null;

  return (
    <div className="mt-3 border-t border-border/60 pt-2">
      <div className="mb-2 text-[10px] font-medium uppercase tracking-wide text-muted-foreground">
        Breakdown
      </div>
      {loading && (
        <div className="italic text-[11px] text-muted-foreground/60">
          loading…
        </div>
      )}
      {error && (
        <div className="italic text-[11px] text-destructive/80">{error}</div>
      )}
      {!loading && !error && (!data || data.categories.length === 0) && (
        <div className="italic text-[11px] text-muted-foreground/60">
          No breakdown available for this turn.
        </div>
      )}
      {!loading && !error && data && data.categories.length > 0 && (
        <div className="space-y-1.5">
          {data.categories.map((cat) => {
            const pct =
              data.totalTokens > 0
                ? Math.min(100, (cat.tokens / data.totalTokens) * 100)
                : 0;
            return (
              <div key={cat.name} className="space-y-0.5">
                <div className="flex items-center gap-2">
                  <span
                    className="inline-block h-2 w-2 shrink-0 rounded-sm"
                    style={{
                      backgroundColor: cat.color ?? "hsl(var(--muted-foreground))",
                    }}
                  />
                  <span className="truncate text-[11px] text-foreground/80">
                    {cat.name}
                  </span>
                  <span className="ml-auto text-[11px] tabular-nums text-muted-foreground">
                    {formatTokens(cat.tokens)}
                  </span>
                </div>
                <div className="h-0.5 w-full overflow-hidden rounded-full bg-muted/40">
                  <div
                    className="h-full transition-all"
                    style={{
                      width: `${pct}%`,
                      backgroundColor:
                        cat.color ?? "hsl(var(--muted-foreground))",
                    }}
                  />
                </div>
              </div>
            );
          })}
        </div>
      )}
    </div>
  );
}

export function ContextDisplay({ sessionId }: ContextDisplayProps) {
  const { data } = useQuery(sessionQueryOptions(sessionId));
  const { state } = useApp();
  const usage = React.useMemo(
    () => findLatestUsage(data?.detail.turns),
    [data?.detail.turns],
  );
  const [popoverOpen, setPopoverOpen] = React.useState(false);

  // Breakdown availability: provider supports the RPC AND a turn is
  // actively running (the SDK's `getContextUsage()` is a method on
  // the live Query, which only exists mid-turn). Both halves must be
  // true or the section stays hidden — the bone of the feature is
  // real-time introspection of what's filling context right now.
  const provider = data?.detail.summary.provider;
  const features = useProviderFeatures(provider);
  const hasRunningTurn = React.useMemo(
    () => (data?.detail.turns ?? []).some((t) => t.status === "running"),
    [data?.detail.turns],
  );
  const showBreakdown = !!features.contextBreakdown && hasRunningTurn;

  const rateLimitEntries = React.useMemo(() => {
    const all = Object.values(state.rateLimits);
    return all.sort((a, b) => a.label.localeCompare(b.label));
  }, [state.rateLimits]);

  const hasWarning = rateLimitEntries.some(
    (r) => r.status === "allowed_warning" || r.status === "rejected",
  );

  // Surface the 5-hour and weekly buckets inline next to the
  // context-window chip so users can glance at "am I about to hit a
  // limit?" without opening the popover. The popover stays as the
  // detailed-breakdown affordance for every bucket. Both can be null
  // (fresh install / API-key user / non-Claude provider) — the chips
  // hide entirely in that case so the toolbar doesn't show a stale
  // 0% placeholder.
  const fiveHour = React.useMemo(
    () => pickFiveHourBucket(state.rateLimits),
    [state.rateLimits],
  );
  const weekly = React.useMemo(
    () => pickWeeklyBucket(state.rateLimits),
    [state.rateLimits],
  );

  // Current context-window occupancy. We prefer the bridge-supplied
  // `liveContextTokens` snapshot — it's computed from the LAST API
  // call's prompt + the running output, which is the right
  // numerator for "what's filling context right now". The aggregate
  // fields (inputTokens / cacheReadTokens / cacheWriteTokens) sum
  // across every iteration of the turn's tool loop and so multi-
  // count the cached system prompt; using them here pushes the
  // indicator past 100% on every tool-heavy turn (the "51M / 1M"
  // inflation bug). The aggregate sum stays as a fallback for
  // providers that don't yet emit liveContextTokens. See
  // provider-claude-sdk bridge `result` and per-assistant-message
  // handlers, where both events carry `liveContextTokens`.
  const used = usage
    ? (usage.liveContextTokens ??
        usage.inputTokens +
          usage.outputTokens +
          (usage.cacheReadTokens ?? 0) +
          (usage.cacheWriteTokens ?? 0))
    : null;

  // Denominator resolution: provider-declared ProviderModel
  // capability wins over the SDK-reported TokenUsage.contextWindow,
  // because SDK values drift when the provider auto-negotiates a
  // beta window (e.g. Anthropic's 1M context beta). Key the lookup
  // on the resolved pinned model from usage when we have it, else
  // fall back to whatever the session's configured model is.
  const session = data?.detail.summary;
  const modelId = usage?.model ?? session?.model;
  const providerEntry = React.useMemo(() => {
    if (!session || !modelId) return undefined;
    return state.providers
      .find((p) => p.kind === session.provider)
      ?.models.find((m) => m.value === modelId);
  }, [state.providers, session, modelId]);
  const declaredWindow = providerEntry?.contextWindow ?? null;
  const sdkWindow = usage?.contextWindow ?? null;
  const total = declaredWindow ?? sdkWindow;
  const windowSource: "declared" | "sdk" | null =
    declaredWindow != null ? "declared" : sdkWindow != null ? "sdk" : null;

  const pct =
    used != null && total != null && total > 0
      ? Math.min(100, Math.round((used / total) * 100))
      : null;

  const usedLabel = formatTokens(used);
  const totalLabel = formatTokens(total);
  const costLabel = formatCost(usage?.totalCostUsd);
  const durationLabel = formatDuration(usage?.durationMs);
  const cacheRead = usage?.cacheReadTokens ?? 0;
  const cacheWrite = usage?.cacheWriteTokens ?? 0;
  const hasCache = cacheRead > 0 || cacheWrite > 0;

  const barFillClass =
    pct == null
      ? "bg-foreground/60"
      : pct >= 80
        ? "bg-destructive"
        : pct >= 50
          ? "bg-amber-500"
          : "bg-foreground/60";

  return (
    <div className="inline-flex items-center gap-1">
      <Popover open={popoverOpen} onOpenChange={setPopoverOpen}>
        <PopoverTrigger asChild>
          <button
            type="button"
            className="inline-flex items-center gap-1 rounded-md px-1.5 py-1 text-xs text-muted-foreground hover:text-foreground"
            title="Context window & plan usage"
          >
            <Info className="h-3 w-3" />
            <span className="tabular-nums">
              {usedLabel} / {totalLabel}
            </span>
            {hasWarning && (
              <span className="ml-0.5 inline-block h-1.5 w-1.5 rounded-full bg-amber-500" />
            )}
          </button>
        </PopoverTrigger>
      <PopoverContent side="top" align="end" className="w-80 p-3">
        <div className="flex items-center gap-2">
          <span className="text-[10px] font-medium uppercase tracking-wide text-muted-foreground">
            Context window
          </span>
          {pct != null && (
            <span className="ml-auto text-[11px] tabular-nums text-muted-foreground">
              {usedLabel} / {totalLabel} · {pct}%
            </span>
          )}
          {pct == null && usage && (
            <span className="ml-auto text-[11px] tabular-nums text-muted-foreground">
              {usedLabel} / --
            </span>
          )}
        </div>
        {pct != null && (
          <div className="mt-2 h-0.5 w-full overflow-hidden rounded-full bg-muted/40">
            <div
              className={`h-full transition-all ${barFillClass}`}
              style={{ width: `${pct}%` }}
            />
          </div>
        )}
        {windowSource && (
          <div className="mt-2 text-[10px] text-muted-foreground/70">
            window source:{" "}
            <span className="tabular-nums">
              {windowSource === "declared"
                ? "provider-declared"
                : "sdk-reported"}
            </span>
            {modelId && (
              <>
                {" · "}
                <span className="font-mono">{modelId}</span>
              </>
            )}
          </div>
        )}
        {!usage && (
          <div className="mt-2 italic text-[11px] text-muted-foreground/70">
            No usage data yet — run a turn to populate.
          </div>
        )}
        {usage && hasCache && (
          <div className="mt-2 text-[11px] text-muted-foreground/80">
            cache read:{" "}
            <span className="tabular-nums">{formatTokens(cacheRead)}</span>
            {" · "}cache write:{" "}
            <span className="tabular-nums">{formatTokens(cacheWrite)}</span>
          </div>
        )}
        {usage && (costLabel || durationLabel) && (
          <div className="mt-1 text-[11px] text-muted-foreground/80">
            {costLabel && <span className="tabular-nums">{costLabel}</span>}
            {costLabel && durationLabel && " · "}
            {durationLabel && (
              <span className="tabular-nums">{durationLabel}</span>
            )}
          </div>
        )}
        {/* Per-category breakdown. Lazily fetched on open via the
            mid-turn bridge RPC; gated on provider capability +
            an active turn because `query.getContextUsage()` only
            exists on a live SDK Query. `popoverOpen` is the
            controlled open state so we don't fire the RPC while
            the popover is closed. */}
        <ContextBreakdownSection
          sessionId={sessionId}
          visible={popoverOpen && showBreakdown}
        />

        <div className="mt-3 border-t border-border/60 pt-2">
          <div className="mb-2 text-[10px] font-medium uppercase tracking-wide text-muted-foreground">
            Plan usage
          </div>
          {rateLimitEntries.length === 0 ? (
            <div className="italic text-[11px] text-muted-foreground/60">
              No plan usage data from this provider.
            </div>
          ) : (
            <div className="space-y-2">
              {rateLimitEntries.map((info) => (
                <RateLimitRow key={info.bucket} info={info} />
              ))}
            </div>
          )}
        </div>
      </PopoverContent>
      </Popover>
      {fiveHour && <InlineRateLimitChip info={fiveHour} shortLabel="5h" />}
      {weekly && <InlineRateLimitChip info={weekly} shortLabel="Wk" />}
    </div>
  );
}
