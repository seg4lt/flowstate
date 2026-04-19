// Pure stream-event → turns reducer, extracted from chat-view.tsx.
//
// Used by the store's single `connectStream` handler to update the
// query cache entry for the event's session — because this is a
// pure function over `prev`, we can run it inside a
// `queryClient.setQueryData` updater and route every event
// directly to the right session's cache entry without any
// cross-session state leakage. Returns the same array reference
// when the event doesn't apply to any known turn, so the
// updater can bail out and avoid a wasted re-render.

import type { ContentBlock, RuntimeEvent, TurnRecord } from "@/lib/types";

// Stream-order block accumulators. Adjacent text deltas coalesce into
// the trailing text block; a non-text block (e.g. a tool call) closes
// the run so the next text delta opens a new block. Always returns a
// new array so React.memo / reference equality picks up the change.
export function appendTextDelta(
  blocks: ContentBlock[] | undefined,
  delta: string,
): ContentBlock[] {
  const list = blocks ?? [];
  const last = list[list.length - 1];
  if (last && last.kind === "text") {
    return [...list.slice(0, -1), { kind: "text", text: last.text + delta }];
  }
  return [...list, { kind: "text", text: delta }];
}

export function appendReasoningDelta(
  blocks: ContentBlock[] | undefined,
  delta: string,
): ContentBlock[] {
  const list = blocks ?? [];
  const last = list[list.length - 1];
  if (last && last.kind === "reasoning") {
    return [
      ...list.slice(0, -1),
      { kind: "reasoning", text: last.text + delta },
    ];
  }
  return [...list, { kind: "reasoning", text: delta }];
}

// Merge-or-append for compaction blocks. Runtime-core already pairs
// up `compact_boundary` + `compact_summary` into one block, but the
// frontend receives incremental updates as either event arrives. If
// the last block is a Compact whose payload is compatible (same
// trigger, no newer-than-stream regressions) we fold the fresh
// fields in; otherwise we append a new block. Two compactions in
// one turn (rare, but possible on very long turns) show as two
// separate blocks.
export function applyCompactUpdate(
  blocks: ContentBlock[] | undefined,
  update: {
    trigger: "auto" | "manual";
    preTokens?: number;
    postTokens?: number;
    durationMs?: number;
    summary?: string;
  },
): ContentBlock[] {
  const list = blocks ?? [];
  const last = list[list.length - 1];
  if (last && last.kind === "compact") {
    const merged: ContentBlock = {
      kind: "compact",
      trigger: update.trigger,
      preTokens: update.preTokens ?? last.preTokens,
      postTokens: update.postTokens ?? last.postTokens,
      durationMs: update.durationMs ?? last.durationMs,
      summary: update.summary ?? last.summary,
    };
    return [...list.slice(0, -1), merged];
  }
  return [
    ...list,
    {
      kind: "compact",
      trigger: update.trigger,
      preTokens: update.preTokens,
      postTokens: update.postTokens,
      durationMs: update.durationMs,
      summary: update.summary,
    },
  ];
}

// Apply a single runtime event to a turns array and return the
// next-state turns. Returns the same array reference when the event
// doesn't apply to any known turn, so callers can bail out and avoid
// a wasted re-render.
export function applyEventToTurns(
  prev: TurnRecord[],
  event: RuntimeEvent,
): TurnRecord[] {
  switch (event.type) {
    case "turn_started":
    case "turn_completed": {
      const exists = prev.some((t) => t.turnId === event.turn.turnId);
      if (exists) {
        return prev.map((t) =>
          t.turnId === event.turn.turnId ? event.turn : t,
        );
      }
      return [...prev, event.turn];
    }
    case "content_delta":
      return prev.map((t) =>
        t.turnId === event.turn_id
          ? {
              ...t,
              output: event.accumulated_output,
              blocks: appendTextDelta(t.blocks, event.delta),
            }
          : t,
      );
    case "reasoning_delta":
      return prev.map((t) =>
        t.turnId === event.turn_id
          ? {
              ...t,
              reasoning: (t.reasoning ?? "") + event.delta,
              blocks: appendReasoningDelta(t.blocks, event.delta),
            }
          : t,
      );
    case "compact_updated":
      return prev.map((t) =>
        t.turnId === event.turn_id
          ? {
              ...t,
              blocks: applyCompactUpdate(t.blocks, {
                trigger: event.trigger,
                preTokens: event.pre_tokens,
                postTokens: event.post_tokens,
                durationMs: event.duration_ms,
                summary: event.summary,
              }),
            }
          : t,
      );
    case "memory_recalled":
      return prev.map((t) =>
        t.turnId === event.turn_id
          ? {
              ...t,
              blocks: [
                ...(t.blocks ?? []),
                {
                  kind: "memory_recall",
                  mode: event.mode,
                  memories: event.memories,
                },
              ],
            }
          : t,
      );
    case "tool_call_started":
      return prev.map((t) =>
        t.turnId === event.turn_id
          ? {
              ...t,
              toolCalls: [
                ...(t.toolCalls ?? []),
                {
                  callId: event.call_id,
                  name: event.name,
                  args: event.args,
                  status: "pending" as const,
                  parentCallId: event.parent_call_id,
                },
              ],
              blocks: [
                ...(t.blocks ?? []),
                { kind: "tool_call", callId: event.call_id },
              ],
            }
          : t,
      );
    case "tool_call_completed":
      return prev.map((t) => {
        if (t.turnId !== event.turn_id || !t.toolCalls) return t;
        // Identity-check the toolCalls array: if the target call id
        // isn't present, skip the re-allocation so React.memo /
        // reference equality can bail on unrelated turns' subtrees.
        // Without this, every completion on a turn we don't hold
        // still allocates a fresh `toolCalls.map(...)` and forces
        // a render pass down the turn view.
        const hit = t.toolCalls.some((tc) => tc.callId === event.call_id);
        if (!hit) return t;
        return {
          ...t,
          toolCalls: t.toolCalls.map((tc) =>
            tc.callId === event.call_id
              ? {
                  ...tc,
                  output: event.output,
                  error: event.error,
                  status: event.error
                    ? ("failed" as const)
                    : ("completed" as const),
                }
              : tc,
          ),
        };
      });
    // Per-tool heartbeat from a provider that opted into
    // ProviderFeatures.toolProgress (Claude SDK today). We just
    // stamp lastProgressAt on the matching tool call; the
    // tool-call card watches that field against wall time and
    // shows a "no progress · Ns" pip when it goes stale, while
    // the stuck banner stays out of the way for tools that are
    // still ticking. Unknown call_ids are silently ignored —
    // usually means the heartbeat raced ahead of
    // tool_call_started by a frame.
    case "tool_progress":
      return prev.map((t) => {
        if (t.turnId !== event.turn_id || !t.toolCalls) return t;
        return {
          ...t,
          toolCalls: t.toolCalls.map((tc) =>
            tc.callId === event.call_id
              ? { ...tc, lastProgressAt: event.occurred_at }
              : tc,
          ),
        };
      });
    // Subagent lifecycle. Previously these only landed via the
    // whole-turn refetch triggered by turn_completed, so the
    // subagent box stayed empty during long-running dispatches.
    // Handling them here lets the UI stream the subagent's state
    // (including its per-agent model, once observed) live.
    case "subagent_started":
      return prev.map((t) =>
        t.turnId === event.turn_id
          ? {
              ...t,
              subagents: [
                ...(t.subagents ?? []),
                {
                  agentId: event.agent_id,
                  parentCallId: event.parent_call_id,
                  agentType: event.agent_type,
                  prompt: event.prompt,
                  model: event.model,
                  events: [],
                  status: "running" as const,
                },
              ],
            }
          : t,
      );
    case "subagent_event":
      return prev.map((t) => {
        if (t.turnId !== event.turn_id || !t.subagents) return t;
        return {
          ...t,
          subagents: t.subagents.map((s) =>
            s.agentId === event.agent_id
              ? { ...s, events: [...s.events, event.event] }
              : s,
          ),
        };
      });
    case "subagent_completed":
      return prev.map((t) => {
        if (t.turnId !== event.turn_id || !t.subagents) return t;
        return {
          ...t,
          subagents: t.subagents.map((s) =>
            s.agentId === event.agent_id
              ? {
                  ...s,
                  output: event.output,
                  error: event.error,
                  status: event.error
                    ? ("failed" as const)
                    : ("completed" as const),
                }
              : s,
          ),
        };
      });
    case "subagent_model_observed":
      return prev.map((t) => {
        if (t.turnId !== event.turn_id || !t.subagents) return t;
        return {
          ...t,
          subagents: t.subagents.map((s) =>
            s.agentId === event.agent_id ? { ...s, model: event.model } : s,
          ),
        };
      });
    // Incremental usage snapshots land on the in-flight turn so the
    // ContextDisplay popover updates as each API call in the turn's
    // tool loop completes. Without this, `turn.usage` only gets set
    // on `turn_completed` — on an 11-minute turn that means 11
    // minutes of a frozen numerator. See provider-claude-sdk bridge
    // which now emits `turn_usage` per assistant message carrying
    // the LATEST call's input/cache (not the aggregated sum that
    // inflated the display past the window).
    case "turn_usage_updated":
      return prev.map((t) =>
        t.turnId === event.turn_id ? { ...t, usage: event.usage } : t,
      );
    default:
      return prev;
  }
}
