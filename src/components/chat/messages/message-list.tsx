import * as React from "react";
import { Virtuoso, type VirtuosoHandle } from "react-virtuoso";
import { ArrowDown } from "lucide-react";
import type { ContentBlock, TurnRecord } from "@/lib/types";
import { TurnView, type MessageItem } from "./turn-view";

interface MessageListProps {
  turns: TurnRecord[];
  loading: boolean;
  pendingInput: string | null;
}

const PENDING_KEY = "__pending__";
const EMPTY_BLOCKS: ContentBlock[] = [];

function turnToItem(turn: TurnRecord): MessageItem {
  return {
    turnId: turn.turnId,
    input: turn.input,
    status: turn.status,
    blocks: turn.blocks ?? EMPTY_BLOCKS,
    toolCalls: turn.toolCalls ?? null,
    streaming: turn.status === "running",
  };
}

function pendingItem(input: string): MessageItem {
  return {
    turnId: PENDING_KEY,
    input,
    status: "running",
    blocks: EMPTY_BLOCKS,
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
  loading,
  pendingInput,
}: MessageListProps) {
  const virtuosoRef = React.useRef<VirtuosoHandle>(null);
  const [atBottom, setAtBottom] = React.useState(true);

  // chat-view maintains turn.blocks live during streaming, so a single
  // pass over turns[] is enough. The pending optimistic row covers the
  // gap between sendMessage and turn_started; it's cleared the moment
  // chat-view sees turn_started arrive.
  const displayItems = React.useMemo<MessageItem[]>(() => {
    const items: MessageItem[] = turns.map(turnToItem);
    if (pendingInput !== null) {
      items.push(pendingItem(pendingInput));
    }
    return items;
  }, [turns, pendingInput]);

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
          <div className="px-6 py-2">
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
