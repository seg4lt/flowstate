import type { SessionDetail, TurnRecord } from "../../types";

export type TimelineRow =
  | { kind: "user"; id: string; turn: TurnRecord }
  | { kind: "reasoning"; id: string; turn: TurnRecord }
  | { kind: "assistant"; id: string; turn: TurnRecord }
  | { kind: "worklog"; id: string; turn: TurnRecord }
  | { kind: "plan"; id: string; turn: TurnRecord }
  | { kind: "working"; id: string; turn: TurnRecord };

export function buildTimelineRows(session: SessionDetail): TimelineRow[] {
  const rows: TimelineRow[] = [];
  for (const turn of session.turns) {
    rows.push({ kind: "user", id: `${turn.turnId}:user`, turn });

    if (turn.reasoning && turn.reasoning.length > 0) {
      rows.push({ kind: "reasoning", id: `${turn.turnId}:reasoning`, turn });
    }

    const hasWorkEntries =
      (turn.toolCalls?.length ?? 0) > 0 ||
      (turn.fileChanges?.length ?? 0) > 0 ||
      (turn.subagents?.length ?? 0) > 0;

    const hasOutput = turn.output.length > 0;
    const isRunning = turn.status === "running";

    const planProposed = turn.plan?.status === "proposed";

    if (isRunning && !hasOutput && !hasWorkEntries) {
      if (!planProposed) {
        rows.push({ kind: "working", id: `${turn.turnId}:working`, turn });
      }
    } else {
      if (hasWorkEntries) {
        rows.push({ kind: "worklog", id: `${turn.turnId}:worklog`, turn });
      }
      if (hasOutput || turn.status === "completed" || turn.status === "interrupted" || turn.status === "failed") {
        rows.push({ kind: "assistant", id: `${turn.turnId}:assistant`, turn });
      }
      if (isRunning && (hasOutput || hasWorkEntries) && !planProposed) {
        rows.push({ kind: "working", id: `${turn.turnId}:working`, turn });
      }
    }

    if (turn.plan) {
      rows.push({ kind: "plan", id: `${turn.turnId}:plan`, turn });
    }
  }
  return rows;
}
