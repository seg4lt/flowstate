import * as React from "react";
import type { TurnStatus } from "@/lib/types";
import { MarkdownContent } from "./markdown-content";

interface AgentMessageProps {
  output: string;
  reasoning?: string;
  streaming: boolean;
  status: TurnStatus;
}

function BlinkingCursor({ tone }: { tone: "foreground" | "muted" }) {
  return (
    <span
      className={
        "ml-0.5 inline-block h-4 w-[2px] animate-pulse align-text-bottom " +
        (tone === "muted" ? "bg-muted-foreground" : "bg-foreground")
      }
    />
  );
}

function AgentMessageInner({
  output,
  reasoning,
  streaming,
  status,
}: AgentMessageProps) {
  // Defer the markdown parse during rapid streaming deltas — React
  // concurrent mode will render stale markdown if the main thread is
  // busy, then catch up when it has time. No manual throttling.
  const deferredOutput = React.useDeferredValue(output);

  // Thinking placeholder — streaming, no reasoning, no content yet.
  if (streaming && !output && !reasoning) {
    return (
      <div className="text-sm text-muted-foreground">
        <span className="animate-pulse">Thinking…</span>
      </div>
    );
  }

  // Reasoning-only streaming — content hasn't started yet, show the
  // thinking stream in italics with a cursor. Matches previous behavior
  // where reasoning disappears once output begins streaming.
  if (streaming && !output && reasoning) {
    return (
      <div className="text-sm">
        <p className="whitespace-pre-wrap text-muted-foreground italic">
          {reasoning}
        </p>
        <BlinkingCursor tone="muted" />
      </div>
    );
  }

  return (
    <div className="text-sm leading-relaxed">
      <MarkdownContent content={deferredOutput} />
      {streaming && <BlinkingCursor tone="foreground" />}
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
    prev.reasoning === next.reasoning &&
    prev.streaming === next.streaming &&
    prev.status === next.status,
);
