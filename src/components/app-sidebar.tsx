import { useNavigate } from "@tanstack/react-router";
import {
  ChevronRight,
  FolderIcon,
  Plus,
  Settings,
  ArrowUpDown,
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

  const sessionsByProject = new Map<string | null, SessionSummary[]>();
  for (const session of state.sessions.values()) {
    const key = session.projectId ?? null;
    const list = sessionsByProject.get(key) ?? [];
    list.push(session);
    sessionsByProject.set(key, list);
  }

  // Sort sessions within each group by updatedAt descending
  for (const list of sessionsByProject.values()) {
    list.sort(
      (a, b) =>
        new Date(b.updatedAt).getTime() - new Date(a.updatedAt).getTime(),
    );
  }

  async function handleNewThread() {
    const provider =
      state.providers.find((p) => p.kind === "claude" && p.installed)?.kind ??
      state.providers.find((p) => p.installed)?.kind ??
      "claude";
    const res = await send({ type: "start_session", provider });
    if (res && res.type === "session_created") {
      navigate({
        to: "/chat/$sessionId",
        params: { sessionId: res.session.sessionId },
      });
    }
  }

  function handleThreadClick(sessionId: string) {
    navigate({ to: "/chat/$sessionId", params: { sessionId } });
  }

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
          <SidebarGroupAction title="Sort projects">
            <ArrowUpDown />
            <span className="sr-only">Sort projects</span>
          </SidebarGroupAction>
          <SidebarGroupAction
            title="New thread"
            className="right-7"
            onClick={handleNewThread}
          >
            <Plus />
            <span className="sr-only">New thread</span>
          </SidebarGroupAction>
          <SidebarGroupContent>
            <SidebarMenu>
              {state.projects.map((project) => {
                const threads = sessionsByProject.get(project.projectId) ?? [];
                return (
                  <Collapsible
                    key={project.projectId}
                    defaultOpen
                    className="group/collapsible"
                  >
                    <SidebarMenuItem>
                      <CollapsibleTrigger asChild>
                        <SidebarMenuButton tooltip={project.name}>
                          <ChevronRight className="transition-transform duration-200 group-data-[state=open]/collapsible:rotate-90" />
                          <FolderIcon />
                          <span>{project.name}</span>
                        </SidebarMenuButton>
                      </CollapsibleTrigger>
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
                        </SidebarMenuSub>
                      </CollapsibleContent>
                    </SidebarMenuItem>
                  </Collapsible>
                );
              })}

              {/* Unassigned sessions */}
              {(() => {
                const unassigned = sessionsByProject.get(null) ?? [];
                if (unassigned.length === 0) return null;
                return (
                  <Collapsible defaultOpen className="group/collapsible">
                    <SidebarMenuItem>
                      <CollapsibleTrigger asChild>
                        <SidebarMenuButton tooltip="Threads">
                          <ChevronRight className="transition-transform duration-200 group-data-[state=open]/collapsible:rotate-90" />
                          <MessageSquare />
                          <span>Threads</span>
                        </SidebarMenuButton>
                      </CollapsibleTrigger>
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
                        </SidebarMenuSub>
                      </CollapsibleContent>
                    </SidebarMenuItem>
                  </Collapsible>
                );
              })()}
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
