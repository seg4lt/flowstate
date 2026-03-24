import { useState } from "react";
import { FolderPlus, RefreshCw } from "lucide-react";
import {
  Sidebar,
  SidebarContent,
  SidebarFooter,
  SidebarGroup,
  SidebarGroupLabel,
  SidebarHeader,
  SidebarMenu,
} from "../ui/sidebar";
import { Input } from "../ui/input";
import { Button } from "../ui/button";
import { Tooltip, TooltipContent, TooltipTrigger } from "../ui/tooltip";
import {
  actions,
  selectProjectGroups,
  selectProviderStatuses,
  useAppStore,
  type SendClientMessage,
} from "../../state/appStore";
import { ProjectGroupNode } from "./ProjectGroupNode";
import { ProviderStatusList } from "./ProviderStatusList";

interface AppSidebarProps {
  sendClientMessage: SendClientMessage;
}

export function AppSidebar({ sendClientMessage }: AppSidebarProps) {
  const projectGroups = useAppStore(selectProjectGroups);
  const providers = useAppStore(selectProviderStatuses);
  const connectionStatus = useAppStore((s) => s.connectionStatus);
  const lastAction = useAppStore((s) => s.lastAction);
  const sessionCount = useAppStore((s) => s.snapshot.sessions.length);
  const [creatingProject, setCreatingProject] = useState(false);
  const [draftName, setDraftName] = useState("");

  const submitNewProject = () => {
    const trimmed = draftName.trim();
    if (!trimmed) {
      setCreatingProject(false);
      return;
    }
    // The SDK's `create_project` takes no name — display labels are
    // purely app-side now. Queue the draft name so the reducer can
    // pop it into projectDisplay when `project_created` arrives.
    actions.queueProjectName(trimmed);
    sendClientMessage({ type: "create_project" });
    setDraftName("");
    setCreatingProject(false);
  };

  const refreshSnapshot = () => {
    sendClientMessage({ type: "load_snapshot" });
    actions.setLastAction("Refreshing...");
  };

  return (
    <Sidebar collapsible="none" className="border-r border-border">
      <SidebarHeader className="px-3 py-2 border-b border-border">
        <div className="flex items-center justify-between">
          <span className="text-xs font-semibold uppercase tracking-wider text-muted-foreground">
            Projects
          </span>
          <div className="flex items-center gap-1">
            <Tooltip>
              <TooltipTrigger>
                <Button
                  variant="ghost"
                  size="icon"
                  className="h-6 w-6"
                  onClick={refreshSnapshot}
                >
                  <RefreshCw className="h-3.5 w-3.5" />
                </Button>
              </TooltipTrigger>
              <TooltipContent>Refresh</TooltipContent>
            </Tooltip>
            <Tooltip>
              <TooltipTrigger>
                <Button
                  variant="ghost"
                  size="icon"
                  className="h-6 w-6"
                  onClick={() => {
                    setCreatingProject(true);
                    setDraftName("");
                  }}
                >
                  <FolderPlus className="h-3.5 w-3.5" />
                </Button>
              </TooltipTrigger>
              <TooltipContent>New project</TooltipContent>
            </Tooltip>
          </div>
        </div>
        {creatingProject && (
          <Input
            autoFocus
            value={draftName}
            onChange={(e) => setDraftName(e.target.value)}
            placeholder="Project name"
            className="mt-2 h-7 text-xs"
            onKeyDown={(e) => {
              if (e.key === "Enter") {
                e.preventDefault();
                submitNewProject();
              } else if (e.key === "Escape") {
                setCreatingProject(false);
                setDraftName("");
              }
            }}
            onBlur={submitNewProject}
          />
        )}
      </SidebarHeader>

      <SidebarContent>
        {projectGroups.map((group, index) => (
          <ProjectGroupNode
            key={group.project?.projectId ?? `unassigned-${index}`}
            group={group}
            providers={providers}
            sendClientMessage={sendClientMessage}
          />
        ))}
        <SidebarGroup>
          <SidebarGroupLabel>Providers</SidebarGroupLabel>
          <SidebarMenu>
            <ProviderStatusList providers={providers} />
          </SidebarMenu>
        </SidebarGroup>
      </SidebarContent>

      <SidebarFooter className="h-7 px-3 border-t border-border bg-sidebar">
        <div className="flex items-center justify-between text-[11px] text-muted-foreground">
          <div className="flex items-center gap-2">
            <div
              className={`w-1.5 h-1.5 rounded-full ${
                connectionStatus === "connected"
                  ? "bg-green-500"
                  : connectionStatus === "connecting"
                    ? "bg-yellow-500"
                    : "bg-red-500"
              }`}
            />
            <span className="capitalize">{connectionStatus}</span>
            <span className="text-border">|</span>
            <span className="truncate max-w-[140px]">{lastAction}</span>
          </div>
          <span>{sessionCount}</span>
        </div>
      </SidebarFooter>
    </Sidebar>
  );
}
