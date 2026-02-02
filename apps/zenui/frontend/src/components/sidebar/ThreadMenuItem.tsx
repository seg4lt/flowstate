import { Trash2 } from "lucide-react";
import { SidebarMenuButton, SidebarMenuItem, SidebarMenuAction } from "../ui/sidebar";
import { actions, useAppStore, type SendClientMessage } from "../../state/appStore";
import { PROVIDER_COLORS, type SessionDetail } from "../../types";

interface Props {
  session: SessionDetail;
  sendClientMessage: SendClientMessage;
}

export function ThreadMenuItem({ session, sendClientMessage }: Props) {
  const activeSessionId = useAppStore((s) => s.activeSessionId);
  const isActive = session.summary.sessionId === activeSessionId;
  const isRunning = session.summary.status === "running";

  return (
    <SidebarMenuItem>
      <SidebarMenuButton
        isActive={isActive}
        onClick={() => {
          actions.selectSession(session.summary.sessionId);
          // Bootstrap ships sessions without their turns (to keep the sidebar
          // fast on startup). Hydrate on first open; subsequent opens already
          // have the turn history in memory.
          if (session.turns.length === 0 && session.summary.turnCount > 0) {
            sendClientMessage({
              type: "load_session",
              session_id: session.summary.sessionId,
            });
          }
        }}
        className="gap-2"
      >
        <div
          className={`w-2 h-2 rounded-full shrink-0 ${PROVIDER_COLORS[session.summary.provider]}`}
        />
        <div className="flex-1 min-w-0">
          <div className="text-sm truncate">{session.summary.title}</div>
          <div className="text-xs text-muted-foreground truncate">
            {isRunning ? (
              <span className="flex items-center gap-1">
                <span className="w-1.5 h-1.5 rounded-full bg-yellow-500 animate-pulse" />
                Running...
              </span>
            ) : (
              session.summary.lastTurnPreview || "No messages yet"
            )}
          </div>
        </div>
      </SidebarMenuButton>
      <SidebarMenuAction
        onClick={(e) => {
          e.stopPropagation();
          sendClientMessage({
            type: "delete_session",
            session_id: session.summary.sessionId,
          });
        }}
        showOnHover
      >
        <Trash2 className="h-3 w-3" />
      </SidebarMenuAction>
    </SidebarMenuItem>
  );
}
