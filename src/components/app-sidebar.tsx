import { useNavigate } from "@tanstack/react-router";
import { open } from "@tauri-apps/plugin-dialog";
import {
  ChevronRight,
  FolderIcon,
  Plus,
  Settings,
  MessageSquare,
} from "lucide-react";
import {
  Collapsible,
  CollapsibleContent,
  CollapsibleTrigger,
} from "@/components/ui/collapsible";
import {
  Sidebar,
  SidebarContent,
  SidebarFooter,
  SidebarGroup,
  SidebarGroupAction,
  SidebarGroupContent,
  SidebarGroupLabel,
  SidebarHeader,
  SidebarMenu,
  SidebarMenuButton,
  SidebarMenuItem,
  SidebarMenuSub,
  SidebarMenuSubButton,
  SidebarMenuSubItem,
  SidebarRail,
} from "@/components/ui/sidebar";
import { useApp } from "@/stores/app-store";
import { ProviderDropdown } from "@/components/sidebar/provider-dropdown";
import type { SessionSummary } from "@/lib/types";

function formatTimeAgo(iso: string): string {
  const diff = Date.now() - new Date(iso).getTime();
  const minutes = Math.floor(diff / 60000);
  if (minutes < 1) return "just now";
  if (minutes < 60) return `${minutes}m ago`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours}h ago`;
  const days = Math.floor(hours / 24);
  return `${days}d ago`;
}

export function AppSidebar() {
  const { state, send } = useApp();
  const navigate = useNavigate();

  // Group sessions by project
  const sessionsByProject = new Map<string | null, SessionSummary[]>();
  for (const session of state.sessions.values()) {
    const key = session.projectId ?? null;
    const list = sessionsByProject.get(key) ?? [];
    list.push(session);
    sessionsByProject.set(key, list);
  }
  for (const list of sessionsByProject.values()) {
    list.sort(
      (a, b) =>
        new Date(b.updatedAt).getTime() - new Date(a.updatedAt).getTime(),
    );
  }

  async function handleAddFolder() {
    const selected = await open({ directory: true, multiple: false });
    if (!selected) return;
    const path = typeof selected === "string" ? selected : selected[0];
    if (!path) return;
    const name = path.split("/").pop() ?? path;
    await send({ type: "create_project", name });
  }

  function handleThreadClick(sessionId: string) {
    navigate({ to: "/chat/$sessionId", params: { sessionId } });
  }

  const unassigned = sessionsByProject.get(null) ?? [];

  return (
    <Sidebar collapsible="icon">
      <SidebarHeader className="h-12 flex-row items-center justify-start border-b border-sidebar-border px-4 py-0">
        <span className="text-sm font-semibold tracking-tight [[data-collapsible=icon]_&]:hidden">
          flowzen
        </span>
      </SidebarHeader>

      <SidebarContent>
        <SidebarGroup>
          <SidebarGroupLabel>Projects</SidebarGroupLabel>
          <SidebarGroupAction title="Add folder" onClick={handleAddFolder}>
            <Plus />
            <span className="sr-only">Add folder</span>
          </SidebarGroupAction>
          <SidebarGroupContent>
            <SidebarMenu>
              {state.projects.map((project) => {
                const threads =
                  sessionsByProject.get(project.projectId) ?? [];
                return (
                  <Collapsible
                    key={project.projectId}
                    defaultOpen
                    className="group/collapsible"
                  >
                    <SidebarMenuItem className="group/project">
                      <CollapsibleTrigger asChild>
                        <SidebarMenuButton tooltip={project.name}>
                          <ChevronRight className="transition-transform duration-200 group-data-[state=open]/collapsible:rotate-90" />
                          <FolderIcon />
                          <span className="flex-1 truncate">
                            {project.name}
                          </span>
                        </SidebarMenuButton>
                      </CollapsibleTrigger>
                      <div className="absolute right-1 top-1">
                        <ProviderDropdown projectId={project.projectId} />
                      </div>
                      <CollapsibleContent>
                        <SidebarMenuSub>
                          {threads.map((session) => (
                            <SidebarMenuSubItem key={session.sessionId}>
                              <SidebarMenuSubButton
                                className="h-auto flex-col items-start gap-0.5 py-1.5"
                                isActive={
                                  state.activeSessionId === session.sessionId
                                }
                                onClick={() =>
                                  handleThreadClick(session.sessionId)
                                }
                              >
                                <span className="truncate text-sm">
                                  {session.title || "New thread"}
                                </span>
                                <span className="text-xs text-muted-foreground">
                                  {formatTimeAgo(session.updatedAt)}
                                </span>
                              </SidebarMenuSubButton>
                            </SidebarMenuSubItem>
                          ))}
                          {threads.length === 0 && (
                            <SidebarMenuSubItem>
                              <span className="px-2 py-1 text-xs text-muted-foreground">
                                No threads yet
                              </span>
                            </SidebarMenuSubItem>
                          )}
                        </SidebarMenuSub>
                      </CollapsibleContent>
                    </SidebarMenuItem>
                  </Collapsible>
                );
              })}

              {/* Folder-less threads */}
              <Collapsible defaultOpen className="group/collapsible">
                <SidebarMenuItem className="group/project">
                  <CollapsibleTrigger asChild>
                    <SidebarMenuButton tooltip="Threads">
                      <ChevronRight className="transition-transform duration-200 group-data-[state=open]/collapsible:rotate-90" />
                      <MessageSquare />
                      <span className="flex-1 truncate">Threads</span>
                    </SidebarMenuButton>
                  </CollapsibleTrigger>
                  <div className="absolute right-1 top-1">
                    <ProviderDropdown />
                  </div>
                  <CollapsibleContent>
                    <SidebarMenuSub>
                      {unassigned.map((session) => (
                        <SidebarMenuSubItem key={session.sessionId}>
                          <SidebarMenuSubButton
                            className="h-auto flex-col items-start gap-0.5 py-1.5"
                            isActive={
                              state.activeSessionId === session.sessionId
                            }
                            onClick={() =>
                              handleThreadClick(session.sessionId)
                            }
                          >
                            <span className="truncate text-sm">
                              {session.title || "New thread"}
                            </span>
                            <span className="text-xs text-muted-foreground">
                              {formatTimeAgo(session.updatedAt)}
                            </span>
                          </SidebarMenuSubButton>
                        </SidebarMenuSubItem>
                      ))}
                      {unassigned.length === 0 && (
                        <SidebarMenuSubItem>
                          <span className="px-2 py-1 text-xs text-muted-foreground">
                            No threads yet
                          </span>
                        </SidebarMenuSubItem>
                      )}
                    </SidebarMenuSub>
                  </CollapsibleContent>
                </SidebarMenuItem>
              </Collapsible>
            </SidebarMenu>
          </SidebarGroupContent>
        </SidebarGroup>
      </SidebarContent>

      <SidebarFooter className="border-t border-sidebar-border">
        <SidebarMenu>
          <SidebarMenuItem>
            <SidebarMenuButton tooltip="Settings">
              <Settings />
              <span>Settings</span>
            </SidebarMenuButton>
          </SidebarMenuItem>
        </SidebarMenu>
      </SidebarFooter>
      <SidebarRail />
    </Sidebar>
  );
}
