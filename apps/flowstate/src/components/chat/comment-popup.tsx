import * as React from "react";

// Shared comment-popup body. Rendered by:
//  - `DiffCommentOverlay` (absolute-positioned over a @pierre/diffs
//    surface, with `top`/`left` supplied)
//  - The CM6 comment extension (mounted inside a `showTooltip` DOM
//    node, with NO `top`/`left` — CM6 positions the wrapping tooltip)
//
// When `top` / `left` are omitted the popup drops its `absolute`
// styling and the inline positioning, so the parent (CM6 tooltip)
// can lay it out. Everything else — textarea, key bindings, outside-
// click cancel, submit/cancel buttons — is identical for both
// surfaces.

export interface CommentPopupProps {
  /** Optional viewport-y for absolute mode. Omit for embedded mode. */
  top?: number;
  /** Optional viewport-x for absolute mode. Omit for embedded mode. */
  left?: number;
  anchorLabel: string;
  onSubmit: (text: string) => void;
  onCancel: () => void;
}

export function CommentPopup({
  top,
  left,
  anchorLabel,
  onSubmit,
  onCancel,
}: CommentPopupProps) {
  const [text, setText] = React.useState("");
  const taRef = React.useRef<HTMLTextAreaElement>(null);
  const rootRef = React.useRef<HTMLDivElement>(null);

  React.useEffect(() => {
    taRef.current?.focus();
  }, []);

  // Outside-click: cancel. Uses mousedown so a click that starts
  // outside the popup and drags in doesn't count as "inside". The
  // CM6-tooltip caller relies on this same handler — clicking
  // outside the tooltip dismisses it identically to the overlay
  // surface.
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

  // Absolute mode: caller (DiffCommentOverlay) positions us against
  // an enclosing `relative` wrapper. Embedded mode: caller (CM6
  // tooltip) wraps us in its own positioned container, so we drop
  // the `absolute` and the inline top/left.
  const positioned = top !== undefined && left !== undefined;
  const style: React.CSSProperties | undefined = positioned
    ? { top: top + 4, left }
    : undefined;
  const positionClass = positioned ? "absolute z-20" : "";

  return (
    <div
      ref={rootRef}
      style={style}
      data-overlay-chrome=""
      className={`${positionClass} w-72 rounded-lg border border-border bg-popover p-2 text-sm text-popover-foreground shadow-lg ring-1 ring-foreground/10`.trim()}
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
