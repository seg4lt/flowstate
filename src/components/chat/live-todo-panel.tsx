import * as React from "react";
import { ChevronDown, ChevronUp, ListChecks } from "lucide-react";
import type { ToolCall } from "@/lib/types";
import { TodoList } from "./tool-renderers";

interface LiveTodoPanelProps {
  toolCalls: ToolCall[] | null | undefined;
}

// Floating per-turn todo panel pinned above the WorkingIndicator
// while a turn is in flight. Collapsed by default as a thin pill
// showing progress ("Plan · 2 of 5"); click to expand the full list
// upward. Driven off the running turn's latest main-agent TodoWrite
// call — subagent TodoWrites stay inline inside their subagent box.
export const LiveTodoPanel = React.memo(function LiveTodoPanel({
  toolCalls,
}: LiveTodoPanelProps) {
  const [expanded, setExpanded] = React.useState(false);

  const latest = React.useMemo(() => {
    if (!toolCalls) return null;
    for (let i = toolCalls.length - 1; i >= 0; i--) {
      const tc = toolCalls[i];
      if (tc.name === "TodoWrite" && tc.parentCallId === undefined) return tc;
    }
    return null;
  }, [toolCalls]);

  if (!latest) return null;

  const rawArgs =
    latest.args && typeof latest.args === "object"
      ? (latest.args as { todos?: unknown })
      : null;
  const todos = rawArgs && Array.isArray(rawArgs.todos) ? rawArgs.todos : null;
  if (!todos || todos.length === 0) return null;

  const total = todos.length;
  const completed = todos.reduce<number>((n, t) => {
    const item = t && typeof t === "object" ? (t as { status?: unknown }) : null;
    return item && item.status === "completed" ? n + 1 : n;
  }, 0);

  return (
    <div className="flex shrink-0 flex-col border-t border-border/60 bg-muted/30">
      {expanded && (
        <div className="max-h-60 overflow-y-auto border-b border-border/40 px-4 py-2">
          <TodoList todos={todos} />
        </div>
      )}
      <button
        type="button"
        onClick={() => setExpanded((v) => !v)}
        className="flex items-center gap-2 px-4 py-1.5 text-xs text-muted-foreground hover:bg-muted/50 hover:text-foreground"
      >
        <ListChecks className="h-3 w-3" />
        <span className="font-medium">Todos</span>
        <span className="text-muted-foreground/70">
          · <span className="tabular-nums">{completed}</span> of{" "}
          <span className="tabular-nums">{total}</span>
        </span>
        {expanded ? (
          <ChevronDown className="ml-auto h-3 w-3" />
        ) : (
          <ChevronUp className="ml-auto h-3 w-3" />
        )}
      </button>
    </div>
  );
});
