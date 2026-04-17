import * as React from "react";
import type { TurnStatus } from "@/lib/types";
import { MarkdownContent } from "./markdown-content";

interface AgentMessageProps {
  output: string;
  streaming: boolean;
  status: TurnStatus;
}

function BlinkingCursor() {
  return (
    <span className="ml-0.5 inline-block h-4 w-[2px] animate-pulse bg-foreground align-text-bottom" />
  );
}

function AgentMessageInner({ output, streaming, status }: AgentMessageProps) {
  // Defer the markdown parse during rapid streaming deltas — React
  // concurrent mode will render stale markdown if the main thread is
  // busy, then catch up when it has time. No manual throttling.
  const deferredOutput = React.useDeferredValue(output);

  return (
    <div className="text-sm leading-relaxed">
      <MarkdownContent content={deferredOutput} />
      {streaming && <BlinkingCursor />}
      {status === "failed" && (
        <p className="mt-2 text-xs text-destructive">Turn failed</p>
      )}
      {status === "interrupted" && (
        <p className="mt-2 text-xs text-muted-foreground">Interrupted</p>
      )}
    </div>
  );
}

export const AgentMessage = React.memo(
  AgentMessageInner,
  (prev, next) =>
    prev.output === next.output &&
    prev.streaming === next.streaming &&
    prev.status === next.status,
);
