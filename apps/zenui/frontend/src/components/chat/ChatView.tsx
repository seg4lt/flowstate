import { MessageSquare } from "lucide-react";
import { ChatHeader } from "./ChatHeader";
import { MessagesTimeline } from "./MessagesTimeline";
import { ComposerFooter } from "./ComposerFooter";
import { selectActiveSession, useAppStore, type SendClientMessage } from "../../state/appStore";

interface Props {
  sendClientMessage: SendClientMessage;
}

export function ChatView({ sendClientMessage }: Props) {
  const activeSession = useAppStore(selectActiveSession);

  if (!activeSession) {
    return (
      <div className="flex-1 flex flex-col items-center justify-center text-muted-foreground bg-background">
        <div className="w-12 h-12 rounded-xl bg-muted flex items-center justify-center mb-4">
          <MessageSquare className="w-6 h-6" />
        </div>
        <h2 className="text-lg font-medium text-foreground mb-1">No active thread</h2>
        <p className="text-sm max-w-xs text-center">
          Create a new project or thread from the sidebar to get started.
        </p>
      </div>
    );
  }

  return (
    <div className="flex-1 flex flex-col bg-background min-w-0">
      <ChatHeader session={activeSession} sendClientMessage={sendClientMessage} />
      <MessagesTimeline session={activeSession} sendClientMessage={sendClientMessage} />
      <ComposerFooter activeSession={activeSession} sendClientMessage={sendClientMessage} />
    </div>
  );
}
