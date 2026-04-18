import * as React from "react";
import { ChevronDown, ChevronRight } from "lucide-react";
import { MarkdownContent } from "./markdown-content";

/**
 * Inline "Conversation recap" divider. Rendered where the Claude
 * Agent SDK compressed older turns into a summary to free up context
 * window. Metrics come from the `compact_boundary` system message;
 * the summary text comes from the `PostCompact` hook. Runtime-core
 * merges them, but either half can arrive first so all four payload
 * fields are independently optional here.
 *
 * Visual: horizontal rule + pill label with trigger tone ("auto" is
 * the only one that fires today outside a manual /compact call). A
 * subdued metrics line sits under the pill. Clicking the chevron
 * reveals the summary as markdown. Collapsed by default — the
 * summary can be multi-paragraph and we don't want it competing with
 * real turn content in the scroll flow.
 */
export function CompactBlock({
  trigger,
  preTokens,
  postTokens,
  durationMs,
  summary,
}: {
  trigger: "auto" | "manual";
  preTokens?: number;
  postTokens?: number;
  durationMs?: number;
  summary?: string;
}) {
  const [expanded, setExpanded] = React.useState(false);
  const hasSummary = summary != null && summary.length > 0;

  return (
    <div className="my-2 flex flex-col items-stretch gap-1.5">
      <div className="flex items-center gap-2 text-[11px] uppercase tracking-wider text-muted-foreground">
        <div className="h-px flex-1 bg-border" />
        <span className="rounded-full border border-amber-500/40 bg-amber-500/5 px-2 py-0.5 font-medium text-amber-700 dark:text-amber-400">
          Conversation recap{trigger === "manual" ? " (manual)" : ""}
        </span>
        <div className="h-px flex-1 bg-border" />
      </div>

      <div className="flex items-center justify-center gap-2 text-[11px] text-muted-foreground">
        {formatMetrics(preTokens, postTokens, durationMs)}
        {hasSummary && (
          <button
            type="button"
            onClick={() => setExpanded((s) => !s)}
            className="inline-flex items-center gap-0.5 rounded hover:text-foreground"
            aria-expanded={expanded}
          >
            {expanded ? (
              <ChevronDown className="h-3 w-3" />
            ) : (
              <ChevronRight className="h-3 w-3" />
            )}
            {expanded ? "hide summary" : "show summary"}
          </button>
        )}
      </div>

      {expanded && hasSummary && (
        <div className="rounded-md border border-border/50 bg-muted/30 px-3 py-2 text-sm">
          <MarkdownContent content={summary!} />
        </div>
      )}
    </div>
  );
}

function formatMetrics(
  preTokens?: number,
  postTokens?: number,
  durationMs?: number,
): string {
  const parts: string[] = [];
  if (preTokens != null && postTokens != null) {
    parts.push(`${formatTokens(preTokens)} \u2192 ${formatTokens(postTokens)}`);
  } else if (preTokens != null) {
    parts.push(`${formatTokens(preTokens)} before`);
  } else if (postTokens != null) {
    parts.push(`${formatTokens(postTokens)} after`);
  }
  if (durationMs != null) {
    parts.push(formatDuration(durationMs));
  }
  return parts.join(", ");
}

function formatTokens(n: number): string {
  if (n >= 1000) {
    const k = n / 1000;
    return k >= 100 ? `${Math.round(k)}k` : `${k.toFixed(1)}k`;
  }
  return n.toString();
}

function formatDuration(ms: number): string {
  if (ms < 1000) return `${ms}ms`;
  const s = ms / 1000;
  return s >= 10 ? `${Math.round(s)}s` : `${s.toFixed(1)}s`;
}
