import * as React from "react";
import {
  ChevronDown,
  ChevronUp,
  Compass,
  Maximize2,
  Minimize2,
  X,
} from "lucide-react";
import { Button } from "@/components/ui/button";
import type { ToolCall, TurnRecord } from "@/lib/types";
import { findLatestMainTodoWrite, parseTodoProgress } from "@/lib/todo-extract";
import { TodoList } from "./tool-renderers";
import { MarkdownContent } from "./messages/markdown-content";
import { CopyButton } from "./messages/copy-button";

interface AgentContextPanelProps {
  turns: TurnRecord[];
  runningTurn: TurnRecord | null;
  onClose: () => void;
  isFullscreen: boolean;
  onToggleFullscreen: () => void;
}

// Pulls plan markdown straight off the main-agent ExitPlanMode tool
// call's args — the same source ExitPlanModeRenderer reads from. We
// intentionally do not rely on TurnRecord.plan: that field is
// declared in types.ts but never populated by the runtime today.
function extractPlanMarkdown(tc: ToolCall | undefined): string | null {
  if (!tc) return null;
  const args =
    tc.args && typeof tc.args === "object"
      ? (tc.args as { plan?: unknown })
      : null;
  return args && typeof args.plan === "string" ? args.plan : null;
}

function scanTurnForPlan(
  turn: TurnRecord | null | undefined,
): string | null {
  const calls = turn?.toolCalls;
  if (!calls) return null;
  for (let i = calls.length - 1; i >= 0; i--) {
    const tc = calls[i];
    if (tc.name === "ExitPlanMode" && tc.parentCallId === undefined) {
      const plan = extractPlanMarkdown(tc);
      if (plan) return plan;
    }
  }
  return null;
}

export const AgentContextPanel = React.memo(function AgentContextPanel({
  turns,
  runningTurn,
  onClose,
  isFullscreen,
  onToggleFullscreen,
}: AgentContextPanelProps) {
  const latestPlan = React.useMemo(() => {
    const fromRunning = scanTurnForPlan(runningTurn);
    if (fromRunning) return fromRunning;
    for (let i = turns.length - 1; i >= 0; i--) {
      const found = scanTurnForPlan(turns[i]);
      if (found) return found;
    }
    return null;
  }, [turns, runningTurn]);

  const latestTodoCall = React.useMemo(
    () => findLatestMainTodoWrite(turns, runningTurn),
    [turns, runningTurn],
  );
  const todoProgress = React.useMemo(
    () => parseTodoProgress(latestTodoCall),
    [latestTodoCall],
  );

  const [todosCollapsed, setTodosCollapsed] = React.useState(false);

  return (
    <div className="flex h-full flex-col">
      <header className="flex h-10 shrink-0 items-center gap-2 border-b border-border bg-background/80 px-2">
        <Compass className="h-4 w-4 text-muted-foreground" />
        <span className="truncate text-[11px] font-medium">Agent Context</span>
        <div className="ml-auto flex items-center gap-1">
          <Button
            variant="ghost"
            size="icon-xs"
            onClick={onToggleFullscreen}
            aria-label={isFullscreen ? "Exit fullscreen" : "Enter fullscreen"}
            title={isFullscreen ? "Exit fullscreen" : "Enter fullscreen"}
          >
            {isFullscreen ? (
              <Minimize2 className="h-3 w-3" />
            ) : (
              <Maximize2 className="h-3 w-3" />
            )}
          </Button>
          <Button
            variant="ghost"
            size="icon-xs"
            onClick={onClose}
            aria-label="Close agent context panel"
          >
            <X className="h-3 w-3" />
          </Button>
        </div>
      </header>

      <section
        className={`flex min-h-0 flex-col overflow-y-auto border-b border-border/60 px-4 py-3 ${
          todosCollapsed ? "flex-1" : "flex-[2]"
        }`}
      >
        <div className="mb-2 flex items-center gap-2">
          <span className="text-[10px] font-medium uppercase tracking-wide text-muted-foreground">
            Plan
          </span>
          {latestPlan && (
            <CopyButton
              text={latestPlan}
              label="Plan copied"
              title="Copy plan"
              className="ml-auto"
            />
          )}
        </div>
        {latestPlan ? (
          <div className="text-sm leading-relaxed">
            <MarkdownContent content={latestPlan} />
          </div>
        ) : (
          <div className="flex flex-1 items-center justify-center text-xs text-muted-foreground">
            No plan yet
          </div>
        )}
      </section>

      <section
        className={`flex flex-col px-2 ${
          todosCollapsed
            ? "shrink-0 py-2"
            : "min-h-0 flex-[1] overflow-y-auto py-2"
        }`}
      >
        <button
          type="button"
          onClick={() => setTodosCollapsed((v) => !v)}
          aria-expanded={!todosCollapsed}
          title={todosCollapsed ? "Expand todos" : "Collapse todos"}
          className="group mb-1 flex w-full items-center gap-2 rounded-md bg-muted/40 px-2 py-1.5 text-left text-muted-foreground hover:bg-muted hover:text-foreground"
        >
          {todosCollapsed ? (
            <ChevronDown className="h-4 w-4 shrink-0 transition-transform" />
          ) : (
            <ChevronUp className="h-4 w-4 shrink-0 transition-transform" />
          )}
          <span className="text-[11px] font-semibold uppercase tracking-wide">
            Todos
          </span>
          {todoProgress && (
            <span className="text-[11px] tabular-nums text-muted-foreground/80 group-hover:text-muted-foreground">
              · {todoProgress.completed} of {todoProgress.total}
            </span>
          )}
          <span className="ml-auto text-[10px] text-muted-foreground/60 group-hover:text-muted-foreground">
            {todosCollapsed ? "show" : "hide"}
          </span>
        </button>
        {!todosCollapsed && (
          <div className="px-2">
            {todoProgress ? (
              <TodoList todos={todoProgress.todos} />
            ) : (
              <div className="flex flex-1 items-center justify-center py-4 text-xs text-muted-foreground">
                No todos yet
              </div>
            )}
          </div>
        )}
      </section>
    </div>
  );
});
