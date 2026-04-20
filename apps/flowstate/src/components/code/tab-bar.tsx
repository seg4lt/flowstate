import * as React from "react";
import { Columns2, Rows2, X } from "lucide-react";
import type { PaneIndex, Tab } from "./use-editor-tabs";

// Horizontal tab strip for one editor pane.
//
// - Scrolls horizontally (overflow-x-auto) when tabs exceed the
//   available width. Wheel/trackpad handles scroll natively; a
//   fade mask at both edges hints at offscreen tabs.
// - Active-tab changes `scrollIntoView({ inline: "nearest" })` so
//   activating an offscreen tab pulls it into view.
// - Close on middle-click or X-button. Click to activate.
// - Native HTML5 drag-and-drop lets tabs move between this pane
//   and the sibling pane (handled by the parent via `onDropTab`).
// - Split buttons are pinned to the right of the scrollable strip
//   (outside the scroll area) so they stay reachable at any tab
//   count.

export interface TabBarProps {
  paneIndex: PaneIndex;
  tabs: Tab[];
  activePath: string | null;
  focused: boolean;
  canSplit: boolean;
  onActivate: (path: string) => void;
  onClose: (path: string) => void;
  onSplitHorizontal?: () => void;
  onSplitVertical?: () => void;
  onFocus: () => void;
  onDropTab: (fromPane: PaneIndex, path: string) => void;
}

const TAB_DRAG_MIME = "application/x-flowstate-tab";

function basenameOf(path: string): string {
  const slash = path.lastIndexOf("/");
  return slash >= 0 ? path.slice(slash + 1) : path;
}

function dirnameOf(path: string): string {
  const slash = path.lastIndexOf("/");
  return slash >= 0 ? path.slice(0, slash) : "";
}

export function TabBar({
  paneIndex,
  tabs,
  activePath,
  focused,
  canSplit,
  onActivate,
  onClose,
  onSplitHorizontal,
  onSplitVertical,
  onFocus,
  onDropTab,
}: TabBarProps) {
  const activeTabRef = React.useRef<HTMLButtonElement | null>(null);
  const [dragOver, setDragOver] = React.useState(false);

  // Pull the active tab into view when it changes (e.g. via Cmd+N
  // keyboard shortcut or split-restore). Using scrollIntoView keeps
  // the impl to one line without custom math.
  React.useEffect(() => {
    if (activeTabRef.current) {
      activeTabRef.current.scrollIntoView({
        inline: "nearest",
        block: "nearest",
      });
    }
  }, [activePath]);

  function onDragOver(e: React.DragEvent<HTMLDivElement>) {
    if (e.dataTransfer.types.includes(TAB_DRAG_MIME)) {
      e.preventDefault();
      e.dataTransfer.dropEffect = "move";
      if (!dragOver) setDragOver(true);
    }
  }
  function onDragLeave() {
    if (dragOver) setDragOver(false);
  }
  function onDrop(e: React.DragEvent<HTMLDivElement>) {
    const raw = e.dataTransfer.getData(TAB_DRAG_MIME);
    setDragOver(false);
    if (!raw) return;
    try {
      const { fromPane, path } = JSON.parse(raw) as {
        fromPane: PaneIndex;
        path: string;
      };
      if (typeof path !== "string") return;
      onDropTab(fromPane, path);
    } catch {
      /* invalid payload */
    }
  }

  return (
    <div
      className={
        "flex shrink-0 items-center border-b border-border " +
        (focused ? "bg-background" : "bg-muted/20") +
        (dragOver ? " outline outline-1 outline-primary/40" : "")
      }
      onMouseDown={onFocus}
      onDragOver={onDragOver}
      onDragLeave={onDragLeave}
      onDrop={onDrop}
    >
      <div
        className="flex min-w-0 flex-1 items-center overflow-x-auto"
        style={{
          scrollbarWidth: "none",
          // Hide the default horizontal scrollbar — the fade masks
          // carry the "more tabs" hint instead.
          msOverflowStyle: "none",
        }}
      >
        {tabs.length === 0 ? (
          <span className="px-3 py-1.5 text-[10px] text-muted-foreground/70">
            No tabs open
          </span>
        ) : (
          tabs.map((tab) => {
            const isActive = tab.path === activePath;
            const basename = basenameOf(tab.path);
            const dirname = dirnameOf(tab.path);
            return (
              <button
                key={tab.path}
                ref={isActive ? activeTabRef : undefined}
                type="button"
                role="tab"
                aria-selected={isActive}
                draggable
                onDragStart={(e) => {
                  e.dataTransfer.setData(
                    TAB_DRAG_MIME,
                    JSON.stringify({ fromPane: paneIndex, path: tab.path }),
                  );
                  e.dataTransfer.effectAllowed = "move";
                }}
                onMouseDown={(e) => {
                  if (e.button === 1) {
                    // Middle-click closes without activating.
                    e.preventDefault();
                    onClose(tab.path);
                  }
                }}
                onClick={() => onActivate(tab.path)}
                title={dirname ? `${dirname}/${basename}` : basename}
                className={
                  "group relative flex shrink-0 items-center gap-1.5 border-r border-border px-2 py-1 text-[11px] " +
                  "min-w-[100px] max-w-[200px] " +
                  (isActive
                    ? "bg-background text-foreground"
                    : "text-muted-foreground hover:bg-muted/40 hover:text-foreground")
                }
              >
                <span className="truncate font-mono">{basename}</span>
                <span
                  role="button"
                  aria-label={`Close ${basename}`}
                  tabIndex={-1}
                  onClick={(e) => {
                    e.stopPropagation();
                    onClose(tab.path);
                  }}
                  onMouseDown={(e) => e.stopPropagation()}
                  className={
                    "ml-auto flex h-3.5 w-3.5 shrink-0 items-center justify-center rounded-sm " +
                    "text-muted-foreground/70 opacity-0 transition-opacity hover:bg-muted hover:text-foreground group-hover:opacity-100 " +
                    (isActive ? "opacity-60" : "")
                  }
                >
                  <X className="h-3 w-3" />
                </span>
                {isActive && (
                  <span
                    aria-hidden="true"
                    className={
                      "pointer-events-none absolute inset-x-0 bottom-0 h-[2px] " +
                      (focused ? "bg-foreground/70" : "bg-foreground/25")
                    }
                  />
                )}
              </button>
            );
          })
        )}
      </div>
      {canSplit && (
        <div className="flex shrink-0 items-center border-l border-border">
          {onSplitHorizontal && (
            <button
              type="button"
              onClick={onSplitHorizontal}
              title="Split right (Cmd/Ctrl+\\)"
              aria-label="Split right"
              className="flex h-7 w-7 items-center justify-center text-muted-foreground hover:bg-muted/40 hover:text-foreground"
            >
              <Columns2 className="h-3 w-3" />
            </button>
          )}
          {onSplitVertical && (
            <button
              type="button"
              onClick={onSplitVertical}
              title="Split down (Cmd/Ctrl+Shift+\\)"
              aria-label="Split down"
              className="flex h-7 w-7 items-center justify-center text-muted-foreground hover:bg-muted/40 hover:text-foreground"
            >
              <Rows2 className="h-3 w-3" />
            </button>
          )}
        </div>
      )}
    </div>
  );
}
