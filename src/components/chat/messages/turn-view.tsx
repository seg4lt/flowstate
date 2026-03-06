import * as React from "react";
import { ChevronDown, ChevronUp } from "lucide-react";
import type { ContentBlock, ToolCall, TurnStatus } from "@/lib/types";
import { ToolCallCard } from "../tool-call-card";
import { UserMessage } from "./user-message";
import { AgentMessage } from "./agent-message";

const GROUP_DEFAULT_VISIBLE = 5;

// A render-layer block that consolidates consecutive `tool_call`
// blocks issued by the same agent into a single group. A change in
// `parentCallId` (main → sub-agent, sub-agent → main, or between two
// different sub-agents) starts a new group, as does any intervening
// text/reasoning block.
type RenderBlock =
  | { kind: "text"; text: string; key: string }
  | { kind: "reasoning"; text: string; key: string }
  | {
      kind: "tool_call_group";
      callIds: string[];
      parentCallId: string | undefined;
      key: string;
    };

function groupBlocks(
  blocks: ContentBlock[],
  callsById: Map<string, ToolCall>,
): RenderBlock[] {
  const result: RenderBlock[] = [];

  // Main-agent grouping: sequential, breaks on any non-tool block or
  // when a sub-agent tool call interrupts the streak. The reference is
  // mutated in place — pushing once into result and then appending to
  // .callIds keeps the box stable across streaming updates.
  let currentMainGroup:
    | {
        kind: "tool_call_group";
        callIds: string[];
        parentCallId: string | undefined;
        key: string;
      }
    | null = null;

  // Sub-agent boxes: one per parentCallId, deduped via this Map. The
  // first time a sub-agent's tool call appears in the stream, its box
  // is created and pushed into result at that position; every later
  // tool call from the SAME sub-agent — even if other content
  // (main-agent tools, text, reasoning) intervenes — appends to the
  // same in-place callIds array, so the user sees one persistent box
  // per sub-agent that collects all of its activity in stream order.
  // Parallel sub-agents land in separate boxes because each has a
  // different parentCallId.
  const subagentBoxes = new Map<
    string,
    {
      kind: "tool_call_group";
      callIds: string[];
      parentCallId: string | undefined;
      key: string;
    }
  >();

  blocks.forEach((block, idx) => {
    if (block.kind === "tool_call") {
      const parent = callsById.get(block.callId)?.parentCallId;

      if (parent === undefined) {
        // Main agent — sequential grouping.
        if (currentMainGroup) {
          currentMainGroup.callIds.push(block.callId);
          return;
        }
        currentMainGroup = {
          kind: "tool_call_group",
          callIds: [block.callId],
          parentCallId: undefined,
          key: `tg-${block.callId}`,
        };
        result.push(currentMainGroup);
        return;
      }

      // Sub-agent — find or create the persistent box for this parent.
      // A sub-agent block always breaks the current main-agent streak,
      // so the next main tool call starts fresh.
      currentMainGroup = null;
      const existing = subagentBoxes.get(parent);
      if (existing) {
        existing.callIds.push(block.callId);
        return;
      }
      const box = {
        kind: "tool_call_group" as const,
        callIds: [block.callId],
        parentCallId: parent,
        // Keyed by parentCallId so the expanded state stays stable as
        // more tool calls get appended over the life of the sub-agent.
        key: `tg-sub-${parent}`,
      };
      subagentBoxes.set(parent, box);
      result.push(box);
      return;
    }

    // Any non-tool block (text, reasoning) breaks the main-agent
    // streak. Sub-agent boxes are unaffected — they keep collecting
    // across these interruptions because their identity is the
    // parentCallId, not stream contiguity.
    currentMainGroup = null;
    if (block.kind === "text") {
      result.push({ kind: "text", text: block.text, key: `text-${idx}` });
    } else if (block.kind === "reasoning") {
      result.push({
        kind: "reasoning",
        text: block.text,
        key: `reasoning-${idx}`,
      });
    }
  });
  return result;
}

function ToolCallGroup({
  callIds,
  parentCallId,
  callsById,
}: {
  callIds: string[];
  parentCallId: string | undefined;
  callsById: Map<string, ToolCall>;
}) {
  const [expanded, setExpanded] = React.useState(false);

  const calls = React.useMemo(() => {
    const out: ToolCall[] = [];
    for (const id of callIds) {
      const tc = callsById.get(id);
      if (tc) out.push(tc);
    }
    return out;
  }, [callIds, callsById]);

  if (calls.length === 0) return null;

  const overflow = calls.length - GROUP_DEFAULT_VISIBLE;
  const hasOverflow = overflow > 0;
  const visible =
    expanded || !hasOverflow ? calls : calls.slice(0, GROUP_DEFAULT_VISIBLE);

  // Sub-agent groups get a visible header so the user can see which
  // dispatch issued them and which agent type is running. The agent
  // type lives in the spawning Task tool's args (`subagent_type`),
  // and that Task tool call is in callsById keyed by parentCallId
  // because parentCallId is the call_id of the Task that spawned
  // this sub-agent. Falls back to the tool name and finally to
  // "Subagent" if neither is available (e.g. a sub-agent whose
  // parent Task call hasn't streamed in yet).
  const isSubagent = parentCallId !== undefined;
  const parentCall = isSubagent ? callsById.get(parentCallId) : undefined;
  const subagentType = isSubagent
    ? (parentCall?.args as { subagent_type?: string } | undefined)
        ?.subagent_type
    : undefined;
  const agentLabel = subagentType ?? parentCall?.name ?? "Subagent";

  const body = (
    <div className="space-y-1">
      {visible.map((tc) => (
        <ToolCallCard key={tc.callId} toolCall={tc} />
      ))}
      {hasOverflow && (
        <button
          type="button"
          onClick={() => setExpanded((e) => !e)}
          className="inline-flex items-center gap-1 rounded-md px-2 py-0.5 text-[11px] text-muted-foreground hover:bg-muted/50 hover:text-foreground"
        >
          {expanded ? (
            <>
              <ChevronUp className="h-3 w-3" />
              Show top {GROUP_DEFAULT_VISIBLE}
            </>
          ) : (
            <>
              <ChevronDown className="h-3 w-3" />
              Show {overflow} more
            </>
          )}
        </button>
      )}
    </div>
  );

  if (isSubagent) {
    return (
      <div className="rounded-md border border-border/50 bg-muted/20 px-2 py-1.5">
        <div className="mb-1 flex items-center gap-1.5 text-[10px] font-medium uppercase tracking-wide text-muted-foreground">
          <span>↳ {agentLabel}</span>
          <span className="text-muted-foreground/60">· {calls.length}</span>
        </div>
        {body}
      </div>
    );
  }

  return <div className="pl-2">{body}</div>;
}

// Normalized shape that covers both completed TurnRecords and the
// synthetic streaming row. `input` is null for streaming items because
// the store does not expose the pending user input — matches the
// previous behavior where the user message only appears after
// turn_completed fires.
export interface MessageItem {
  turnId: string;
  input: string | null;
  status: TurnStatus;
  // Canonical ordered content stream — text, reasoning, and tool-call
  // positions in the order the provider emitted them. Tool-call blocks
  // reference toolCalls[] by callId.
  blocks: ContentBlock[];
  toolCalls: ToolCall[] | null;
  streaming: boolean;
}

interface TurnViewProps {
  item: MessageItem;
}

function TurnViewInner({ item }: TurnViewProps) {
  const callsById = React.useMemo(() => {
    const map = new Map<string, ToolCall>();
    for (const tc of item.toolCalls ?? []) map.set(tc.callId, tc);
    return map;
  }, [item.toolCalls]);

  const renderBlocks = React.useMemo(
    () => groupBlocks(item.blocks, callsById),
    [item.blocks, callsById],
  );

  // Index of the trailing text block in the grouped stream so the
  // blinking cursor only attaches to the very last text run while
  // the turn is still streaming.
  const lastTextRenderIdx = React.useMemo(() => {
    for (let i = renderBlocks.length - 1; i >= 0; i--) {
      if (renderBlocks[i].kind === "text") return i;
    }
    return -1;
  }, [renderBlocks]);

  const hasAnyContent = item.blocks.length > 0;

  return (
    <div className="space-y-3">
      {item.input !== null && <UserMessage input={item.input} />}

      {!hasAnyContent && item.streaming && (
        <div className="text-sm text-muted-foreground">
          <span className="animate-pulse">Thinking…</span>
        </div>
      )}

      {renderBlocks.map((block, idx) => {
        switch (block.kind) {
          case "text":
            return (
              <AgentMessage
                key={block.key}
                output={block.text}
                streaming={item.streaming && idx === lastTextRenderIdx}
                status={item.status}
              />
            );
          case "reasoning":
            return (
              <details
                key={block.key}
                open
                className="rounded-md border border-border/50 bg-muted/30 px-3 py-1.5 text-xs"
              >
                <summary className="cursor-pointer select-none text-muted-foreground hover:text-foreground">
                  Reasoning
                </summary>
                <p className="mt-2 whitespace-pre-wrap italic text-muted-foreground">
                  {block.text}
                </p>
              </details>
            );
          case "tool_call_group":
            return (
              <ToolCallGroup
                key={block.key}
                callIds={block.callIds}
                parentCallId={block.parentCallId}
                callsById={callsById}
              />
            );
        }
      })}
    </div>
  );
}

export const TurnView = React.memo(TurnViewInner, (prev, next) => {
  const a = prev.item;
  const b = next.item;
  return (
    a.turnId === b.turnId &&
    a.input === b.input &&
    a.status === b.status &&
    a.streaming === b.streaming &&
    // Reference equality on both arrays — chat-view always builds new
    // arrays when blocks or tool calls change, so this catches every
    // streaming update.
    a.blocks === b.blocks &&
    a.toolCalls === b.toolCalls
  );
});
