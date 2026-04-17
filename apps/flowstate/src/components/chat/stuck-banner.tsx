import * as React from "react";
import { AlertTriangle } from "lucide-react";

interface StuckBannerProps {
  /** Seconds since the last event from the running turn. */
  elapsedSeconds: number;
  onInterrupt: () => void;
  onReload: () => void;
}

function StuckBannerInner({
  elapsedSeconds,
  onInterrupt,
  onReload,
}: StuckBannerProps) {
  return (
    <div className="shrink-0 border-t border-amber-500/40 bg-amber-500/5 px-3 py-2.5">
      <div className="flex items-center gap-3">
        <AlertTriangle className="h-4 w-4 shrink-0 text-amber-500" />
        <div className="min-w-0 flex-1 text-xs">
          <div className="font-medium">Session may be stuck</div>
          <div className="text-muted-foreground">
            No events for {elapsedSeconds}s while a tool call is pending. The
            bridge may have dropped the permission answer.
          </div>
        </div>
        <div className="flex shrink-0 gap-2">
          <button
            type="button"
            onClick={onReload}
            className="rounded-md border border-input bg-background px-3 py-1.5 text-xs font-medium hover:bg-accent"
          >
            Reload session
          </button>
          <button
            type="button"
            onClick={onInterrupt}
            className="rounded-md bg-destructive px-3 py-1.5 text-xs font-medium text-destructive-foreground hover:bg-destructive/90"
          >
            Interrupt turn
          </button>
        </div>
      </div>
    </div>
  );
}

export const StuckBanner = React.memo(StuckBannerInner);
