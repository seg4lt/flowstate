import type { TurnRecord } from "@/lib/types";
import { ToolCallCard } from "./tool-call-card";

interface MessageBubbleProps {
  turn: TurnRecord;
}

export function MessageBubble({ turn }: MessageBubbleProps) {
  return (
    <div className="space-y-3">
      {/* User message */}
      <div className="flex justify-end">
        <div className="max-w-[80%] rounded-lg bg-primary px-3 py-2 text-sm text-primary-foreground">
          <p className="whitespace-pre-wrap">{turn.input}</p>
        </div>
      </div>

      {/* Assistant response */}
      {turn.output && (
        <div className="flex justify-start">
          <div className="max-w-[80%] rounded-lg bg-muted px-3 py-2 text-sm">
            <p className="whitespace-pre-wrap">{turn.output}</p>
          </div>
        </div>
      )}

      {/* Tool calls */}
      {turn.toolCalls && turn.toolCalls.length > 0 && (
        <div className="space-y-2 pl-2">
          {turn.toolCalls.map((tc) => (
            <ToolCallCard key={tc.callId} toolCall={tc} />
          ))}
        </div>
      )}

      {/* Status indicator for failed/interrupted */}
      {turn.status === "failed" && (
        <p className="text-xs text-destructive">Turn failed</p>
      )}
      {turn.status === "interrupted" && (
        <p className="text-xs text-muted-foreground">Interrupted</p>
      )}
    </div>
  );
}
