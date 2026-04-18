import { RotateCcw } from "lucide-react";
import type { RetryState } from "@/lib/types";
import { useTicker } from "@/hooks/use-ticker";

interface ApiRetryBannerProps {
  state: RetryState;
}

/**
 * Inline "Retrying…" banner that surfaces provider-level auto-retry
 * state while the turn is still in flight. Lives just above the
 * composer — a subtle amber strip with a ticking countdown of when
 * the SDK's next attempt is scheduled to fire. Auto-disappears when
 * chat-view clears `retryState` (next assistant text delta, turn
 * completion, or a follow-up retry event replaces this one).
 *
 * Purely informational — the user can't cancel or skip the retry
 * from here. Giving them a visible "this isn't frozen, the SDK is
 * working on it" signal is the point.
 */
export function ApiRetryBanner({ state }: ApiRetryBannerProps) {
  const now = useTicker(1000);
  const remainingMs = Math.max(
    0,
    state.retryDelayMs - (now - state.startedAt),
  );
  const seconds = Math.ceil(remainingMs / 1000);
  const max = state.maxRetries > 0 ? ` of ${state.maxRetries}` : "";
  const hint = seconds > 0 ? ` · next attempt in ${seconds}s` : " · retrying now…";
  const tooltip = state.errorStatus
    ? `HTTP ${state.errorStatus}: ${state.error || "transient error"}`
    : state.error || "transient API error";

  return (
    <div
      className="flex shrink-0 items-center gap-2 border-t border-amber-500/40 bg-amber-500/5 px-4 py-1 text-[11px] text-amber-700 dark:text-amber-400"
      title={tooltip}
    >
      <RotateCcw className="h-3 w-3 shrink-0 animate-spin [animation-duration:3s]" />
      <span>
        Provider is retrying (attempt {state.attempt}
        {max}){hint}
      </span>
    </div>
  );
}
