import { useEffect, useLayoutEffect, useMemo, useRef } from "react";
import { Bot } from "lucide-react";
import { buildTimelineRows, type TimelineRow } from "./timelineRows";
import { WorkingIndicator } from "./WorkingIndicator";
import { WorkLogCard } from "./WorkLogCard";
import { PlanCard } from "./PlanCard";
import { ScrollToBottomPill } from "./ScrollToBottomPill";
import { isScrollContainerNearBottom } from "../../chat-scroll";
import type { SessionDetail } from "../../types";
import { PROVIDER_COLORS, PROVIDER_LABELS } from "../../types";
import type { SendClientMessage } from "../../state/appStore";

interface Props {
  session: SessionDetail;
  sendClientMessage: SendClientMessage;
}

export function MessagesTimeline({ session, sendClientMessage }: Props) {
  const scrollRef = useRef<HTMLDivElement | null>(null);
  const wasNearBottomRef = useRef(true);

  const rows = useMemo(() => buildTimelineRows(session), [session]);
  const accumulatedOutputLength = session.turns.reduce(
    (acc, turn) => acc + turn.output.length + (turn.reasoning?.length ?? 0),
    0,
  );

  // Track whether user is near the bottom BEFORE the next update so we can
  // auto-scroll when new content arrives.
  const handleScroll = () => {
    wasNearBottomRef.current = isScrollContainerNearBottom(scrollRef.current);
  };

  useLayoutEffect(() => {
    if (!wasNearBottomRef.current) return;
    if (rows.length === 0) return;
    scrollRef.current?.scrollTo({ top: scrollRef.current.scrollHeight });
  }, [rows.length, accumulatedOutputLength]);

  useEffect(() => {
    // Refresh the near-bottom flag after the initial mount.
    wasNearBottomRef.current = true;
  }, [session.summary.sessionId]);

  const providerColor = PROVIDER_COLORS[session.summary.provider];
  const providerLabel = PROVIDER_LABELS[session.summary.provider];

  return (
    <div className="relative flex-1 min-h-0">
      <div
        ref={scrollRef}
        onScroll={handleScroll}
        className="h-full overflow-y-auto overflow-x-hidden"
      >
        {rows.length === 0 ? (
          <div className="flex h-full items-center justify-center text-muted-foreground">
            <div className="text-center">
              <Bot className="w-8 h-8 mx-auto mb-3 opacity-50" />
              <p>Thread ready. Send your first message below.</p>
            </div>
          </div>
        ) : (
          <div className="max-w-3xl mx-auto px-6 pt-4 pb-32">
            {rows.map((row) => (
              <TimelineRowContent
                key={row.id}
                row={row}
                providerColor={providerColor}
                providerLabel={providerLabel}
                sessionId={session.summary.sessionId}
                sendClientMessage={sendClientMessage}
              />
            ))}
          </div>
        )}
      </div>
      <ScrollToBottomPill
        scrollRef={scrollRef}
        rowCount={rows.length}
        onClick={() =>
          scrollRef.current?.scrollTo({ top: scrollRef.current.scrollHeight, behavior: "smooth" })
        }
      />
    </div>
  );
}


function TimelineRowContent({
  row,
  providerColor,
  providerLabel,
  sessionId,
  sendClientMessage,
}: {
  row: TimelineRow;
  providerColor: string;
  providerLabel: string;
  sessionId: string;
  sendClientMessage: SendClientMessage;
}) {
  const turn = row.turn;

  if (row.kind === "user") {
    return (
      <div className="flex gap-3 pb-4">
        <div className="w-7 h-7 rounded-full bg-primary flex items-center justify-center shrink-0 text-primary-foreground text-xs font-medium">
          You
        </div>
        <div className="flex-1 pt-0.5 min-w-0">
          <p className="text-sm leading-relaxed whitespace-pre-wrap">{turn.input}</p>
        </div>
      </div>
    );
  }

  if (row.kind === "reasoning") {
    return (
      <div className="flex gap-3 pb-2">
        <div className="w-7 shrink-0" />
        <details className="flex-1 min-w-0 group" open={turn.status === "running"}>
          <summary className="cursor-pointer text-xs text-muted-foreground select-none flex items-center gap-1 list-none mb-1">
            <span className="inline-block transition-transform group-open:rotate-90">▶</span>
            <span>Thinking</span>
            {turn.status === "running" && (
              <span className="w-1.5 h-1.5 rounded-full bg-yellow-500 animate-pulse ml-1" />
            )}
          </summary>
          <div className="text-xs leading-relaxed whitespace-pre-wrap text-muted-foreground bg-muted/40 rounded p-2 border-l-2 border-border">
            {turn.reasoning}
          </div>
        </details>
      </div>
    );
  }

  if (row.kind === "assistant") {
    const placeholder =
      turn.output.length === 0
        ? turn.status === "completed"
          ? "(empty response)"
          : ""
        : "";
    return (
      <div className="flex gap-3 pb-4">
        <div
          className={`w-7 h-7 rounded-full flex items-center justify-center shrink-0 text-white text-xs font-medium ${providerColor}`}
        >
          {providerLabel[0]}
        </div>
        <div className="flex-1 min-w-0 pt-0.5">
          <div className="text-sm font-medium mb-1">{providerLabel}</div>
          <div className="text-sm leading-relaxed whitespace-pre-wrap">
            {turn.output || placeholder}
          </div>
        </div>
      </div>
    );
  }

  if (row.kind === "worklog") {
    return (
      <div className="flex gap-3 pb-3">
        <div className="w-7 shrink-0" />
        <div className="flex-1 min-w-0">
          <WorkLogCard
            toolCalls={turn.toolCalls ?? []}
            fileChanges={turn.fileChanges ?? []}
            subagents={turn.subagents ?? []}
            turnCompleted={turn.status !== "running"}
          />
        </div>
      </div>
    );
  }

  if (row.kind === "plan" && turn.plan) {
    return (
      <div className="flex gap-3 pb-3">
        <div className="w-7 shrink-0" />
        <div className="flex-1 min-w-0">
          <PlanCard
            sessionId={sessionId}
            plan={turn.plan}
            sendClientMessage={sendClientMessage}
          />
        </div>
      </div>
    );
  }

  if (row.kind === "working") {
    return (
      <div className="flex gap-3 pb-3">
        <div className="w-7 shrink-0" />
        <div className="flex-1 min-w-0">
          <WorkingIndicator startedAt={turn.createdAt} />
        </div>
      </div>
    );
  }

  return null;
}
