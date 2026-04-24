import * as React from "react";
import { Plus } from "lucide-react";
import {
  addComment,
  type DiffCommentAnchor,
} from "@/lib/diff-comments-store";
import { cn } from "@/lib/utils";

/** Shared review-commenting affordance that wraps a `MultiFileDiff`
 *  (or any similar per-file content surface) and surfaces a hover
 *  gutter **+** button anchored to the line under the cursor.
 *
 *  Hover resolution:
 *  - A delegated `mousemove` on the overlay container reads
 *    `event.composedPath()` (to pierce @pierre/diffs's open shadow
 *    root) and walks up looking for an element carrying a recognised
 *    line attribute (`data-line`, `data-line-number`, …). The line's
 *    file path is read from the nearest ancestor with
 *    `data-diff-path` / `data-search-path`, which the caller sets on
 *    the per-file `<section>`.
 *
 *  Line latch (the "+" is reachable" fix):
 *  - Once a line is hovered, its `DOMRect` is stashed in a ref. On
 *    every subsequent `mousemove` we first check whether the cursor
 *    is still inside that rect — unioned with the "+" button's rect
 *    plus a small hysteresis margin (`HOVER_HIT_ZONE_PAD`). If yes,
 *    we do nothing. This is what lets the user actually slide the
 *    cursor from the line text onto the button without the button
 *    unmounting mid-traverse.
 *  - When the cursor leaves the hit zone without landing on another
 *    line, we start a `HOVER_FADE_MS` fade timer. Re-entering a line
 *    cancels it; leaving the container entirely also starts it.
 *
 *  Clicking the "+" opens a small absolutely-positioned popup
 *  (bespoke, not Radix Popover — simpler for our one-shot anchor).
 *  The textarea autofocuses; Enter commits, Escape cancels.
 *  Committing fires `addComment(sessionId, …)` and closes. **Never
 *  sends** — comments sit as chips above the chat composer until the
 *  user presses Send.
 *
 *  When `sessionId` is null (new unsaved thread, or a `/browse` code
 *  view with no chat session) the overlay renders its children
 *  untouched — there's no session to attach a comment to, so
 *  surfacing the "+" would just produce orphan chips on first send.
 */
export function DiffCommentOverlay({
  sessionId,
  surface,
  pathAttr,
  className,
  children,
}: {
  sessionId: string | null;
  surface: "diff" | "search" | "code";
  /** DOM data-attribute name on the per-file section that carries the
   *  file's path. `data-diff-path` in the diff panel,
   *  `data-search-path` in the multibuffer, `data-code-path` in the
   *  open-file code viewer tab. */
  pathAttr: "data-diff-path" | "data-search-path" | "data-code-path";
  /** Extra classes for the wrapper div. Callers that host a scroll
   *  container inside us (the open-file code viewer's Virtualizer)
   *  need to pass `h-full` — without it our wrapper collapses to
   *  content height and the child's `h-full overflow-auto` has
   *  nothing bounded to scroll against. */
  className?: string;
  children: React.ReactNode;
}) {
  const containerRef = React.useRef<HTMLDivElement>(null);
  const [hoverTarget, setHoverTarget] = React.useState<HoverTarget | null>(null);
  const [activePopup, setActivePopup] = React.useState<PopupState | null>(null);
  // Client-rect of the currently latched diff line, in viewport
  // coordinates. Read by the mousemove handler to decide whether the
  // cursor is still inside the line's hit zone without re-doing the
  // full resolve-from-DOM dance. Lives in a ref (not state) so
  // updating it doesn't trigger a React render — it changes on every
  // latch switch, which would otherwise cascade unnecessarily.
  const latchedLineRectRef = React.useRef<DOMRect | null>(null);
  const hoverButtonRef = React.useRef<HTMLButtonElement | null>(null);
  const fadeTimerRef = React.useRef<number | null>(null);

  // Bail out entirely for orphan sessions — still render children so
  // the diff/multibuffer keeps working, just without comment affordances.
  const enabled = sessionId != null && sessionId.length > 0;

  const clearFadeTimer = React.useCallback(() => {
    if (fadeTimerRef.current != null) {
      window.clearTimeout(fadeTimerRef.current);
      fadeTimerRef.current = null;
    }
  }, []);

  React.useEffect(() => {
    if (!enabled) return;
    const container = containerRef.current;
    if (!container) return;

    function resolveLineAt(
      e: MouseEvent,
    ): HoverCandidate | typeof SENTINEL_CHROME | null {
      const path = e.composedPath();
      const realTarget = (path[0] as Element | undefined) ?? null;
      if (!realTarget || !(realTarget instanceof Element)) return null;
      // If the event path passes through our own UI chrome ("+"
      // button or popup), treat the move as "still on the latched
      // line" — don't try to resolve a fresh line, don't clear.
      if (
        path.some(
          (n) => n instanceof Element && n.hasAttribute?.("data-overlay-chrome"),
        )
      ) {
        return SENTINEL_CHROME;
      }
      const lineEl = findLineEl(realTarget, container!);
      if (!lineEl) return null;
      const line = extractLineNumber(lineEl);
      const filePath = readPathAttr(lineEl, pathAttr);
      if (!filePath || line == null) return null;
      const rect = lineEl.getBoundingClientRect();
      const containerRect = container!.getBoundingClientRect();
      return {
        top: rect.top - containerRect.top + container!.scrollTop,
        height: rect.height,
        path: filePath,
        line,
        lineRect: rect,
      };
    }

    function onMouseMove(e: MouseEvent) {
      const current = latchedLineRectRef.current;
      // Fast path: cursor still inside the current hit zone → do
      // nothing. This is the core of the "+" is reachable" fix.
      if (
        current &&
        pointInHitZone(e.clientX, e.clientY, current, hoverButtonRef.current)
      ) {
        clearFadeTimer();
        return;
      }
      const next = resolveLineAt(e);
      if (next === SENTINEL_CHROME) {
        // Moving over our own button — latch is already correct;
        // do NOT clear, do NOT start fade. The fast-path above
        // would've caught this if the button's rect was up-to-date,
        // but a just-rendered button can slip through one frame.
        clearFadeTimer();
        return;
      }
      if (next) {
        // Fresh line — switch latch immediately.
        clearFadeTimer();
        latchedLineRectRef.current = next.lineRect;
        setHoverTarget({
          top: next.top,
          height: next.height,
          path: next.path,
          line: next.line,
        });
        return;
      }
      // Cursor is inside the container but not on any line (gutter
      // between lines, file header, etc.). Don't clear immediately;
      // start the fade so the user has time to arrive at the "+".
      startFade();
    }

    function onMouseLeave() {
      // Mouse left the whole container — start the fade. If they
      // return to a line within the window, the fade is cancelled.
      startFade();
    }

    function startFade() {
      if (fadeTimerRef.current != null) return; // already counting down
      fadeTimerRef.current = window.setTimeout(() => {
        fadeTimerRef.current = null;
        latchedLineRectRef.current = null;
        setHoverTarget(null);
      }, HOVER_FADE_MS);
    }

    container.addEventListener("mousemove", onMouseMove);
    container.addEventListener("mouseleave", onMouseLeave);
    return () => {
      container.removeEventListener("mousemove", onMouseMove);
      container.removeEventListener("mouseleave", onMouseLeave);
      clearFadeTimer();
    };
  }, [enabled, pathAttr, clearFadeTimer]);

  const handleSubmit = React.useCallback(
    (text: string) => {
      if (!sessionId || !activePopup) return;
      const trimmed = text.trim();
      if (trimmed.length === 0) {
        setActivePopup(null);
        return;
      }
      const anchor: DiffCommentAnchor = {
        path: activePopup.path,
        surface,
        line: activePopup.line,
      };
      addComment(sessionId, { anchor, text: trimmed });
      setActivePopup(null);
      setHoverTarget(null);
      latchedLineRectRef.current = null;
    },
    [sessionId, activePopup, surface],
  );

  // Dev-only diagnostic: the #1 "why doesn't the '+' appear" cause is
  // "this surface was entered without a session", which happens on the
  // `/browse?path=…` route (no `/$sessionId` param) — we pass
  // sessionId=null, which disables the overlay. Log once on mount so
  // devtools makes the reason obvious instead of silent no-op.
  React.useEffect(() => {
    if (!import.meta.env.DEV) return;
    if (!enabled) {
      // eslint-disable-next-line no-console
      console.info(
        `[diff-comment-overlay] disabled on "${surface}" surface — no active session (likely /browse route). Comments only work when entered from a chat (e.g. /code/$sessionId).`,
      );
    }
  }, [enabled, surface]);

  // Render a single wrapper in both enabled and disabled states, with
  // a `data-comment-overlay-status` attribute — lets the user inspect
  // the DOM and see at a glance whether the overlay is live ("enabled")
  // or passing through ("disabled"). Keeping the wrapper in the
  // disabled branch is a deliberate cost (one extra div) traded for
  // drastically faster debugging.
  if (!enabled) {
    return (
      <div
        className={className}
        data-comment-overlay-status="disabled"
        data-comment-overlay-surface={surface}
      >
        {children}
      </div>
    );
  }

  return (
    <div
      ref={containerRef}
      className={cn("relative", className)}
      data-comment-overlay-status="enabled"
      data-comment-overlay-surface={surface}
    >
      {children}

      {hoverTarget && !activePopup && (
        <button
          // hoverButtonRef: the mousemove handler reads this button's
          // client rect to union it into the hit zone, so sliding off
          // the line toward the "+" doesn't blow away the latch.
          ref={hoverButtonRef}
          type="button"
          aria-label="Add comment on this line"
          // data-overlay-chrome flags this as "UI we added, not a
          // diff line" so the mousemove handler leaves hoverTarget
          // latched while the pointer sits on the button — belt-
          // and-suspenders with the hit-zone check in case the
          // button's rect hasn't been captured yet on first render.
          data-overlay-chrome=""
          onMouseDown={(e) => {
            // Prevent default mouseDown behavior so the click lands
            // on the button cleanly (no stray text-selection, no
            // focus race with the diff pane).
            e.preventDefault();
            e.stopPropagation();
          }}
          onClick={() => {
            setActivePopup({
              path: hoverTarget.path,
              line: hoverTarget.line,
              top: hoverTarget.top,
              left: 4,
            });
          }}
          style={{
            top: hoverTarget.top + hoverTarget.height / 2 - 10,
          }}
          className={cn(
            "absolute left-1 z-10 inline-flex h-5 w-5 items-center justify-center rounded-full border border-border bg-primary text-primary-foreground shadow-sm hover:bg-primary/90",
          )}
        >
          <Plus className="h-3 w-3" />
        </button>
      )}

      {activePopup && (
        <CommentPopup
          top={activePopup.top}
          left={activePopup.left}
          anchorLabel={formatPopupAnchor(activePopup)}
          onSubmit={handleSubmit}
          onCancel={() => setActivePopup(null)}
        />
      )}
    </div>
  );
}

// --- Popup -----------------------------------------------------------

function CommentPopup({
  top,
  left,
  anchorLabel,
  onSubmit,
  onCancel,
}: {
  top: number;
  left: number;
  anchorLabel: string;
  onSubmit: (text: string) => void;
  onCancel: () => void;
}) {
  const [text, setText] = React.useState("");
  const taRef = React.useRef<HTMLTextAreaElement>(null);
  const rootRef = React.useRef<HTMLDivElement>(null);

  React.useEffect(() => {
    taRef.current?.focus();
  }, []);

  // Outside-click: cancel. Uses mousedown so a click that starts
  // outside the popup and drags in doesn't count as "inside".
  React.useEffect(() => {
    function onMouseDown(e: MouseEvent) {
      const root = rootRef.current;
      if (!root) return;
      if (!root.contains(e.target as Node)) {
        onCancel();
      }
    }
    // Delay attaching by one tick so the click that opened the popup
    // doesn't immediately close it.
    const raf = requestAnimationFrame(() => {
      document.addEventListener("mousedown", onMouseDown, true);
    });
    return () => {
      cancelAnimationFrame(raf);
      document.removeEventListener("mousedown", onMouseDown, true);
    };
  }, [onCancel]);

  function onKeyDown(e: React.KeyboardEvent<HTMLTextAreaElement>) {
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      onSubmit(text);
    } else if (e.key === "Escape") {
      e.preventDefault();
      e.stopPropagation(); // don't bubble to ChatView's interrupt
      onCancel();
    }
  }

  return (
    <div
      ref={rootRef}
      style={{ top: top + 4, left }}
      data-overlay-chrome=""
      className="absolute z-20 w-72 rounded-lg border border-border bg-popover p-2 text-sm text-popover-foreground shadow-lg ring-1 ring-foreground/10"
      onMouseDown={(e) => e.stopPropagation()}
    >
      <div
        className="mb-1 truncate font-mono text-[10px] text-muted-foreground"
        title={anchorLabel}
      >
        {anchorLabel}
      </div>
      <textarea
        ref={taRef}
        value={text}
        onChange={(e) => setText(e.target.value)}
        onKeyDown={onKeyDown}
        rows={3}
        placeholder="Leave a comment… (Enter to add, Esc to cancel)"
        className="w-full resize-none rounded border border-input bg-background px-2 py-1.5 text-xs focus-visible:outline-none focus-visible:ring-1 focus-visible:ring-ring"
      />
      <div className="mt-1.5 flex justify-end gap-1.5">
        <button
          type="button"
          onClick={onCancel}
          className="rounded border border-border bg-background px-2 py-0.5 text-[11px] hover:bg-accent"
        >
          Cancel
        </button>
        <button
          type="button"
          disabled={text.trim().length === 0}
          onClick={() => onSubmit(text)}
          className="rounded bg-primary px-2 py-0.5 text-[11px] text-primary-foreground hover:bg-primary/90 disabled:opacity-50"
        >
          Add
        </button>
      </div>
    </div>
  );
}

// --- Hover latch tunables --------------------------------------------

/** Pixels of forgiveness around the line-bbox + button-bbox union
 *  when deciding whether the cursor is "still on" the latched line.
 *  Needs to at least bridge the gutter between the line's leftmost
 *  rendered pixel and the "+" button (the button is at `left: 4px`
 *  and pierre's gutter is ~30px), plus absorb tiny vertical twitches.
 *  Higher = more forgiving, but risks latching onto a line the user
 *  has clearly moved past. 8px threads both needles. */
const HOVER_HIT_ZONE_PAD = 8;

/** How long the "+" stays visible after the cursor leaves a line's
 *  hit zone without landing on another line. Gives the user time to
 *  slide the mouse over to the button after glancing at the line.
 *  GitHub uses ~500ms; 400ms feels snappier here without being too
 *  twitchy. */
const HOVER_FADE_MS = 400;

/** Sentinel returned by the mousemove probe when the cursor is on
 *  our own UI chrome (button / popup). Tells the caller "don't
 *  disturb the latch" without threading a special boolean. */
const SENTINEL_CHROME = Symbol("overlay-chrome");

interface HoverCandidate {
  top: number;
  height: number;
  path: string;
  line: number;
  lineRect: DOMRect;
}

/** True when (x, y) is inside the union of the line's rect and the
 *  hover button's rect (if rendered), inflated by
 *  `HOVER_HIT_ZONE_PAD`. Coordinates are viewport-relative (client
 *  coords from the MouseEvent). */
function pointInHitZone(
  x: number,
  y: number,
  lineRect: DOMRect,
  buttonEl: HTMLElement | null,
): boolean {
  if (rectContainsInflated(lineRect, x, y, HOVER_HIT_ZONE_PAD)) return true;
  if (buttonEl) {
    const b = buttonEl.getBoundingClientRect();
    if (rectContainsInflated(b, x, y, HOVER_HIT_ZONE_PAD)) return true;
    // Also bridge the horizontal strip between the button and the
    // line's left edge — moving diagonally from the line text toward
    // the button crosses gutter area that's outside either rect but
    // conceptually still "reaching for the button".
    const stripLeft = Math.min(b.left, lineRect.left) - HOVER_HIT_ZONE_PAD;
    const stripRight = Math.max(b.right, lineRect.right) + HOVER_HIT_ZONE_PAD;
    const stripTop = Math.min(b.top, lineRect.top) - HOVER_HIT_ZONE_PAD;
    const stripBottom = Math.max(b.bottom, lineRect.bottom) + HOVER_HIT_ZONE_PAD;
    if (x >= stripLeft && x <= stripRight && y >= stripTop && y <= stripBottom) {
      return true;
    }
  }
  return false;
}

function rectContainsInflated(
  r: DOMRect,
  x: number,
  y: number,
  pad: number,
): boolean {
  return (
    x >= r.left - pad &&
    x <= r.right + pad &&
    y >= r.top - pad &&
    y <= r.bottom + pad
  );
}

// --- Popup / hover types ---------------------------------------------

interface HoverTarget {
  top: number;
  height: number;
  path: string;
  line: number;
}

interface PopupState {
  path: string;
  line: number;
  top: number;
  left: number;
}

// --- DOM helpers -----------------------------------------------------

/** Candidate attribute names carrying a 1-based line number on a
 *  diff-line element. Ordered by likelihood — we ship with
 *  `@pierre/diffs` and the library's DOM may use any of these across
 *  versions, so we probe them all and take the first hit. Adding a new
 *  attribute here is a one-line change if the library's internals move.
 *
 *  Pierre-specific attrs as of v1.1.15:
 *    - `data-line`           — set on each *code content* line by the
 *      tokenizer (see `worker.js:466`, `utils/processLine.js:16`).
 *    - `data-column-number`  — set on each *gutter* (line-number cell)
 *      by `createGutterItem` in `utils/hast_utils.js`. Value is the
 *      user-visible line number. Including this attr is what makes
 *      the "+" appear when the user hovers the line-number gutter
 *      (not just the code content). Without it, gutter mousemoves
 *      walk up to a div with `data-line-type` + `data-column-number`
 *      + `data-line-index`, none of which match the old list, so the
 *      hover resolution silently dropped those pixels. */
const LINE_NUMBER_ATTRS = [
  "data-line-number",
  "data-line",
  "data-column-number",
  "data-pierre-line",
  "data-new-line",
];

/** Class-based fallback for identifying a line container when no
 *  explicit `data-line-*` attribute exists. Kept narrow so we don't
 *  false-positive on outer wrappers. */
const LINE_CLASS_HINTS = ["pr-line", "diff-line", "line-row"];

function extractLineNumber(el: Element): number | null {
  for (const attr of LINE_NUMBER_ATTRS) {
    const v = el.getAttribute(attr);
    if (v != null && v.length > 0) {
      const n = Number.parseInt(v, 10);
      if (!Number.isNaN(n) && n > 0) return n;
    }
  }
  return null;
}

function isLineEl(el: Element): boolean {
  for (const attr of LINE_NUMBER_ATTRS) {
    if (el.hasAttribute(attr)) return true;
  }
  for (const cls of LINE_CLASS_HINTS) {
    if (el.classList.contains(cls)) return true;
  }
  return false;
}

/** Walk one step up the DOM, crossing shadow-root boundaries. Returns
 *  null when the walk reaches the document root. Every ancestor walk
 *  in this file uses this helper because @pierre/diffs renders its
 *  line DOM inside an open shadow root — `parentElement` alone stops
 *  at the shadow host's inner edge. */
function walkUp(el: Element): Element | null {
  if (el.parentElement) return el.parentElement;
  const root = el.getRootNode();
  if (root instanceof ShadowRoot) return root.host as Element;
  return null;
}

function findLineEl(start: Element, container: Element): Element | null {
  let cur: Element | null = start;
  while (cur && cur !== container) {
    if (isLineEl(cur)) return cur;
    cur = walkUp(cur);
  }
  return null;
}

function readPathAttr(
  start: Element,
  attr: "data-diff-path" | "data-search-path" | "data-code-path",
): string | null {
  let cur: Element | null = start;
  while (cur) {
    const v = cur.getAttribute(attr);
    if (v) return v;
    cur = walkUp(cur);
  }
  return null;
}

function formatPopupAnchor(p: PopupState): string {
  const slash = p.path.lastIndexOf("/");
  const basename = slash >= 0 ? p.path.slice(slash + 1) : p.path;
  return `${basename}:${p.line}`;
}
