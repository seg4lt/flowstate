import * as React from "react";
import type { TurnRecord } from "@/lib/types";
import { MessageBubble } from "./message-bubble";
import { StreamingText } from "./streaming-text";

interface StreamingTurn {
  turnId: string;
  accumulatedOutput: string;
}

interface MessageListProps {
  turns: TurnRecord[];
  streaming: StreamingTurn | null;
  loading: boolean;
}

export function MessageList({ turns, streaming, loading }: MessageListProps) {
  const endRef = React.useRef<HTMLDivElement>(null);
  const containerRef = React.useRef<HTMLDivElement>(null);
  const shouldAutoScrollRef = React.useRef(true);

  React.useEffect(() => {
    const container = containerRef.current;
    if (!container) return;

    function onScroll() {
      if (!container) return;
      const { scrollTop, scrollHeight, clientHeight } = container;
      shouldAutoScrollRef.current = scrollHeight - scrollTop - clientHeight < 80;
    }

    container.addEventListener("scroll", onScroll);
    return () => container.removeEventListener("scroll", onScroll);
  }, []);

  React.useEffect(() => {
    if (shouldAutoScrollRef.current) {
      endRef.current?.scrollIntoView({ behavior: "smooth" });
    }
  }, [turns, streaming?.accumulatedOutput]);

  if (loading) {
    return (
      <div className="flex flex-1 items-center justify-center text-sm text-muted-foreground">
        Loading...
      </div>
    );
  }

  if (turns.length === 0 && !streaming) {
    return (
      <div className="flex flex-1 items-center justify-center p-8 text-sm text-muted-foreground">
        Send a message to start the conversation.
      </div>
    );
  }

  return (
    <div ref={containerRef} className="min-h-0 flex-1 overflow-y-auto">
      <div className="mx-auto max-w-3xl space-y-4 p-4">
        {turns.map((turn) => (
          <MessageBubble key={turn.turnId} turn={turn} />
        ))}

        {streaming && (
          <StreamingText content={streaming.accumulatedOutput} />
        )}

        <div ref={endRef} />
      </div>
    </div>
  );
}
