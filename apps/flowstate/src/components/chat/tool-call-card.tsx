import * as React from "react";
import { ChevronRight, Wrench } from "lucide-react";
import type { ToolCall } from "@/lib/types";
import { renderToolArgs, ToolOutputContent } from "./tool-renderers";

interface ToolCallCardProps {
  toolCall: ToolCall;
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

export function ToolCallCard({ toolCall }: ToolCallCardProps) {
  const [open, setOpen] = React.useState(false);

  const statusColor =
    toolCall.status === "completed"
      ? "text-green-600 dark:text-green-400"
      : toolCall.status === "failed"
        ? "text-destructive"
        : "text-muted-foreground";

  const preview = toolPreview(toolCall.name, toolCall.args);

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
        <span className={`ml-2 shrink-0 text-[10px] ${statusColor}`}>
          {toolCall.status}
        </span>
      </button>

      {open && (
        <div className="space-y-2 px-1 pb-2 pt-1">
          <div>{renderToolArgs(toolCall.name, toolCall.args)}</div>
          {toolCall.output && (
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
