import * as React from "react";
import { ListChecks } from "lucide-react";
import type { ToolCall } from "@/lib/types";
import { TodoList } from "../tool-renderers";

interface TurnTodoPanelProps {
  toolCall: ToolCall;
}

export const TurnTodoPanel = React.memo(function TurnTodoPanel({
  toolCall,
}: TurnTodoPanelProps) {
  const args =
    toolCall.args && typeof toolCall.args === "object"
      ? (toolCall.args as { todos?: unknown })
      : null;
  const todos = args && Array.isArray(args.todos) ? args.todos : null;
  if (!todos || todos.length === 0) return null;

  const total = todos.length;
  const completed = todos.reduce<number>((n, t) => {
    const item = t && typeof t === "object" ? (t as { status?: unknown }) : null;
    return item && item.status === "completed" ? n + 1 : n;
  }, 0);
  const pct = Math.round((completed / total) * 100);

  return (
    <div className="rounded-lg border border-border/60 bg-muted/40 px-4 py-3">
      <div className="mb-2 flex items-center justify-between gap-2">
        <div className="flex items-center gap-1.5">
          <ListChecks className="h-4 w-4 text-muted-foreground" />
          <span className="text-xs font-medium">Todos</span>
        </div>
        <span className="text-[11px] tabular-nums text-muted-foreground">
          {completed} of {total}
        </span>
      </div>
      <div className="mb-2 h-0.5 w-full overflow-hidden rounded-full bg-border/50">
        <div
          className="h-full bg-foreground/40 transition-all"
          style={{ width: `${pct}%` }}
        />
      </div>
      <TodoList todos={todos} />
    </div>
  );
});
