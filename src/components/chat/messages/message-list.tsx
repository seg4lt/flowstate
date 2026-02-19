import * as React from "react";
import { Virtuoso, type VirtuosoHandle } from "react-virtuoso";
import { ArrowDown } from "lucide-react";
import type { TurnRecord } from "@/lib/types";
import { TurnView, type MessageItem } from "./turn-view";

interface StreamingTurn {
  turnId: string;
  accumulatedOutput: string;
  accumulatedReasoning: string;
}

interface MessageListProps {
  turns: TurnRecord[];
  streaming: StreamingTurn | null;
  loading: boolean;
  pendingInput: string | null;
}

const PENDING_KEY = "__pending__";

function turnToItem(turn: TurnRecord): MessageItem {
  return {
    turnId: turn.turnId,
    input: turn.input,
    output: turn.output,
    reasoning: turn.reasoning ?? null,
    status: turn.status,
    toolCalls: turn.toolCalls ?? null,
    streaming: false,
  };
}

// Merge a streaming entry into the matching turn so the user message
// (already in turns[] from turn_started) and the streaming agent
// output render as a single item. Without this they'd render as two
// separate rows that visibly merge on turn_completed.
function mergeStreaming(turn: TurnRecord, s: StreamingTurn): MessageItem {
  return {
    turnId: turn.turnId,
    input: turn.input,
    output: s.accumulatedOutput || turn.output,
    reasoning: s.accumulatedReasoning || turn.reasoning || null,
    status: turn.status,
    toolCalls: turn.toolCalls ?? null,
    streaming: true,
  };
}

function pendingItem(input: string): MessageItem {
  return {
    turnId: PENDING_KEY,
    input,
    output: "",
    reasoning: null,
    status: "running",
    toolCalls: null,
    streaming: true,
  };
}

const EmptyPlaceholder = () => (
  <div className="flex h-full items-center justify-center p-8 text-sm text-muted-foreground">
    Send a message to start the conversation.
  </div>
);

export function MessageList({
  turns,
  streaming,
  loading,
  pendingInput,
}: MessageListProps) {
  const virtuosoRef = React.useRef<VirtuosoHandle>(null);
  const [atBottom, setAtBottom] = React.useState(true);

  // Compose display items in three layers, in order:
  //   1. Completed turns from turns[]. If a streaming entry matches a
  //      turn by turnId, merge its accumulated output INTO that turn so
  //      the user message + streaming agent output render as one item.
  //   2. Pending optimistic row (the user just hit send, daemon hasn't
  //      sent turn_started yet). Cleared on turn_started.
  //   3. (Edge) An orphan streaming entry whose turnId doesn't match
  //      any turn in turns[] — shouldn't normally happen now that
  //      turn_started populates turns[], but kept defensively so a
  //      misordered event sequence still renders something sensible.
  const displayItems = React.useMemo<MessageItem[]>(() => {
    const items: MessageItem[] = [];
    const streamingTurnId = streaming?.turnId ?? null;
    let mergedStreaming = false;

    for (const turn of turns) {
      if (streaming && turn.turnId === streamingTurnId) {
        items.push(mergeStreaming(turn, streaming));
        mergedStreaming = true;
      } else {
        items.push(turnToItem(turn));
      }
    }

    if (pendingInput !== null) {
      items.push(pendingItem(pendingInput));
    }

    if (streaming && !mergedStreaming) {
      items.push({
        turnId: streaming.turnId,
        input: null,
        output: streaming.accumulatedOutput,
        reasoning: streaming.accumulatedReasoning || null,
        status: "running",
        toolCalls: null,
        streaming: true,
      });
    }

    return items;
  }, [turns, streaming, pendingInput]);

  if (loading) {
    return (
      <div className="flex flex-1 items-center justify-center text-sm text-muted-foreground">
        Loading...
      </div>
    );
  }

  return (
    <div className="relative min-h-0 flex-1">
      <Virtuoso
        ref={virtuosoRef}
        className="h-full"
        data={displayItems}
        computeItemKey={(_, item) => item.turnId}
        itemContent={(_, item) => (
          <div className="mx-auto max-w-3xl px-4 py-2">
            <TurnView item={item} />
          </div>
        )}
        followOutput={(isAtBottom) => (isAtBottom ? "auto" : false)}
        atBottomThreshold={80}
        atBottomStateChange={setAtBottom}
        initialTopMostItemIndex={Math.max(0, displayItems.length - 1)}
        increaseViewportBy={{ top: 600, bottom: 600 }}
        components={{ EmptyPlaceholder }}
      />

      {!atBottom && displayItems.length > 0 && (
        <button
          type="button"
          onClick={() => {
            virtuosoRef.current?.scrollToIndex({
              index: "LAST",
              align: "end",
              behavior: "smooth",
            });
          }}
          className="absolute right-4 bottom-4 inline-flex items-center gap-1 rounded-full border border-border bg-background px-3 py-1.5 text-xs shadow-md hover:bg-accent"
        >
          <ArrowDown className="h-3 w-3" />
          Jump to latest
        </button>
      )}
    </div>
  );
}
