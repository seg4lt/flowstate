import * as React from "react";
import { Virtuoso, type VirtuosoHandle } from "react-virtuoso";
import { ArrowDown, Loader2 } from "lucide-react";
import type {
  AttachmentRef,
  ContentBlock,
  ProviderKind,
  TurnRecord,
} from "@/lib/types";
import { TurnView, type MessageItem } from "./turn-view";

interface MessageListProps {
  turns: TurnRecord[];
  loading: boolean;
  pendingInput: string | null;
  onOpenAttachment?: (attachment: AttachmentRef) => void;
  /** Identity of the currently-visible session. Used as the
   *  scroll-reset trigger so switching threads always lands the
   *  user at the bottom-most (latest) message, even though
   *  MessageList itself doesn't remount between sessions. */
  sessionId: string;
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
  /** Monotonically-increasing counter bumped by chat-view every time the
   *  user dispatches a message. A change here forces an unconditional
   *  smooth scroll to the latest item, even if the user had scrolled up
   *  to read history. Decoupled from `pendingInput` so the effect fires
   *  exactly once per send and doesn't get skipped if React batches. */
  userSendTick: number;
  /** Provider kind of the current session — used to label the model
   *  info popover on each agent reply. */
  providerKind?: ProviderKind;
  /** Session-level configured model. Used as the per-turn model
   *  fallback when `turn.usage.model` hasn't been populated yet
   *  (happens mid-stream and on very old rows). */
  sessionModel?: string;
}

const PENDING_KEY = "__pending__";
const EMPTY_BLOCKS: ContentBlock[] = [];

function turnToItem(
  turn: TurnRecord,
  providerKind: ProviderKind | undefined,
  sessionModel: string | undefined,
): MessageItem {
  return {
    turnId: turn.turnId,
    input: turn.input,
    status: turn.status,
    blocks: turn.blocks ?? EMPTY_BLOCKS,
    toolCalls: turn.toolCalls ?? null,
    streaming: turn.status === "running",
    inputAttachments: turn.inputAttachments,
    durationMs: turn.usage?.durationMs,
    // Prefer the pinned resolved model (authoritative post-turn)
    // over the session-configured alias. Either is fine for display;
    // the pinned id is better because it encodes exactly which
    // build answered this specific turn.
    model: turn.usage?.model ?? sessionModel,
    providerKind,
    subagents: turn.subagents,
  };
}

function pendingItem(
  input: string,
  providerKind: ProviderKind | undefined,
  sessionModel: string | undefined,
): MessageItem {
  return {
    turnId: PENDING_KEY,
    input,
    status: "running",
    blocks: EMPTY_BLOCKS,
    toolCalls: null,
    streaming: true,
    model: sessionModel,
    providerKind,
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
  sessionId,
  hiddenOlderCount = 0,
  loadingOlder = false,
  onLoadOlder,
  onOpenAttachment,
  userSendTick,
  providerKind,
  sessionModel,
}: MessageListProps) {
  const virtuosoRef = React.useRef<VirtuosoHandle>(null);
  const [atBottom, setAtBottom] = React.useState(true);
  // `suppressJump` hides the "Jump to latest" affordance while a user-
  // initiated send is actively being scrolled to the bottom. Without
  // this, Virtuoso's `atBottomStateChange` fires `false` the moment the
  // optimistic pending row grows the list past the 80px threshold, and
  // the button flashes for the duration of the smooth scroll animation.
  const [suppressJump, setSuppressJump] = React.useState(false);
  const suppressTimerRef = React.useRef<ReturnType<typeof setTimeout> | null>(
    null,
  );

  // chat-view maintains turn.blocks live during streaming, so a single
  // pass over turns[] is enough. The pending optimistic row covers the
  // gap between sendMessage and turn_started; it's cleared the moment
  // chat-view sees turn_started arrive.
  const displayItems = React.useMemo<MessageItem[]>(() => {
    const items: MessageItem[] = turns.map((t) =>
      turnToItem(t, providerKind, sessionModel),
    );
    if (pendingInput !== null) {
      items.push(pendingItem(pendingInput, providerKind, sessionModel));
    }
    return items;
  }, [turns, pendingInput, providerKind, sessionModel]);

  // Jump to the latest message whenever the user navigates to a
  // new thread. MessageList doesn't remount between sessions (that
  // design decision is what makes re-visits render instantly), so
  // Virtuoso's `initialTopMostItemIndex` — which only applies on
  // mount — can't do this on its own. A ref-tracked sessionId
  // drives the imperative scroll once the target session's items
  // are actually in the list; without the length check the first
  // render after a click would try to scroll an empty virtuoso
  // and the call silently no-ops.
  const scrolledForSessionRef = React.useRef<string | null>(null);
  React.useEffect(() => {
    if (displayItems.length === 0) return;
    if (scrolledForSessionRef.current === sessionId) return;
    scrolledForSessionRef.current = sessionId;
    // Defer one frame so Virtuoso has a chance to measure the
    // items that just arrived. Without the frame the scrollToIndex
    // call fires before layout and gets dropped.
    const raf = requestAnimationFrame(() => {
      virtuosoRef.current?.scrollToIndex({
        index: "LAST",
        align: "end",
        behavior: "auto",
      });
    });
    return () => cancelAnimationFrame(raf);
  }, [sessionId, displayItems.length]);

  // Force a scroll to the latest message every time the user dispatches
  // a new message. Unlike Virtuoso's `followOutput`, this fires even
  // when the user is scrolled up reading history — sending a message
  // should always pop them back to the bottom so they can see their
  // own input land. The ref guard makes the "skip initial mount" intent
  // explicit and protects against StrictMode double-invocation in dev.
  // The rAF defer matches the thread-open effect: Virtuoso needs a
  // layout pass after `displayItems` grows by the optimistic pending row.
  const lastSendTickRef = React.useRef(userSendTick);
  React.useEffect(() => {
    if (userSendTick === lastSendTickRef.current) return;
    lastSendTickRef.current = userSendTick;
    // Hide the "Jump to latest" affordance briefly while we jump. 400ms
    // is plenty for an instant scroll to settle and for Virtuoso's
    // `atBottomStateChange(true)` to fire; after the timer clears,
    // `atBottom` is trustworthy again.
    setSuppressJump(true);
    if (suppressTimerRef.current) clearTimeout(suppressTimerRef.current);
    suppressTimerRef.current = setTimeout(() => setSuppressJump(false), 400);
    const raf = requestAnimationFrame(() => {
      // `behavior: "auto"` (instant) is required here. A smooth scroll
      // across Virtuoso's virtualized list leaves the in-between items
      // unrendered during the animation — the user sees a blank/black
      // viewport until a re-render is triggered (e.g., by manual
      // scroll). Instant scroll teleports to the new position and
      // Virtuoso immediately renders the items in the destination
      // window. This matches the thread-open effect above.
      virtuosoRef.current?.scrollToIndex({
        index: "LAST",
        align: "end",
        behavior: "auto",
      });
    });
    return () => cancelAnimationFrame(raf);
  }, [userSendTick]);

  // Clear any pending suppression timer on unmount so we don't
  // setState on a torn-down component (e.g., if the user navigates
  // away mid-send).
  React.useEffect(() => {
    return () => {
      if (suppressTimerRef.current) clearTimeout(suppressTimerRef.current);
    };
  }, []);

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
            <TurnView item={item} onOpenAttachment={onOpenAttachment} />
          </div>
        )}
        followOutput={(isAtBottom) => (isAtBottom ? "auto" : false)}
        atBottomThreshold={80}
        atBottomStateChange={setAtBottom}
        initialTopMostItemIndex={Math.max(0, displayItems.length - 1)}
        increaseViewportBy={{ top: 600, bottom: 600 }}
        components={{ EmptyPlaceholder }}
      />

      {!atBottom && !suppressJump && displayItems.length > 0 && (
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
