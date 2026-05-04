import * as React from "react";
import { ChevronRight, Wrench } from "lucide-react";
import type { ToolCall } from "@/lib/types";
import { useTicker } from "@/hooks/use-ticker";
import { renderToolArgs, ToolOutputContent } from "./tool-renderers";

interface ToolCallCardProps {
  toolCall: ToolCall;
  /** Initial open state for the collapsible. Used by the
   *  edit-standalone code path so each broken-out Edit lands
   *  pre-expanded with its diff visible — the whole point of the
   *  toggle. Defaults to closed (existing collapsed-card behavior). */
  defaultOpen?: boolean;
}

// One-line preview of what a tool call is doing, shown inline next to
// the tool name on the COLLAPSED header so users don't have to expand
// every card to see what happened. Only short, descriptive fields are
// surfaced here -- never the raw command, full file path, output, or
// any other potentially long blob. Those still live behind the toggle
// in the expanded view. CSS truncate clips anything that does run wide.
function toolPreview(name: string, args: unknown): string | null {
  if (!args || typeof args !== "object") return null;
  const a = args as Record<string, unknown>;
  const str = (key: string) =>
    typeof a[key] === "string" ? (a[key] as string) : undefined;
  // For paths, show the basename so the most informative part fits in
  // a narrow card. The full path lives in the expanded args view.
  const basename = (path: string | undefined) => {
    if (!path) return undefined;
    const slash = path.lastIndexOf("/");
    return slash >= 0 ? path.slice(slash + 1) : path;
  };

  switch (name) {
    case "Bash":
      // Description only -- the raw command can be hundreds of chars.
      return str("description") ?? null;
    case "Read":
    case "Write":
    case "Edit":
    case "NotebookEdit":
      return basename(str("file_path") ?? str("notebook_path")) ?? null;
    case "Glob":
    case "Grep":
      return str("pattern") ?? null;
    case "Task":
    case "Agent":
      return str("description") ?? null;
    case "WebSearch":
      return str("query") ?? null;
    case "Skill":
      return str("skill") ?? null;
    case "ScheduleWakeup":
      return str("reason") ?? null;
    case "ExitPlanMode": {
      const plan = str("plan");
      if (!plan) return null;
      const firstLine = plan.split("\n").find((line) => line.trim().length > 0);
      if (!firstLine) return null;
      const cleaned = firstLine.replace(/^#+\s*/, "").trim();
      return cleaned.length > 40 ? cleaned.slice(0, 40) + "…" : cleaned;
    }
    case "EnterPlanMode":
      return "Switching to plan mode";
    default:
      return null;
  }
}

// Live elapsed counter for an in-flight tool call. Only renders when
// both halves are present: `startedAt` (provided cross-provider by
// runtime-core on ToolCallStarted) AND status === 'pending'. A
// ticking 1-second counter is enough resolution for the longest
// tool call users actually watch in real time.
function ToolElapsed({ startedAt }: { startedAt: string }) {
  const now = useTicker(1000);
  const startedMs = React.useMemo(() => {
    const t = Date.parse(startedAt);
    return Number.isNaN(t) ? null : t;
  }, [startedAt]);
  if (startedMs == null) return null;
  const elapsedSec = Math.max(0, Math.floor((now - startedMs) / 1000));
  const label =
    elapsedSec < 60
      ? `${elapsedSec}s`
      : `${Math.floor(elapsedSec / 60)}m ${elapsedSec % 60}s`;
  return (
    <span
      className="ml-2 shrink-0 font-mono text-[10px] tabular-nums text-muted-foreground/70"
      aria-label={`Running for ${label}`}
    >
      {label}
    </span>
  );
}

// Threshold (ms) past which a per-tool heartbeat counts as stale.
// Picked to be longer than the SDK's nominal heartbeat cadence so
// momentary scheduling jitter doesn't flip the pip on every other
// frame. Shorter than the session-wide 45s stuck banner so a
// stalled tool surfaces here first.
const TOOL_STALL_THRESHOLD_MS = 30_000;

// Per-tool stalled-tool indicator. Renders only when the provider
// is actually heartbeating this tool (`lastProgressAt` populated)
// AND the most recent heartbeat is older than the threshold AND
// the call is still pending. Absence of `lastProgressAt` means
// the provider doesn't emit `tool_progress` for this tool — the
// session-wide stuck banner handles that case as a fallback. The
// pip stays out of the way for fast tools (Read/Glob) that finish
// before any heartbeat arrives.
function ToolStalled({ lastProgressAt }: { lastProgressAt: string }) {
  const now = useTicker(1000);
  const lastMs = React.useMemo(() => {
    const t = Date.parse(lastProgressAt);
    return Number.isNaN(t) ? null : t;
  }, [lastProgressAt]);
  if (lastMs == null) return null;
  const sinceMs = now - lastMs;
  if (sinceMs < TOOL_STALL_THRESHOLD_MS) return null;
  const sinceSec = Math.floor(sinceMs / 1000);
  const label =
    sinceSec < 60
      ? `${sinceSec}s`
      : `${Math.floor(sinceSec / 60)}m ${sinceSec % 60}s`;
  return (
    <span
      className="ml-2 shrink-0 rounded-sm bg-amber-500/10 px-1.5 py-0.5 font-mono text-[10px] tabular-nums text-amber-700 dark:text-amber-400"
      aria-label={`No progress for ${label}`}
      title="The SDK hasn't reported progress for this tool recently. It may be stuck."
    >
      no progress · {label}
    </span>
  );
}

export function ToolCallCard({
  toolCall,
  defaultOpen = false,
}: ToolCallCardProps) {
  const [open, setOpen] = React.useState(defaultOpen);

  const statusColor =
    toolCall.status === "completed"
      ? "text-green-600 dark:text-green-400"
      : toolCall.status === "failed"
        ? "text-destructive"
        : "text-muted-foreground";

  const preview = toolPreview(toolCall.name, toolCall.args);
  const isPending = toolCall.status === "pending";
  const showElapsed =
    isPending && typeof toolCall.startedAt === "string";
  // Stalled-tool pip is gated naturally by the presence of
  // `lastProgressAt` — only providers that emit `tool_progress`
  // populate it. Pending check guards against a stale heartbeat
  // lingering after completion (the field isn't cleared on
  // tool_call_completed since the call is done; the pip just
  // disappears because `isPending` is false).
  const showStalled =
    isPending && typeof toolCall.lastProgressAt === "string";

  // For Edit / MultiEdit / Write the args renderer already shows the
  // diff (or full new file body) inline, so the provider's `output`
  // string is just a redundant confirmation like "The file has been
  // updated." On a successful call we suppress the Output block to
  // keep the card tight — the diff IS the result. On failure we
  // still surface it: when something goes wrong, the output line
  // tends to be the only place that says WHY (path didn't exist,
  // string match wasn't unique, etc.). The separate `error` block
  // below is unaffected and renders normally either way.
  const isFileWriteTool =
    toolCall.name === "Edit" ||
    toolCall.name === "MultiEdit" ||
    toolCall.name === "Write";
  const hideOutputForFileWrite =
    isFileWriteTool && toolCall.status === "completed";

  return (
    <div className="text-xs">
      <button
        type="button"
        className="flex w-full items-center gap-1.5 px-1 py-1 text-left hover:bg-muted/40 focus-visible:bg-muted/40"
        onClick={() => setOpen(!open)}
      >
        <ChevronRight
          className={`h-3 w-3 shrink-0 text-muted-foreground/70 transition-transform ${open ? "rotate-90" : ""}`}
        />
        <Wrench className="h-3 w-3 shrink-0 text-muted-foreground/70" />
        <span className="min-w-0 flex-1 truncate">
          <span className="font-medium">{toolCall.name}</span>
          {preview && (
            <span className="ml-1.5 text-muted-foreground">{preview}</span>
          )}
        </span>
        {showElapsed && toolCall.startedAt && (
          <ToolElapsed startedAt={toolCall.startedAt} />
        )}
        {showStalled && toolCall.lastProgressAt && (
          <ToolStalled lastProgressAt={toolCall.lastProgressAt} />
        )}
        <span className={`ml-2 shrink-0 text-[10px] ${statusColor}`}>
          {toolCall.status}
        </span>
      </button>

      {open && (
        <div className="space-y-2 px-1 pb-2 pt-1">
          <div>{renderToolArgs(toolCall.name, toolCall.args)}</div>
          {toolCall.output && !hideOutputForFileWrite && (
            <div>
              <p className="mb-1 font-medium text-muted-foreground">Output</p>
              {toolCall.name === "Task" || toolCall.name === "Agent" ? (
                <ToolOutputContent output={toolCall.output} />
              ) : (
                <pre className="max-h-40 overflow-auto rounded bg-muted p-2 text-[11px]">
                  {toolCall.output}
                </pre>
              )}
            </div>
          )}
          {toolCall.error && (
            <div>
              <p className="mb-1 font-medium text-destructive">Error</p>
              <pre className="max-h-40 overflow-auto rounded bg-muted p-2 text-[11px] text-destructive">
                {toolCall.error}
              </pre>
            </div>
          )}
        </div>
      )}
    </div>
  );
}
