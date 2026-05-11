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
  /** Identity of the currently-visible session. Threaded through to
   *  Virtuoso as `key={sessionId}` so each thread gets a fresh
   *  Virtuoso instance — that's how switching tabs reliably lands
   *  the user at the bottom-most (latest) message. MessageList
   *  itself does NOT remount between sessions; only Virtuoso does. */
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

// Settle-window tuning. See the long comment on `settlingRef` inside
// MessageList for the rationale; pulled out here so the numbers are
// easy to find and don't get recreated each render.
//
// SETTLE_HARD_CEILING_MS: max wall-clock ms from initial mount that
// `totalListHeightChanged` is allowed to keep re-pinning to LAST.
// Past this, we hand back to the `followOutput` path so live streaming
// (or a user that's scrolled up to read history mid-load) is not
// yanked.
//
// SETTLE_QUIET_MS: idle quiet duration the debounce waits for before
// declaring settling "done". Each height-change resets the timer.
// 400ms is wide enough to bridge the gap between back-to-back async
// Shiki highlights (~100-300ms cold per code block, see
// `code-block.tsx`) without staying open through an actively
// streaming turn.
const SETTLE_HARD_CEILING_MS = 5000;
const SETTLE_QUIET_MS = 400;

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
    // Authorship — drives the chip + muted-bubble variant on
    // wakeup / peer turns. Legacy turns from before the column
    // existed deserialize as `"user"` thanks to the Rust
    // `#[serde(default)]` on `TurnRecord::source`.
    source: turn.source,
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

  // "Settling window" refs — while `settlingRef` is true, the
  // `totalListHeightChanged` callback below re-pins to LAST so that
  // post-mount height measurements (markdown / reasoning / tool-call
  // groups resolving to their real heights via ResizeObserver) don't
  // leave the user stranded above the actual latest content. Virtuoso's
  // `initialTopMostItemIndex` anchors against *estimated* heights at
  // mount; once items measure, the resolved scroll offset drifts up.
  //
  // Why this is debounced, not a fixed one-shot timer: threads that
  // contain CompactBlocks (auto-compaction recap dividers) are long by
  // definition — they triggered the SDK's context compaction. Long
  // threads tend to carry many code-blocks, and `code-block.tsx`
  // highlights with Shiki asynchronously (~100-300ms cold per block,
  // see `ensureLanguageLoaded`). Cumulative re-measurement easily
  // outruns a fixed-duration window, leaving the user above the latest
  // turn. Instead, every `totalListHeightChanged` while settling resets
  // the close timer, so the window stays open as long as the list is
  // still growing. The hard ceiling against `initialMountTsRef` caps
  // the worst case — a stray height change minutes later (e.g., the
  // user expanding a CompactBlock summary themselves) won't yank
  // scroll.
  const settlingRef = React.useRef(false);
  const settleTimerRef = React.useRef<ReturnType<typeof setTimeout> | null>(
    null,
  );
  const initialMountTsRef = React.useRef(0);
  const mountedItemsLenRef = React.useRef(0);

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

  // Imperative scroll-to-latest used by the "Jump to latest" pill and the
  // userSendTick effect. Session-switch landing is NOT done here — it's
  // handled natively by Virtuoso via `key={sessionId}` + `initialTopMost-
  // ItemIndex={length-1}`, which is bulletproof because it runs once on
  // mount with the full data already in place. This callback only fires
  // for *user-initiated* jumps where the data is already laid out and a
  // single rAF is enough to let Virtuoso measure any newly-appended row
  // (e.g., the optimistic pending turn after `handleSend`).
  //
  // Notes:
  //   * Bail if the list is transiently empty — `index: "LAST"` against a
  //     zero-length data array can land on index 0 (the TOP of the list).
  //   * Suppress the "Jump to latest" affordance for 400 ms so the button
  //     doesn't flash while Virtuoso's atBottomStateChange settles, and
  //     a double-click is a no-op.
  //   * `behavior: "auto"` (instant). Smooth scroll across Virtuoso's
  //     virtualized list leaves the in-between items unrendered during
  //     the animation, which made "Jump to latest" look like it was
  //     dumping users at the top.
  const scrollToLatest = React.useCallback(() => {
    if (displayItems.length === 0) return;
    setSuppressJump(true);
    if (suppressTimerRef.current) clearTimeout(suppressTimerRef.current);
    suppressTimerRef.current = setTimeout(() => setSuppressJump(false), 400);
    requestAnimationFrame(() => {
      virtuosoRef.current?.scrollToIndex({
        index: "LAST",
        align: "end",
        behavior: "auto",
      });
    });
  }, [displayItems.length]);

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

  // Shared "bump the settle window" helper. Opens settling if it isn't
  // already, and (re)arms the close timer for SETTLE_QUIET_MS of
  // quiet. Called from: (a) the per-session-mount effect below,
  // (b) the cold-preview-growth effect, and (c) `totalListHeight-
  // Changed` itself while settling is active (so streaming async
  // measurements — Shiki highlight, late images, expanded reasoning
  // blocks — keep extending the window as long as content is still
  // re-laying-out).
  const bumpSettle = React.useCallback(() => {
    settlingRef.current = true;
    if (settleTimerRef.current) clearTimeout(settleTimerRef.current);
    settleTimerRef.current = setTimeout(() => {
      settlingRef.current = false;
    }, SETTLE_QUIET_MS);
  }, []);

  // Per-session-mount settle window. Runs once when sessionId changes —
  // Virtuoso has just re-mounted (`key={sessionId}`), its
  // `initialTopMostItemIndex` has fired against estimated heights, and
  // the first measurement pass is about to land. Opening the window
  // lets `totalListHeightChanged` events re-pin to the bottom so the
  // user lands on the *true* end of the latest turn, not on a position
  // estimated from ~30px-per-item heuristics. The window stays open as
  // long as content is still measuring (debounced via `bumpSettle`
  // from `totalListHeightChanged`); a hard ceiling enforced in the
  // callback caps it at SETTLE_HARD_CEILING_MS from mount.
  React.useEffect(() => {
    mountedItemsLenRef.current = displayItems.length;
    initialMountTsRef.current = Date.now();
    bumpSettle();
    return () => {
      if (settleTimerRef.current) clearTimeout(settleTimerRef.current);
      settlingRef.current = false;
    };
    // Intentionally only sessionId — we want a clean window per thread
    // switch, not per turns update.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [sessionId]);

  // Cold-preview → full-transcript hydration: `loadFullSession` replaces
  // the paginated tail with the full history, growing `displayItems`
  // without re-keying Virtuoso. Re-arm the settle window so the freshly-
  // rendered tail lands at the bottom. Only fires within ~1.5s of mount
  // so streaming growth that lands minutes later does NOT re-pin and
  // yank a history-reading user.
  React.useEffect(() => {
    if (displayItems.length <= mountedItemsLenRef.current) return;
    if (Date.now() - initialMountTsRef.current > 1500) return;
    mountedItemsLenRef.current = displayItems.length;
    bumpSettle();
  }, [displayItems.length, bumpSettle]);

  // Clear any pending suppression timer on unmount so we don't
  // setState on a torn-down component (e.g., if the user navigates
  // away mid-send).
  React.useEffect(() => {
    return () => {
      if (suppressTimerRef.current) clearTimeout(suppressTimerRef.current);
    };
  }, []);

  // Cold-cache case: no turns in hand yet AND the daemon hasn't
  // replied. We render a loader instead of Virtuoso — Virtuoso's
  // mount is intentionally gated on `displayItems.length > 0` so
  // that `initialTopMostItemIndex={length-1}` (see <Virtuoso/> below)
  // resolves against the *real* last item, not against an empty array
  // that would land it on index 0. The chat shell (header, composer,
  // toolbar) stays mounted regardless because they live above this
  // component in the tree.
  const showColdLoader = loading && displayItems.length === 0;
  // "Fresh thread, no messages yet" — the daemon has answered and
  // confirmed the thread is empty. Render the static placeholder
  // ourselves rather than via Virtuoso's `components.EmptyPlaceholder`,
  // since we don't mount Virtuoso when there's nothing to show.
  const showEmptyPlaceholder = !loading && displayItems.length === 0;

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
      {showEmptyPlaceholder && <EmptyPlaceholder />}
      {displayItems.length > 0 && (
        // `key={sessionId}` is the load-bearing piece of the "always
        // land at the latest message on tab switch" contract. Without
        // it, switching threads keeps the same Virtuoso instance, which
        // preserves its pixel scroll offset across the data swap — and
        // since the new dataset rarely has the same total height, that
        // offset usually points past the end of content (blank pane) or
        // mid-history (stranded above the latest message). Re-keying
        // forces a clean mount per session, which lets Virtuoso's own
        // `initialTopMostItemIndex={length-1}` do its job: lay out from
        // the bottom on first render, no imperative scroll dance
        // required. Only Virtuoso re-mounts — chat-view, MessageList,
        // and TurnView's memoised render outputs are unaffected, and
        // Virtuoso virtualises by viewport so the per-switch render
        // cost is bounded by what's visible, not by total list length.
        //
        // The render gate (`displayItems.length > 0`) matters: mounting
        // Virtuoso with empty data would resolve `initialTopMostItem-
        // Index={Math.max(0, -1)} = 0` and stick on item 0 even after
        // turns arrive (initialTopMostItemIndex only applies on mount).
        // Cold-cache loads show the loader above instead, then mount
        // Virtuoso once `displayItems` is populated.
        <Virtuoso
          key={sessionId}
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
          atBottomThreshold={120}
          atBottomStateChange={setAtBottom}
          // `align: "end"` is what actually pins the latest item to the
          // BOTTOM of the viewport. The bare-number form
          // (`initialTopMostItemIndex={length-1}`) defaults to
          // `align: "start"` which puts the last item at the *top* of
          // the viewport — fine for short turns, but on long replies it
          // lands the user at the beginning of the latest message
          // instead of the end of it (which felt like "I'm not at the
          // latest"). Using "LAST" + end matches what the imperative
          // `scrollToIndex` calls elsewhere in this file do.
          initialTopMostItemIndex={{ index: "LAST", align: "end" }}
          increaseViewportBy={{ top: 600, bottom: 600 }}
          // Post-mount height-settling: when items measure via
          // ResizeObserver and the total list height changes, re-pin
          // to LAST so the user lands at the true end of the latest
          // turn (not the estimated-height-based offset that
          // `initialTopMostItemIndex` resolved against at mount).
          //
          // Gated by `settlingRef` so this path is a no-op outside the
          // initial landing window — streaming growth keeps using the
          // existing `followOutput` contract. Each fire also bumps the
          // settle debounce via `bumpSettle`, so as long as items are
          // still measuring (Shiki async-highlighting code blocks one
          // by one, images loading, summary blocks rendering markdown)
          // the window stays open and we keep re-pinning. This is the
          // load-bearing fix for "scroll doesn't land at latest in
          // threads with summary/compact blocks" — those threads are
          // long by construction (auto-compaction triggered them),
          // carry many Shiki-highlighted code blocks, and the
          // cumulative async re-measurement easily outruns any
          // fixed-duration window.
          //
          // The hard ceiling (`SETTLE_HARD_CEILING_MS` from mount) is
          // a safety brake: if a thread is *continuously* re-laying-
          // out (live streaming, expanded summary edits) we stop
          // forcing the user to the bottom so the existing
          // `followOutput` semantics ("if user scrolled up to read
          // history, don't yank them") take over. `behavior: "auto"`
          // matches `scrollToLatest`'s instant pop; no animation
          // jitter. We deliberately don't toggle `suppressJump` here
          // — this path is invisible to the user (they're already
          // expecting to be at the bottom on thread click) and mid-
          // measurement state toggles can re-trigger renders.
          totalListHeightChanged={() => {
            if (!settlingRef.current) return;
            if (
              Date.now() - initialMountTsRef.current >
              SETTLE_HARD_CEILING_MS
            ) {
              settlingRef.current = false;
              if (settleTimerRef.current) {
                clearTimeout(settleTimerRef.current);
                settleTimerRef.current = null;
              }
              return;
            }
            virtuosoRef.current?.scrollToIndex({
              index: "LAST",
              align: "end",
              behavior: "auto",
            });
            bumpSettle();
          }}
        />
      )}

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
