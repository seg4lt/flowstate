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
  /** Cached preview of the most recent turn's output (from the app
   *  store's `sessionDisplay` map). When the cold-cache load is in
   *  flight we render this in place of the spinner so the user sees
   *  *something* familiar from the thread the moment they click,
   *  instead of a blank pane. Optional — old/empty threads have no
   *  preview, in which case the loader falls back to the spinner. */
  coldPreview?: string | null;
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
  coldPreview,
}: MessageListProps) {
  const virtuosoRef = React.useRef<VirtuosoHandle>(null);
  const [atBottom, setAtBottom] = React.useState(true);
  // `suppressJump` hides the "Jump to latest" affordance while a user-
  // initiated scroll-to-bottom is in flight. Without this, Virtuoso's
  // `atBottomStateChange` fires `false` the moment the list grows past
  // the atBottom threshold (e.g., an optimistic pending row after a
  // send, or tokens streaming in), and the button flashes until the
  // next `atBottomStateChange(true)` lands. See `scrollToLatest` below.
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

  // Single user-initiated "scroll to latest" implementation. Consolidates
  // the four invariants every user-driven jump needs:
  //   1. Bail if the list is transiently empty — otherwise Virtuoso
  //      resolves `index: "LAST"` against a zero-length data array and
  //      can land on index 0 (the TOP of the list).
  //   2. Suppress the "Jump to latest" affordance for 400ms so the
  //      button doesn't flash while Virtuoso's atBottomStateChange
  //      settles, and a double-click is a no-op.
  //   3. Retry the scroll across several frames. On a session switch
  //      (or any case where Virtuoso just received a new `data` prop),
  //      Virtuoso renders/measures asynchronously — a single rAF often
  //      fires before measurement, the scrollToIndex resolves against
  //      stale layout and silently no-ops, and Virtuoso stays parked
  //      at the previous scroll offset. With the new dataset that
  //      offset is usually past the end of content, so the viewport
  //      virtualises nothing → blank pane until the user scrolls.
  //      scrollToIndex with index: "LAST" is idempotent, so retrying
  //      across ~6 frames (~100 ms) is safe: the call lands as soon
  //      as Virtuoso is ready, and subsequent attempts are no-ops at
  //      the same offset (no flicker).
  //   4. `behavior: "auto"` (instant). Smooth scroll across Virtuoso's
  //      virtualized list leaves the in-between items unrendered
  //      during the animation, which is what made "Jump to latest"
  //      look like it was dumping users at the top.
  const scrollAttemptRef = React.useRef<{
    cancelled: boolean;
    frame: number;
  } | null>(null);
  const scrollToLatest = React.useCallback(() => {
    if (displayItems.length === 0) return;
    setSuppressJump(true);
    if (suppressTimerRef.current) clearTimeout(suppressTimerRef.current);
    suppressTimerRef.current = setTimeout(() => setSuppressJump(false), 400);

    // Cancel any in-flight retry chain from a previous call so we don't
    // pile up duplicate rAF loops on rapid thread switches.
    if (scrollAttemptRef.current) {
      scrollAttemptRef.current.cancelled = true;
      cancelAnimationFrame(scrollAttemptRef.current.frame);
    }
    const handle: { cancelled: boolean; frame: number } = {
      cancelled: false,
      frame: 0,
    };
    scrollAttemptRef.current = handle;
    let attempts = 0;
    const MAX_ATTEMPTS = 6;
    const tick = () => {
      if (handle.cancelled) return;
      const v = virtuosoRef.current;
      if (v) {
        v.scrollToIndex({
          index: "LAST",
          align: "end",
          behavior: "auto",
        });
      }
      attempts += 1;
      if (attempts < MAX_ATTEMPTS) {
        handle.frame = requestAnimationFrame(tick);
      }
    };
    handle.frame = requestAnimationFrame(tick);
  }, [displayItems.length]);

  // Always jump to the latest message when the user clicks a thread.
  // MessageList doesn't remount between sessions (that design choice is
  // what makes re-visits render instantly), so Virtuoso's
  // `initialTopMostItemIndex` — which only applies on mount — can't
  // drive this on its own.
  //
  // We re-scroll on every `sessionId` change rather than gating to
  // first-visit-only. Earlier code used a `Set<sessionId>` to dedup
  // the scroll, citing "huge thread re-measure lag" on revisits, but
  // Virtuoso virtualises by viewport — `scrollToIndex({index: "LAST"})`
  // is bounded by what's visible, not by total list length. The dedup
  // produced a worse UX (revisits could land mid-history at whatever
  // offset Virtuoso happened to be at, sometimes blank because the new
  // dataset's height didn't reach the parked offset) and contradicted
  // the natural user expectation that clicking a thread shows the
  // newest message.
  //
  // Cold-cache handling: the session may be selected before its turns
  // arrive, so we can't unconditionally fire on `sessionId` change —
  // an empty list resolves `index: "LAST"` to nothing useful. We track
  // the last sessionId we *successfully* drove a scroll for in a single-
  // slot ref. When sessionId changes, the slot mismatches; when items
  // arrive (length 0 → N) we stamp and scroll. Length changes within
  // the same session (streaming tokens, new turns) DON'T re-fire,
  // because the slot already matches — `followOutput` handles the
  // "you were at bottom, stay at bottom" case, and `userSendTick`
  // handles "user sent, force-jump".
  const lastScrolledSessionRef = React.useRef<string | null>(null);
  React.useEffect(() => {
    if (displayItems.length === 0) return;
    if (lastScrolledSessionRef.current === sessionId) return;
    lastScrolledSessionRef.current = sessionId;
    scrollToLatest();
  }, [sessionId, displayItems.length, scrollToLatest]);

  // Force a scroll to the latest message every time the user dispatches
  // a new message. Unlike Virtuoso's `followOutput`, this fires even
  // when the user is scrolled up reading history — sending a message
  // should always pop them back to the bottom so they can see their
  // own input land. The ref guard makes the "skip initial mount" intent
  // explicit and protects against StrictMode double-invocation in dev.
  // The actual scroll + suppress + rAF pattern lives in scrollToLatest
  // so the button-click path and this effect stay in lockstep.
  const lastSendTickRef = React.useRef(userSendTick);
  React.useEffect(() => {
    if (userSendTick === lastSendTickRef.current) return;
    lastSendTickRef.current = userSendTick;
    scrollToLatest();
  }, [userSendTick, scrollToLatest]);

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
    <div className="relative min-h-0 min-w-0 flex-1">
      {showColdLoader && (
        // Anchored to the bottom (where the latest turn will appear)
        // so the preview occupies roughly the same screen position the
        // real content will when it lands — minimises perceived layout
        // jump. `coldPreview` comes from the app-store's
        // `sessionDisplay.lastTurnPreview` cache that's already loaded
        // at app boot; rendering it instead of a centred spinner makes
        // the click *feel* immediate even when `load_session` is still
        // in flight. Old/empty threads with no preview fall back to
        // the centred spinner unchanged.
        <div className="pointer-events-none absolute inset-x-0 bottom-0 z-10 flex flex-col items-center px-6 pb-6">
          {coldPreview ? (
            <div className="w-full max-w-3xl rounded-md border border-dashed border-border/60 bg-background/40 px-4 py-3 text-sm text-muted-foreground">
              <div className="mb-1 flex items-center gap-2 text-[11px] uppercase tracking-wide text-muted-foreground/70">
                <Loader2 className="h-3 w-3 animate-spin" />
                Loading thread…
              </div>
              <p className="line-clamp-3 whitespace-pre-wrap break-words">
                {coldPreview}
              </p>
            </div>
          ) : (
            <div className="flex items-center text-xs text-muted-foreground">
              <Loader2 className="mr-2 h-3 w-3 animate-spin" />
              Loading thread…
            </div>
          )}
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
            <TurnView
              item={item}
              onOpenAttachment={onOpenAttachment}
            />
          </div>
        )}
        followOutput={(isAtBottom) => (isAtBottom ? "auto" : false)}
        atBottomThreshold={120}
        atBottomStateChange={setAtBottom}
        initialTopMostItemIndex={Math.max(0, displayItems.length - 1)}
        increaseViewportBy={{ top: 600, bottom: 600 }}
        components={{ EmptyPlaceholder }}
      />

      {!atBottom && !suppressJump && displayItems.length > 0 && (
        <button
          type="button"
          onClick={scrollToLatest}
          className="absolute right-4 bottom-4 inline-flex items-center gap-1 rounded-full border border-border bg-background px-3 py-1.5 text-xs shadow-md hover:bg-accent"
        >
          <ArrowDown className="h-3 w-3" />
          Jump to latest
        </button>
      )}
    </div>
  );
}
