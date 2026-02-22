import * as React from "react";
import type { ContentBlock, ToolCall, TurnStatus } from "@/lib/types";
import { ToolCallCard } from "../tool-call-card";
import { UserMessage } from "./user-message";
import { AgentMessage } from "./agent-message";

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

  // Find the trailing text block so the blinking cursor only attaches
  // to the very last text run while the turn is still streaming.
  const lastTextIdx = React.useMemo(() => {
    for (let i = item.blocks.length - 1; i >= 0; i--) {
      if (item.blocks[i].kind === "text") return i;
    }
    return -1;
  }, [item.blocks]);

  const hasAnyContent = item.blocks.length > 0;

  return (
    <div className="space-y-3">
      {item.input !== null && <UserMessage input={item.input} />}

      {!hasAnyContent && item.streaming && (
        <div className="text-sm text-muted-foreground">
          <span className="animate-pulse">Thinking…</span>
        </div>
      )}

      {item.blocks.map((block, idx) => {
        switch (block.kind) {
          case "text":
            return (
              <AgentMessage
                key={`text-${idx}`}
                output={block.text}
                streaming={item.streaming && idx === lastTextIdx}
                status={item.status}
              />
            );
          case "reasoning":
            return (
              <details
                key={`reasoning-${idx}`}
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
          case "tool_call": {
            const tc = callsById.get(block.callId);
            if (!tc) return null;
            return (
              <div key={`tool-${block.callId}`} className="pl-2">
                <ToolCallCard toolCall={tc} />
              </div>
            );
          }
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
