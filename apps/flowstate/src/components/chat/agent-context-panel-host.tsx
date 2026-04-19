import * as React from "react";
import { cn } from "@/lib/utils";
import { PanelDragHandle } from "@/components/ui/panel-drag-handle";
import { AgentContextPanel } from "./agent-context-panel";
import type { TurnRecord } from "@/lib/types";

const CONTEXT_WIDTH_KEY = "flowstate:context-width";
const CONTEXT_MIN_WIDTH = 320;
const CONTEXT_DEFAULT_WIDTH = 440;

function readInitialContextWidth(): number {
  try {
    const saved = window.localStorage.getItem(CONTEXT_WIDTH_KEY);
    if (saved) {
      const parsed = Number.parseInt(saved, 10);
      if (Number.isFinite(parsed) && parsed >= CONTEXT_MIN_WIDTH) {
        return parsed;
      }
    }
  } catch {
    /* storage may be unavailable */
  }
  return CONTEXT_DEFAULT_WIDTH;
}

export interface AgentContextPanelHostProps {
  containerRef: React.RefObject<HTMLDivElement | null>;
  open: boolean;
  onOpenChange: (open: boolean) => void;
  fullscreen: boolean;
  onFullscreenChange: (fullscreen: boolean) => void;
  turns: TurnRecord[];
  runningTurn: TurnRecord | null;
}

/** Right-docked Agent Context pane. Mutually exclusive with the diff
 *  panel — ChatView handles mutual exclusion by calling
 *  `onOpenChange(false)` on whichever panel is ceding the slot. */
export function AgentContextPanelHost(props: AgentContextPanelHostProps) {
  const {
    containerRef,
    open,
    onOpenChange,
    fullscreen,
    onFullscreenChange,
    turns,
    runningTurn,
  } = props;

  const [width, setWidth] = React.useState<number>(readInitialContextWidth);

  if (!open) return null;

  return (
    <>
      {!fullscreen && (
        <PanelDragHandle
          containerRef={containerRef}
          width={width}
          onResize={setWidth}
          storageKey={CONTEXT_WIDTH_KEY}
          minWidth={CONTEXT_MIN_WIDTH}
          ariaLabel="Resize agent context panel"
        />
      )}
      <aside
        className={cn(
          "border-l border-border bg-background",
          fullscreen ? "flex-1" : "shrink-0",
        )}
        style={fullscreen ? undefined : { width }}
      >
        <AgentContextPanel
          turns={turns}
          runningTurn={runningTurn}
          onClose={() => {
            onFullscreenChange(false);
            onOpenChange(false);
          }}
          isFullscreen={fullscreen}
          onToggleFullscreen={() => onFullscreenChange(!fullscreen)}
        />
      </aside>
    </>
  );
}
