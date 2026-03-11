import * as React from "react";
import { Virtuoso, type VirtuosoHandle } from "react-virtuoso";
import { ArrowDown, Loader2 } from "lucide-react";
import type { ContentBlock, TurnRecord } from "@/lib/types";
import { TurnView, type MessageItem } from "./turn-view";

interface MessageListProps {
  turns: TurnRecord[];
  loading: boolean;
  pendingInput: string | null;
  /** Number of older turns the daemon still has that haven't been
   *  fetched yet. Zero means the full history is in memory. Non-zero
   *  turns on the "Load older" button above the message list. */
  hiddenOlderCount?: number;
  /** True while a `loadFullSession` round-trip is in flight. Lets
   *  the "Load older" button swap to a spinner without remounting. */
  loadingOlder?: boolean;
  /** Triggered when the user clicks "Load older". Chat-view owns the
   *  query cache mutation; the list component is just a button. */
  onLoadOlder?: () => void;
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
  hiddenOlderCount = 0,
  loadingOlder = false,
  onLoadOlder,
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

  // Cold-cache case: no turns in hand yet AND the daemon hasn't
  // replied. Render a small loader inline in the scroll region
  // rather than an early return that unmounts Virtuoso — that way
  // the chat shell (header, composer, toolbar) stays mounted and
  // interactive while we wait, and a rapidly-switched-back-to
  // thread that *does* have cached turns skips this branch entirely.
  const showColdLoader = loading && displayItems.length === 0;

  return (
    <div className="relative min-h-0 flex-1">
      {showColdLoader && (
        <div className="pointer-events-none absolute inset-0 z-10 flex items-center justify-center text-xs text-muted-foreground">
          <Loader2 className="mr-2 h-3 w-3 animate-spin" />
          Loading thread…
        </div>
      )}
      {hiddenOlderCount > 0 && !showColdLoader && (
        <div className="absolute left-1/2 top-2 z-20 -translate-x-1/2">
          <button
            type="button"
            disabled={loadingOlder || !onLoadOlder}
            onClick={onLoadOlder}
            className="inline-flex items-center gap-1.5 rounded-full border border-border bg-background/90 px-3 py-1 text-[11px] text-muted-foreground shadow-sm backdrop-blur hover:bg-accent disabled:opacity-70"
          >
            {loadingOlder ? (
              <>
                <Loader2 className="h-3 w-3 animate-spin" />
                Loading {hiddenOlderCount} older…
              </>
            ) : (
              <>Show {hiddenOlderCount} older turn{hiddenOlderCount === 1 ? "" : "s"}</>
            )}
          </button>
        </div>
      )}
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
