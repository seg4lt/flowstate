import { SidebarProvider } from "./components/ui/sidebar";
import { TooltipProvider } from "./components/ui/tooltip";
import { TitleBar } from "./components/TitleBar";
import { AppSidebar } from "./components/sidebar/AppSidebar";
import { ChatView } from "./components/chat/ChatView";
import { PermissionDialog } from "./components/dialogs/PermissionDialog";
import { QuestionDialog } from "./components/dialogs/QuestionDialog";
import { useWebSocket } from "./state/useWebSocket";

export default function App() {
  const sendClientMessage = useWebSocket();

  return (
    <TooltipProvider>
      <div className="relative h-screen w-screen flex flex-col bg-background text-foreground overflow-hidden">
        <TitleBar />
        <SidebarProvider className="flex-1 min-h-0">
          <AppSidebar sendClientMessage={sendClientMessage} />
          <ChatView sendClientMessage={sendClientMessage} />
        </SidebarProvider>
        <PermissionDialog sendClientMessage={sendClientMessage} />
        <QuestionDialog sendClientMessage={sendClientMessage} />
      </div>
    </TooltipProvider>
  );
}
