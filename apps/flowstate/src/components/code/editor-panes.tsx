import * as React from "react";
import type { SplitDirection } from "./use-editor-tabs";

// Container that renders either a single child or two children
// separated by a draggable resize handle. Direction controls layout:
//   "horizontal" → flex-row (side-by-side),  handle = col-resize
//   "vertical"   → flex-col (top/bottom),    handle = row-resize
//
// The split is ratio-based (stored in EditorLayout.splitRatio)
// rather than fixed-pixel so it stays sensible when the window
// resizes. Clamped to [0.15, 0.85] so a pane can't be dragged to
// zero width/height.
//
// Sizing notes: the parent that hosts <EditorPanes/> is a plain
// block `<div>`, so `flex-1` here would have no flex context to
// resolve against. Use `h-full w-full` instead. For the split
// layout, the first pane is percentage-sized (flex-basis = ratio)
// and the second pane takes `flex: 1 1 0` — that way the handle's
// 1px width doesn't push the total past 100% and cause overflow.

export interface EditorPanesProps {
  direction: SplitDirection | null;
  ratio: number;
  onRatioChange: (ratio: number) => void;
  first: React.ReactNode;
  second?: React.ReactNode;
  /** When set (and the layout is split), only the indicated pane is
   *  visible; the other stays mounted but `display:none`. The split
   *  handle is hidden while fullscreen is active. Keeping the hidden
   *  pane in the React tree is deliberate — its CodeMirror instance
   *  retains cursor, scroll, undo history, and Shiki decorations
   *  across the toggle, so exiting fullscreen drops the user back
   *  exactly where they were instead of remounting from scratch. */
  fullscreenedPaneIndex?: 0 | 1 | null;
}

export function EditorPanes({
  direction,
  ratio,
  onRatioChange,
  first,
  second,
  fullscreenedPaneIndex = null,
}: EditorPanesProps) {
  const containerRef = React.useRef<HTMLDivElement | null>(null);

  if (direction === null || second === undefined) {
    return (
      <div
        ref={containerRef}
        className="flex h-full w-full min-h-0 min-w-0 overflow-hidden"
      >
        <div className="flex min-h-0 min-w-0 flex-1 flex-col">{first}</div>
      </div>
    );
  }

  const isHorizontal = direction === "horizontal";
  const firstBasis = `${ratio * 100}%`;
  const fsActive = fullscreenedPaneIndex !== null;
  const hideFirst = fullscreenedPaneIndex === 1;
  const hideSecond = fullscreenedPaneIndex === 0;

  // Sizing rules:
  //   * No fullscreen → first uses ratio basis, handle is 1px,
  //     second takes the remainder (flex 1 1 0).
  //   * Fullscreen on pane N → that pane goes flex 1 1 0 (full),
  //     the other gets `display: none`, handle is hidden.
  const firstStyle: React.CSSProperties = hideFirst
    ? { display: "none" }
    : fsActive
      ? { flex: "1 1 0" }
      : isHorizontal
        ? { flex: `0 0 ${firstBasis}`, width: firstBasis }
        : { flex: `0 0 ${firstBasis}`, height: firstBasis };
  const secondStyle: React.CSSProperties | undefined = hideSecond
    ? { display: "none" }
    : undefined;

  return (
    <div
      ref={containerRef}
      className={
        "flex h-full w-full min-h-0 min-w-0 overflow-hidden " +
        (isHorizontal ? "flex-row" : "flex-col")
      }
    >
      <div
        className="flex min-h-0 min-w-0 flex-col overflow-hidden"
        style={firstStyle}
      >
        {first}
      </div>
      {!fsActive && (
        <SplitResizeHandle
          containerRef={containerRef}
          direction={direction}
          onRatioChange={onRatioChange}
        />
      )}
      {/* Second pane takes whatever remains (flex: 1 1 0). This is
          what keeps the total at 100% even with the 1px handle in
          the middle — otherwise two 50% children + 1px handle
          would overflow the container by a pixel. */}
      <div
        className="flex min-h-0 min-w-0 flex-1 flex-col overflow-hidden"
        style={secondStyle}
      >
        {second}
      </div>
    </div>
  );
}

// ─── resize handle ──────────────────────────────────────────────

interface SplitResizeHandleProps {
  containerRef: React.RefObject<HTMLDivElement | null>;
  direction: SplitDirection;
  onRatioChange: (ratio: number) => void;
}

function SplitResizeHandle({
  containerRef,
  direction,
  onRatioChange,
}: SplitResizeHandleProps) {
  const draggingRef = React.useRef(false);

  // Latch `onRatioChange` in a ref so the mousemove/up effect below
  // doesn't tear down and re-register its window listeners every
  // time the callback's identity changes. The parent `tabs.setSplitRatio`
  // gets a fresh function reference after each dispatch (because the
  // api object is useMemo'd on `layout`), so *every* drag tick would
  // otherwise churn the listeners — dropping events, making the drag
  // feel unresponsive or outright broken.
  const onRatioChangeRef = React.useRef(onRatioChange);
  React.useEffect(() => {
    onRatioChangeRef.current = onRatioChange;
  }, [onRatioChange]);

  React.useEffect(() => {
    function onMove(e: MouseEvent) {
      if (!draggingRef.current || !containerRef.current) return;
      const rect = containerRef.current.getBoundingClientRect();
      const raw =
        direction === "horizontal"
          ? (e.clientX - rect.left) / rect.width
          : (e.clientY - rect.top) / rect.height;
      if (!Number.isFinite(raw)) return;
      const clamped = Math.max(0.15, Math.min(0.85, raw));
      onRatioChangeRef.current(clamped);
    }
    function onUp() {
      if (!draggingRef.current) return;
      draggingRef.current = false;
      document.body.style.cursor = "";
      document.body.style.userSelect = "";
    }
    window.addEventListener("mousemove", onMove);
    window.addEventListener("mouseup", onUp);
    return () => {
      window.removeEventListener("mousemove", onMove);
      window.removeEventListener("mouseup", onUp);
    };
    // Only re-bind listeners when the direction changes — otherwise
    // stable for the lifetime of the handle.
  }, [containerRef, direction]);

  const isHorizontal = direction === "horizontal";

  // Visual separator is 1px, but the actual hit area is 5px wide
  // (padded either side) so it's comfortable to grab without
  // pushing adjacent panes around. The inner 1px gets the hover
  // accent so the affordance is still clearly visible.
  return (
    <div
      role="separator"
      aria-label="Resize split"
      aria-orientation={isHorizontal ? "vertical" : "horizontal"}
      className={
        "group relative z-10 shrink-0 " +
        (isHorizontal
          ? "w-px cursor-col-resize"
          : "h-px cursor-row-resize")
      }
      onMouseDown={(e) => {
        e.preventDefault();
        draggingRef.current = true;
        document.body.style.cursor = isHorizontal
          ? "col-resize"
          : "row-resize";
        document.body.style.userSelect = "none";
      }}
    >
      {/* Visual line */}
      <div
        aria-hidden="true"
        className={
          "pointer-events-none absolute bg-border group-hover:bg-primary/40 " +
          (isHorizontal ? "inset-y-0 left-0 w-px" : "inset-x-0 top-0 h-px")
        }
      />
      {/* Expanded hit area */}
      <div
        aria-hidden="true"
        className={
          "absolute " +
          (isHorizontal
            ? "-left-1 -right-1 inset-y-0"
            : "-top-1 -bottom-1 inset-x-0")
        }
      />
    </div>
  );
}
