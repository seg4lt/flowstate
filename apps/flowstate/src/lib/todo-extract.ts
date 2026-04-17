import type { ToolCall, TurnRecord } from "./types";

// Scans a single TurnRecord for the most recent main-agent TodoWrite
// call. Subagent calls (parentCallId set) are intentionally ignored —
// they're owned by the subagent's own box, not the session-level view.
function scanTurnForTodoWrite(
  turn: TurnRecord | null | undefined,
): ToolCall | null {
  const calls = turn?.toolCalls;
  if (!calls) return null;
  for (let i = calls.length - 1; i >= 0; i--) {
    const tc = calls[i];
    if (tc.name === "TodoWrite" && tc.parentCallId === undefined) return tc;
  }
  return null;
}

// Finds the latest main-agent TodoWrite in a session. Prefers the
// running turn's current state (so the view is live during execution)
// and falls back to the most recent completed turn carrying a
// TodoWrite. Returns null when no main-agent TodoWrite exists anywhere
// in the session.
export function findLatestMainTodoWrite(
  turns: TurnRecord[],
  runningTurn: TurnRecord | null | undefined,
): ToolCall | null {
  const fromRunning = scanTurnForTodoWrite(runningTurn);
  if (fromRunning) return fromRunning;
  for (let i = turns.length - 1; i >= 0; i--) {
    const found = scanTurnForTodoWrite(turns[i]);
    if (found) return found;
  }
  return null;
}

// Parses a TodoWrite ToolCall's args into the raw todos array plus
// a completed/total progress count. Returns null when the args shape
// isn't what we expect — callers fall through to an empty state.
export interface TodoProgress {
  todos: unknown[];
  completed: number;
  total: number;
}

export function parseTodoProgress(
  toolCall: ToolCall | null | undefined,
): TodoProgress | null {
  if (!toolCall) return null;
  const rawArgs =
    toolCall.args && typeof toolCall.args === "object"
      ? (toolCall.args as { todos?: unknown })
      : null;
  const todos = rawArgs && Array.isArray(rawArgs.todos) ? rawArgs.todos : null;
  if (!todos || todos.length === 0) return null;
  const completed = todos.reduce<number>((n, t) => {
    const item = t && typeof t === "object" ? (t as { status?: unknown }) : null;
    return item && item.status === "completed" ? n + 1 : n;
  }, 0);
  return { todos, completed, total: todos.length };
}
