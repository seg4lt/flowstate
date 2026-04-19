import * as React from "react";
import type { ProviderKind, TurnStatus } from "@/lib/types";
import { MarkdownContent } from "./markdown-content";
import { CopyButton } from "./copy-button";
import { MessageModelInfo } from "../message-model-info";

interface AgentMessageProps {
  output: string;
  streaming: boolean;
  status: TurnStatus;
  /** Raw provider-level model id that produced this reply, when
   *  known. Usually `turn.usage.model` once the turn completes, else
   *  falls back to `session.summary.model` from the caller. */
  model?: string;
  /** Provider kind for looking up the display label in the info
   *  popover. */
  providerKind?: ProviderKind;
}

function BlinkingCursor() {
  return (
    <span className="ml-0.5 inline-block h-4 w-[2px] animate-pulse bg-foreground align-text-bottom" />
  );
}

function AgentMessageInner({
  output,
  streaming,
  status,
  model,
  providerKind,
}: AgentMessageProps) {
  // Defer the markdown parse during rapid streaming deltas — React
  // concurrent mode will render stale markdown if the main thread is
  // busy, then catch up when it has time. No manual throttling.
  const deferredOutput = React.useDeferredValue(output);

  // We gate the copy button on `!streaming` so users don't grab a
  // half-finished reply by accident. The model-info popover is fine
  // to show any time (model is known even mid-stream via the session
  // fallback).
  const showCopy = !streaming && output.length > 0;

  return (
    <div className="group text-sm leading-relaxed">
      <MarkdownContent content={deferredOutput} />
      {streaming && <BlinkingCursor />}
      {status === "failed" && (
        <p className="mt-2 text-xs text-destructive">Turn failed</p>
      )}
      {status === "interrupted" && (
        <p className="mt-2 text-xs text-muted-foreground">Interrupted</p>
      )}
      {(showCopy || (model && providerKind)) && (
        <div className="mt-1 flex items-center gap-0.5 opacity-0 transition-opacity group-hover:opacity-100 focus-within:opacity-100">
          {model && providerKind && (
            <MessageModelInfo
              modelId={model}
              providerKind={providerKind}
            />
          )}
          {showCopy && (
            <CopyButton
              text={output}
              title="Copy as markdown"
              label="Copied markdown"
            />
          )}
        </div>
      )}
    </div>
  );
}

export const AgentMessage = React.memo(
  AgentMessageInner,
  (prev, next) =>
    prev.output === next.output &&
    prev.streaming === next.streaming &&
    prev.status === next.status &&
    prev.model === next.model &&
    prev.providerKind === next.providerKind,
);
