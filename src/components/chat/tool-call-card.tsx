import * as React from "react";
import { ChevronRight, Wrench } from "lucide-react";
import type { ToolCall } from "@/lib/types";

interface ToolCallCardProps {
  toolCall: ToolCall;
}

export function ToolCallCard({ toolCall }: ToolCallCardProps) {
  const [open, setOpen] = React.useState(false);

  const statusColor =
    toolCall.status === "completed"
      ? "text-green-600 dark:text-green-400"
      : toolCall.status === "failed"
        ? "text-destructive"
        : "text-muted-foreground";

  return (
    <div className="rounded-md border border-border text-xs">
      <button
        type="button"
        className="flex w-full items-center gap-2 px-3 py-2 text-left hover:bg-muted/50"
        onClick={() => setOpen(!open)}
      >
        <ChevronRight
          className={`h-3 w-3 shrink-0 transition-transform ${open ? "rotate-90" : ""}`}
        />
        <Wrench className="h-3 w-3 shrink-0" />
        <span className="truncate font-medium">{toolCall.name}</span>
        <span className={`ml-auto shrink-0 ${statusColor}`}>
          {toolCall.status}
        </span>
      </button>

      {open && (
        <div className="space-y-2 border-t border-border px-3 py-2">
          <div>
            <p className="mb-1 font-medium text-muted-foreground">Args</p>
            <pre className="max-h-40 overflow-auto rounded bg-muted p-2 text-[11px]">
              {JSON.stringify(toolCall.args, null, 2)}
            </pre>
          </div>
          {toolCall.output && (
            <div>
              <p className="mb-1 font-medium text-muted-foreground">Output</p>
              <pre className="max-h-40 overflow-auto rounded bg-muted p-2 text-[11px]">
                {toolCall.output}
              </pre>
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
