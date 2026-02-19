import * as React from "react";
import type { ToolCall, TurnStatus } from "@/lib/types";
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
  output: string;
  reasoning: string | null;
  status: TurnStatus;
  toolCalls: ToolCall[] | null;
  streaming: boolean;
}

interface TurnViewProps {
  item: MessageItem;
}

function TurnViewInner({ item }: TurnViewProps) {
  return (
    <div className="space-y-3">
      {item.input !== null && <UserMessage input={item.input} />}

      {(item.output || item.streaming) && (
        <AgentMessage
          output={item.output}
          reasoning={item.reasoning ?? undefined}
          streaming={item.streaming}
          status={item.status}
        />
      )}

      {item.toolCalls && item.toolCalls.length > 0 && (
        <div className="space-y-2 pl-2">
          {item.toolCalls.map((tc) => (
            <ToolCallCard key={tc.callId} toolCall={tc} />
          ))}
        </div>
      )}
    </div>
  );
}

export const TurnView = React.memo(TurnViewInner, (prev, next) => {
  const a = prev.item;
  const b = next.item;
  return (
    a.turnId === b.turnId &&
    a.input === b.input &&
    a.output === b.output &&
    a.reasoning === b.reasoning &&
    a.status === b.status &&
    a.streaming === b.streaming &&
    // Reference equality — chat-view always builds new arrays when a
    // tool call is added or completes, so this catches both length
    // changes and per-call status updates.
    a.toolCalls === b.toolCalls
  );
});
