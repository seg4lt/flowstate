import * as React from "react";

// Vertical drag handle between the chat column and a right-docked
// pane (diff / agent-context). Mirrors the sidebar DragHandle pattern
// in router.tsx but measures against the split container's right edge
// so the panel grows from the right as the mouse moves left. The
// handle lives inline between the two flex children (not absolutely
// positioned) to avoid z-index fights with the sidebar handle and
// other overlays. Generic over storageKey/minWidth so both the diff
// pane and the agent-context pane can reuse the same primitive with
// their own persisted width.

// Minimum width the chat column is allowed to shrink to before the
// drag handle stops collapsing it further. Kept in chat-view alongside
// the diff constants historically; inlined here so the primitive is
// self-contained.
const CHAT_MIN_WIDTH = 420;

export interface PanelDragHandleProps {
  containerRef: React.RefObject<HTMLDivElement | null>;
  width: number;
  onResize: (w: number) => void;
  storageKey: string;
  minWidth: number;
  ariaLabel: string;
}

export function PanelDragHandle({
  containerRef,
  width,
  onResize,
  storageKey,
  minWidth,
  ariaLabel,
}: PanelDragHandleProps) {
  const draggingRef = React.useRef(false);
  const latestWidthRef = React.useRef(width);

  React.useEffect(() => {
    latestWidthRef.current = width;
  }, [width]);

  React.useEffect(() => {
    function onMove(e: MouseEvent) {
      if (!draggingRef.current || !containerRef.current) return;
      const rect = containerRef.current.getBoundingClientRect();
      const maxWidth = Math.max(
        minWidth,
        Math.floor(rect.width - CHAT_MIN_WIDTH),
      );
      const next = Math.max(
        minWidth,
        Math.min(maxWidth, Math.round(rect.right - e.clientX)),
      );
      latestWidthRef.current = next;
      onResize(next);
    }
    function onUp() {
      if (!draggingRef.current) return;
      draggingRef.current = false;
      document.body.style.cursor = "";
      document.body.style.userSelect = "";
      try {
        window.localStorage.setItem(
          storageKey,
          String(latestWidthRef.current),
        );
      } catch {
        /* storage may be unavailable; width is still live in state */
      }
    }
    window.addEventListener("mousemove", onMove);
    window.addEventListener("mouseup", onUp);
    return () => {
      window.removeEventListener("mousemove", onMove);
      window.removeEventListener("mouseup", onUp);
    };
  }, [containerRef, onResize, storageKey, minWidth]);

  return (
    <div
      role="separator"
      aria-label={ariaLabel}
      aria-orientation="vertical"
      className="w-1 shrink-0 cursor-col-resize bg-border/50 hover:bg-border"
      onMouseDown={(e) => {
        e.preventDefault();
        draggingRef.current = true;
        document.body.style.cursor = "col-resize";
        document.body.style.userSelect = "none";
      }}
    />
  );
}
