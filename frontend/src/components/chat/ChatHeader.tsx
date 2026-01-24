import { Button } from "../ui/button";
import { Badge } from "../ui/badge";
import { PROVIDER_COLORS, PROVIDER_LABELS, type SessionDetail } from "../../types";
import type { SendClientMessage } from "../../state/appStore";

interface Props {
  session: SessionDetail;
  sendClientMessage: SendClientMessage;
}

export function ChatHeader({ session, sendClientMessage }: Props) {
  const { summary } = session;
  const isRunning = summary.status === "running";
  return (
    <div className="h-14 border-b border-border flex items-center justify-between px-6 shrink-0">
      <div className="flex items-center gap-3 min-w-0">
        <div className={`w-2.5 h-2.5 rounded-full shrink-0 ${PROVIDER_COLORS[summary.provider]}`} />
        <div className="min-w-0">
          <div className="flex items-center gap-2">
            <h1 className="font-semibold truncate">{summary.title}</h1>
            {summary.model && (
              <Badge variant="outline" className="text-[10px] h-4 px-1.5">
                {summary.model}
              </Badge>
            )}
          </div>
          <p className="text-xs text-muted-foreground truncate">
            {PROVIDER_LABELS[summary.provider]} · {summary.turnCount} turns
          </p>
        </div>
      </div>
      <div className="flex items-center gap-2">
        {isRunning && (
          <Button
            variant="destructive"
            size="sm"
            onClick={() =>
              sendClientMessage({
                type: "interrupt_turn",
                session_id: summary.sessionId,
              })
            }
          >
            Interrupt
          </Button>
        )}
      </div>
    </div>
  );
}
