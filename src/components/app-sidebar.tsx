import { useNavigate } from "@tanstack/react-router";
import { open } from "@tauri-apps/plugin-dialog";
import {
  Archive,
  ArchiveRestore,
  ChevronRight,
  EllipsisVertical,
  FolderIcon,
  FolderMinus,
  Plus,
  Settings,
  MessageSquare,
  Trash2,
} from "lucide-react";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
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
import { ThreadItem } from "@/components/sidebar/thread-item";
import type { SessionSummary } from "@/lib/types";

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

  // Group archived sessions by project
  const projectNameMap = new Map<string, string>();
  for (const p of state.projects) {
    projectNameMap.set(p.projectId, p.name);
  }

  const archivedByProject = new Map<string | null, SessionSummary[]>();
  for (const session of state.archivedSessions) {
    // If projectId exists and is still a known project, group under it; otherwise "General"
    const key =
      session.projectId && projectNameMap.has(session.projectId)
        ? session.projectId
        : null;
    const list = archivedByProject.get(key) ?? [];
    list.push(session);
    archivedByProject.set(key, list);
  }
  for (const list of archivedByProject.values()) {
    list.sort(
      (a, b) =>
        new Date(b.updatedAt).getTime() - new Date(a.updatedAt).getTime(),
    );
  }

  // Build sorted groups: named projects alphabetically, then "General" last
  const archivedGroups: {
    key: string;
    name: string;
    sessions: SessionSummary[];
  }[] = [];
  const namedGroups: typeof archivedGroups = [];
  for (const [projectId, sessions] of archivedByProject) {
    if (projectId) {
      namedGroups.push({
        key: projectId,
        name: projectNameMap.get(projectId) ?? "Unknown",
        sessions,
      });
    }
  }
  namedGroups.sort((a, b) => a.name.localeCompare(b.name));
  archivedGroups.push(...namedGroups);
  const generalSessions = archivedByProject.get(null);
  if (generalSessions && generalSessions.length > 0) {
    archivedGroups.push({
      key: "__general__",
      name: "General",
      sessions: generalSessions,
    });
  }

  async function handleAddFolder() {
    const selected = await open({ directory: true, multiple: false });
    if (!selected) return;
    const path = typeof selected === "string" ? selected : selected[0];
    if (!path) return;
    const name = path.split("/").pop() ?? path;
    await send({ type: "create_project", name, path });
  }

  async function handleRemoveProject(projectId: string) {
    await send({ type: "delete_project", project_id: projectId });
  }

  function handleThreadClick(sessionId: string) {
    navigate({ to: "/chat/$sessionId", params: { sessionId } });
  }

  const unassigned = sessionsByProject.get(null) ?? [];

  return (
    <Sidebar collapsible="offcanvas">
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
                        <ThreadItem
                          key={session.sessionId}
                          sessionId={session.sessionId}
                          title={session.title}
                          updatedAt={session.updatedAt}
                          isActive={
                            state.activeSessionId === session.sessionId
                          }
                          running={session.status === "running"}
                          onClick={() =>
                            handleThreadClick(session.sessionId)
                          }
                        />
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
                      <div className="absolute right-1 top-1 flex items-center gap-0.5">
                        <button
                          type="button"
                          title="Remove project"
                          className="inline-flex h-6 w-6 shrink-0 items-center justify-center rounded-md text-muted-foreground opacity-0 transition-opacity hover:text-destructive group-hover/project:opacity-100"
                          onClick={(e) => {
                            e.stopPropagation();
                            handleRemoveProject(project.projectId);
                          }}
                        >
                          <FolderMinus className="h-3.5 w-3.5" />
                        </button>
                        <ProviderDropdown projectId={project.projectId} />
                      </div>
                      <CollapsibleContent>
                        <SidebarMenuSub>
                          {threads.map((session) => (
                            <ThreadItem
                              key={session.sessionId}
                              sessionId={session.sessionId}
                              title={session.title}
                              updatedAt={session.updatedAt}
                              isActive={
                                state.activeSessionId === session.sessionId
                              }
                              running={session.status === "running"}
                              onClick={() =>
                                handleThreadClick(session.sessionId)
                              }
                            />
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
              {/* Archived threads */}
              <Collapsible
                className="group/collapsible"
                onOpenChange={(open) => {
                  if (open) {
                    send({ type: "list_archived_sessions" });
                  }
                }}
              >
                <SidebarMenuItem>
                  <CollapsibleTrigger asChild>
                    <SidebarMenuButton tooltip="Archived">
                      <ChevronRight className="transition-transform duration-200 group-data-[state=open]/collapsible:rotate-90" />
                      <Archive />
                      <span className="flex-1 truncate">Archived</span>
                    </SidebarMenuButton>
                  </CollapsibleTrigger>
                  <CollapsibleContent>
                    <SidebarMenuSub>
                      {archivedGroups.map((group) => (
                        <Collapsible
                          key={group.key}
                          defaultOpen
                          className="group/archived-project"
                        >
                          <SidebarMenuSubItem>
                            <CollapsibleTrigger asChild>
                              <SidebarMenuSubButton className="h-7 w-full min-w-0">
                                <ChevronRight className="h-3 w-3 shrink-0 transition-transform duration-200 group-data-[state=open]/archived-project:rotate-90" />
                                <span className="flex-1 truncate text-xs font-medium">
                                  {group.name}
                                </span>
                                <span className="text-[10px] tabular-nums text-muted-foreground">
                                  {group.sessions.length}
                                </span>
                              </SidebarMenuSubButton>
                            </CollapsibleTrigger>
                            <CollapsibleContent>
                              <SidebarMenuSub className="mx-2 px-1.5">
                                {group.sessions.map((session) => (
                                  <SidebarMenuSubItem
                                    key={session.sessionId}
                                    className="group/thread -mr-6"
                                  >
                                    <SidebarMenuSubButton className="h-7 w-full min-w-0 rounded-r-none pr-12">
                                      <span className="flex-1 truncate text-xs">
                                        {session.title || "New thread"}
                                      </span>
                                    </SidebarMenuSubButton>
                                    <div
                                      className="absolute right-1 top-1/2 flex -translate-y-1/2 items-center gap-0.5 opacity-0 transition-opacity group-hover/thread:opacity-100 has-[[data-state=open]]:opacity-100"
                                      onClick={(e) => e.stopPropagation()}
                                      onKeyDown={(e) => e.stopPropagation()}
                                    >
                                      <button
                                        type="button"
                                        title="Unarchive"
                                        className="inline-flex h-5 w-5 items-center justify-center rounded-md text-sidebar-foreground outline-none hover:bg-sidebar-accent"
                                        onClick={() =>
                                          send({
                                            type: "unarchive_session",
                                            session_id: session.sessionId,
                                          })
                                        }
                                      >
                                        <ArchiveRestore className="h-3 w-3" />
                                      </button>
                                      <DropdownMenu>
                                        <DropdownMenuTrigger asChild>
                                          <button
                                            type="button"
                                            className="inline-flex h-5 w-5 items-center justify-center rounded-md text-sidebar-foreground outline-none hover:bg-sidebar-accent"
                                          >
                                            <EllipsisVertical className="h-3 w-3" />
                                          </button>
                                        </DropdownMenuTrigger>
                                        <DropdownMenuContent
                                          align="start"
                                          className="min-w-32"
                                        >
                                          <DropdownMenuItem
                                            variant="destructive"
                                            onClick={() =>
                                              send({
                                                type: "delete_session",
                                                session_id: session.sessionId,
                                              })
                                            }
                                          >
                                            <Trash2 className="mr-2 h-3.5 w-3.5" />
                                            Delete
                                          </DropdownMenuItem>
                                        </DropdownMenuContent>
                                      </DropdownMenu>
                                    </div>
                                  </SidebarMenuSubItem>
                                ))}
                              </SidebarMenuSub>
                            </CollapsibleContent>
                          </SidebarMenuSubItem>
                        </Collapsible>
                      ))}
                      {state.archivedSessions.length === 0 && (
                        <SidebarMenuSubItem>
                          <span className="px-2 py-1 text-xs text-muted-foreground">
                            No archived threads
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
            <SidebarMenuButton
              tooltip="Settings"
              onClick={() => navigate({ to: "/settings" })}
            >
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
