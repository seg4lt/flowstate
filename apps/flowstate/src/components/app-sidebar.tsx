import * as React from "react";
import { useLocation, useNavigate } from "@tanstack/react-router";
import { open } from "@tauri-apps/plugin-dialog";
import {
  Archive,
  BarChart3,
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
  DndContext,
  DragOverlay,
  KeyboardSensor,
  PointerSensor,
  closestCenter,
  useSensor,
  useSensors,
  type DragEndEvent,
} from "@dnd-kit/core";
import {
  SortableContext,
  arrayMove,
  sortableKeyboardCoordinates,
  useSortable,
  verticalListSortingStrategy,
} from "@dnd-kit/sortable";
import { CSS } from "@dnd-kit/utilities";
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
  useSidebar,
} from "@/components/ui/sidebar";
import { useApp, useProvisionFailures } from "@/stores/app-store";
import { useAttentionTone } from "@/hooks/use-attention-tone";
import { cn } from "@/lib/utils";
import { ProviderDropdown } from "@/components/sidebar/provider-dropdown";
import { WorktreeAwareNewThread } from "@/components/sidebar/worktree-new-thread-dropdown";
import { ThreadItem } from "@/components/sidebar/thread-item";
import {
  SidebarDragSuppressionProvider,
  useSidebarDragSuppressed,
} from "@/components/sidebar/drag-suppression";
import type { SessionSummary } from "@/lib/types";

/**
 * Wraps a single active-project row with dnd-kit's useSortable so
 * the whole Collapsible can be picked up and moved. The wrapper div
 * also owns the Collapsible — we don't apply the sortable ref
 * directly on <Collapsible> because the shared `ui/collapsible.tsx`
 * wrapper is a plain function component (no forwardRef in React 18)
 * so refs don't propagate through it.
 *
 * `defaultOpen` + `group/collapsible` that used to live on the
 * per-project <Collapsible> are now owned by this component so the
 * call site stays declarative.
 */
function SortableProject({
  id,
  children,
}: {
  id: string;
  children: React.ReactNode;
}) {
  const {
    attributes,
    listeners,
    setNodeRef,
    transform,
    transition,
    isDragging,
  } = useSortable({ id });

  const style: React.CSSProperties = {
    // Translate only — dropping the Scale component kills the text /
    // icon "zoom" effect that happens when dragging between rows of
    // different heights (a collapsed project vs. an expanded one).
    // dnd-kit's default Transform string includes a scaleX/scaleY
    // computed from the sibling size delta, which propagates to every
    // child of the dragged element.
    transform: CSS.Translate.toString(transform),
    transition,
    // Source stays visible but clearly dim — it reads as an empty
    // slot while the DragOverlay renders the crisp preview elsewhere.
    opacity: isDragging ? 0.35 : undefined,
    zIndex: isDragging ? 10 : undefined,
  };

  return (
    <div
      ref={setNodeRef}
      style={style}
      {...attributes}
      {...listeners}
      className={cn(
        // touch-none stops the browser from claiming pointer events
        // for native scroll on touch devices — required by dnd-kit
        // for touch-initiated drags.
        "cursor-grab touch-none",
        isDragging &&
          "cursor-grabbing rounded-md outline-dashed outline-2 outline-sidebar-accent-foreground/40",
      )}
    >
      <Collapsible defaultOpen className="group/collapsible">
        {children}
      </Collapsible>
    </div>
  );
}

export function AppSidebar() {
  // The provider lives at the AppSidebar root so any dialog opened from
  // inside the sidebar tree (worktree new-thread, rename, etc.) can
  // temporarily disable drag sensors via `useSuppressSidebarDrag`.
  return (
    <SidebarDragSuppressionProvider>
      <AppSidebarBody />
    </SidebarDragSuppressionProvider>
  );
}

function AppSidebarBody() {
  const { state, send, createProject, reorderProjects } = useApp();
  const navigate = useNavigate();
  const location = useLocation();
  // Red dot on the footer Settings icon when one or more
  // runtime-provisioning phases failed at boot. Drives only the
  // visual indicator below; the Settings page itself renders the
  // banner + Retry buttons.
  const provisionFailures = useProvisionFailures();
  const hasProvisionFailures = provisionFailures.length > 0;
  // Aggregate "wants attention" tone across all non-active threads.
  // Drives the dot rendered next to the "flowstate" wordmark below
  // so a long thread list still has a persistent cue when the
  // attention-wanting row is scrolled out of view. Collapsed sidebar
  // relies on SidebarTrigger's dot instead (ui/sidebar.tsx).
  const attentionTone = useAttentionTone();

  // Drag-to-reorder on the active projects list. Pointer sensor with
  // a 6px distance activation so normal short clicks still fire
  // navigation; any pointer movement past 6px starts a drag instead.
  // Keyboard sensor gives Space-to-pick-up / Arrow-to-move parity for
  // non-mouse users.
  const sensors = useSensors(
    useSensor(PointerSensor, { activationConstraint: { distance: 6 } }),
    useSensor(KeyboardSensor, { coordinateGetter: sortableKeyboardCoordinates }),
  );
  // When a sidebar-owned modal (e.g. CreateWorktreeDialog) is open,
  // suppress drag activation entirely. Otherwise Space/Enter inside
  // the dialog can re-fire on a still-focused sortable and pick up
  // a project for reorder. Empty array = neither pointer nor keyboard
  // can start a drag until the modal closes.
  const dragSuppressed = useSidebarDragSuppressed();
  const activeSensors = dragSuppressed ? [] : sensors;

  // Active projects sorted by user-chosen sort_order. Projects with
  // sortOrder === null (newly created / never manually reordered)
  // sink to the bottom, alphabetical among themselves — so the
  // user's explicit arrangement always wins over insertion order.
  // Worktree-child projects are filtered out (rolled up visually
  // under their parent elsewhere in this file).
  const sortedActiveProjects = React.useMemo(() => {
    const worktreeIds = new Set(state.projectWorktrees.keys());
    const nameFor = (projectId: string) =>
      state.projectDisplay.get(projectId)?.name ?? "Untitled project";
    return state.projects
      .filter((p) => !worktreeIds.has(p.projectId))
      .slice()
      .sort((a, b) => {
        const oa = state.projectDisplay.get(a.projectId)?.sortOrder;
        const ob = state.projectDisplay.get(b.projectId)?.sortOrder;
        if (oa == null && ob == null) {
          return nameFor(a.projectId).localeCompare(nameFor(b.projectId));
        }
        if (oa == null) return 1;
        if (ob == null) return -1;
        return oa - ob;
      });
  }, [state.projects, state.projectDisplay, state.projectWorktrees]);

  const handleProjectDragEnd = React.useCallback(
    (event: DragEndEvent) => {
      const { active, over } = event;
      if (!over || active.id === over.id) return;
      const ids = sortedActiveProjects.map((p) => p.projectId);
      const from = ids.indexOf(String(active.id));
      const to = ids.indexOf(String(over.id));
      if (from < 0 || to < 0) return;
      const reordered = arrayMove(ids, from, to);
      // Fire-and-forget — the per-project dispatches inside
      // reorderProjects land synchronously, so the UI updates on the
      // next render; the Tauri writes happen in parallel and don't
      // block the visual reorder.
      void reorderProjects(reordered);
    },
    [sortedActiveProjects, reorderProjects],
  );

  // Tracks which project is currently being dragged so <DragOverlay>
  // can render a crisp, natural-size preview that follows the cursor
  // while the in-place source stays dimmed as the "slot it came from".
  const [activeProjectId, setActiveProjectId] = React.useState<string | null>(
    null,
  );
  // On a narrow window the sidebar renders as a full-screen Sheet
  // overlay that covers the chat view. Without `closeIfMobile()` the
  // sheet stays open after the user picks a thread and they have to
  // hunt for the backdrop to dismiss it — "open close is broken" as
  // far as the user is concerned. Desktop ignores it.
  const { isMobile, setOpenMobile } = useSidebar();
  const closeIfMobile = React.useCallback(() => {
    if (isMobile) setOpenMobile(false);
  }, [isMobile, setOpenMobile]);

  // Look up display metadata (titles, names) from the app-side store.
  // The SDK only knows ids + runtime state; anything the user sees as
  // a label comes from `state.sessionDisplay` / `state.projectDisplay`.
  const sessionTitle = (sessionId: string): string => {
    return state.sessionDisplay.get(sessionId)?.title ?? "";
  };
  const projectName = (projectId: string): string => {
    return state.projectDisplay.get(projectId)?.name ?? "Untitled project";
  };

  // Group sessions by project. Sessions whose projectId points at a
  // project that no longer exists (deleted/tombstoned) are filtered
  // out entirely instead of being dumped into the unassigned bucket
  // — they're "hibernating" until the user re-adds the same folder
  // as a project, at which point the persistence layer un-tombstones
  // the original project_id and they reappear under it.
  //
  // Worktree projects are a special case: each git worktree gets its
  // own SDK project so the agent runs with cwd = worktree folder,
  // but visually the user sees ONE project in the sidebar. We pull
  // the parent projectId out of `projectWorktrees` and use that as
  // the grouping key so worktree threads land under the main repo's
  // section, not as separate top-level entries.
  const knownProjectIds = new Set(state.projects.map((p) => p.projectId));
  const effectiveProjectId = (rawProjectId: string | null): string | null => {
    if (!rawProjectId) return null;
    return (
      state.projectWorktrees.get(rawProjectId)?.parentProjectId ?? rawProjectId
    );
  };
  const sessionsByProject = new Map<string | null, SessionSummary[]>();
  for (const session of state.sessions.values()) {
    if (session.projectId && !knownProjectIds.has(session.projectId)) {
      continue;
    }
    const key = effectiveProjectId(session.projectId ?? null);
    const list = sessionsByProject.get(key) ?? [];
    list.push(session);
    sessionsByProject.set(key, list);
  }
  for (const list of sessionsByProject.values()) {
    list.sort(
      (a, b) =>
        new Date(b.createdAt).getTime() - new Date(a.createdAt).getTime(),
    );
  }

  // Group archived sessions by project
  const projectNameMap = new Map<string, string>();
  for (const p of state.projects) {
    projectNameMap.set(p.projectId, projectName(p.projectId));
  }

  const archivedByProject = new Map<string | null, SessionSummary[]>();
  for (const session of state.archivedSessions) {
    // Mirror the active-session grouping above: sessions tied to a
    // tombstoned/deleted project are filtered out entirely (they
    // stay hibernating until the folder is re-added) rather than
    // spilling into "General" and inflating its count. After that
    // filter, worktree projects roll up to their parent so archived
    // worktree threads show under the same visual project they
    // always did. Only sessions with a truly null projectId land
    // in "General".
    if (session.projectId && !knownProjectIds.has(session.projectId)) {
      continue;
    }
    const rolledUp = effectiveProjectId(session.projectId ?? null);
    const key = rolledUp && projectNameMap.has(rolledUp) ? rolledUp : null;
    const list = archivedByProject.get(key) ?? [];
    list.push(session);
    archivedByProject.set(key, list);
  }
  for (const list of archivedByProject.values()) {
    list.sort(
      (a, b) =>
        new Date(b.createdAt).getTime() - new Date(a.createdAt).getTime(),
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
    await createProject(path, name);
  }

  async function handleRemoveProject(projectId: string) {
    await send({ type: "delete_project", project_id: projectId });
  }

  function handleThreadClick(sessionId: string) {
    navigate({ to: "/chat/$sessionId", params: { sessionId } });
    closeIfMobile();
  }

  function handleProjectClick(projectId: string) {
    navigate({ to: "/project/$projectId", params: { projectId } });
    closeIfMobile();
  }

  // Pull worktree metadata for a given session's SDK project. If the
  // session is tied to a project_worktree row we surface the branch
  // label + the worktree folder path so ThreadItem can render the
  // branch icon + tooltip. For main-project threads both are null.
  function worktreeInfo(session: SessionSummary): {
    branch: string | null;
    path: string | null;
  } {
    if (!session.projectId) return { branch: null, path: null };
    const link = state.projectWorktrees.get(session.projectId);
    if (!link) return { branch: null, path: null };
    const wtProject = state.projects.find(
      (p) => p.projectId === session.projectId,
    );
    return { branch: link.branch ?? null, path: wtProject?.path ?? null };
  }

  const unassigned = sessionsByProject.get(null) ?? [];

  return (
    <Sidebar collapsible="offcanvas">
      <SidebarHeader className="h-12 flex-row items-center justify-start border-b border-sidebar-border px-4 py-0">
        <span className="text-sm font-semibold tracking-tight [[data-collapsible=icon]_&]:hidden">
          flowstate
        </span>
        {attentionTone && (
          <span
            aria-label={
              attentionTone === "awaiting"
                ? "One or more threads need a response"
                : "One or more threads finished"
            }
            title={
              attentionTone === "awaiting"
                ? "One or more threads need a response"
                : "One or more threads finished"
            }
            className={cn(
              "ml-2 inline-block size-1.5 rounded-full [[data-collapsible=icon]_&]:hidden",
              attentionTone === "awaiting" ? "bg-blue-500" : "bg-green-500",
            )}
          />
        )}
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
                    <SidebarMenuButton tooltip="General">
                      <ChevronRight className="transition-transform duration-200 group-data-[state=open]/collapsible:rotate-90" />
                      <MessageSquare />
                      <span className="flex-1 truncate">General</span>
                    </SidebarMenuButton>
                  </CollapsibleTrigger>
                  <div className="absolute right-1 top-1">
                    <ProviderDropdown />
                  </div>
                  <CollapsibleContent>
                    <SidebarMenuSub>
                      {unassigned.map((session) => {
                        const wt = worktreeInfo(session);
                        return (
                          <ThreadItem
                            key={session.sessionId}
                            sessionId={session.sessionId}
                            title={sessionTitle(session.sessionId)}
                            updatedAt={session.updatedAt}
                            isActive={
                              state.activeSessionId === session.sessionId
                            }
                            worktreeBranch={wt.branch}
                            worktreePath={wt.path}
                            running={session.status === "running"}
                            awaitingInput={state.awaitingInputSessionIds.has(
                              session.sessionId,
                            )}
                            pendingDone={state.doneSessionIds.has(
                              session.sessionId,
                            )}
                            onClick={() =>
                              handleThreadClick(session.sessionId)
                            }
                          />
                        );
                      })}
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

              <DndContext
                sensors={activeSensors}
                collisionDetection={closestCenter}
                onDragStart={(event) =>
                  setActiveProjectId(String(event.active.id))
                }
                onDragCancel={() => setActiveProjectId(null)}
                onDragEnd={(event) => {
                  setActiveProjectId(null);
                  handleProjectDragEnd(event);
                }}
              >
                <SortableContext
                  items={sortedActiveProjects.map((p) => p.projectId)}
                  strategy={verticalListSortingStrategy}
                >
                  {sortedActiveProjects.map((project) => {
                    const threads =
                      sessionsByProject.get(project.projectId) ?? [];
                    const isActive =
                      location.pathname === `/project/${project.projectId}`;
                    return (
                      <SortableProject
                        key={project.projectId}
                        id={project.projectId}
                      >
                        <SidebarMenuItem className="group/project">
                      <SidebarMenuButton
                        tooltip={projectName(project.projectId)}
                        isActive={isActive}
                        onClick={() => handleProjectClick(project.projectId)}
                        className="pl-7 pr-14"
                      >
                        <FolderIcon />
                        <span className="flex-1 truncate">
                          {projectName(project.projectId)}
                        </span>
                      </SidebarMenuButton>
                      {/* The chevron is its own CollapsibleTrigger now so
                          the rest of the row is free to navigate. Anchored
                          to `top-1` (not `top-1/2`) because SidebarMenuItem
                          is the li that also contains CollapsibleContent —
                          "50%" of that expanded box is halfway down the
                          thread list, not halfway down the header row.
                          Same trick the right-side action cluster uses. */}
                      <CollapsibleTrigger asChild>
                        <button
                          type="button"
                          aria-label={`Toggle ${projectName(project.projectId)} threads`}
                          onClick={(e) => e.stopPropagation()}
                          className="absolute left-1 top-1 z-10 flex h-6 w-6 items-center justify-center rounded-md text-muted-foreground outline-none hover:text-foreground"
                        >
                          <ChevronRight className="h-4 w-4 transition-transform duration-200 group-data-[state=open]/collapsible:rotate-90" />
                        </button>
                      </CollapsibleTrigger>
                      <div className="absolute right-1 top-1 flex items-center gap-1.5">
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
                        <WorktreeAwareNewThread projectId={project.projectId} projectPath={project.path} />
                      </div>
                      <CollapsibleContent>
                        <SidebarMenuSub>
                          {threads.map((session) => {
                            const wt = worktreeInfo(session);
                            return (
                              <ThreadItem
                                key={session.sessionId}
                                sessionId={session.sessionId}
                                title={sessionTitle(session.sessionId)}
                                updatedAt={session.updatedAt}
                                isActive={
                                  state.activeSessionId === session.sessionId
                                }
                                worktreeBranch={wt.branch}
                                worktreePath={wt.path}
                                running={session.status === "running"}
                                awaitingInput={state.awaitingInputSessionIds.has(
                                  session.sessionId,
                                )}
                                pendingDone={state.doneSessionIds.has(
                                  session.sessionId,
                                )}
                                onClick={() =>
                                  handleThreadClick(session.sessionId)
                                }
                              />
                            );
                          })}
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
                      </SortableProject>
                    );
                  })}
                </SortableContext>
                {/* Portal preview of the dragged project. `dropAnimation={null}`
                    skips the default spring-back — the real list already
                    renders the new order at drop, so animating the overlay
                    back onto it would feel laggy. */}
                <DragOverlay dropAnimation={null}>
                  {activeProjectId ? (
                    <div className="pointer-events-none flex items-center gap-2 rounded-md border border-sidebar-border bg-sidebar px-3 py-2 text-sm font-medium shadow-lg">
                      <FolderIcon className="h-4 w-4 shrink-0" />
                      <span className="max-w-[180px] truncate">
                        {state.projectDisplay.get(activeProjectId)?.name ??
                          "Untitled project"}
                      </span>
                    </div>
                  ) : null}
                </DragOverlay>
              </DndContext>
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
                                    <SidebarMenuSubButton
                                      className="h-7 w-full min-w-0 cursor-pointer rounded-r-none pr-12"
                                      onClick={() =>
                                        handleThreadClick(session.sessionId)
                                      }
                                    >
                                      <span className="flex-1 truncate text-xs">
                                        {sessionTitle(session.sessionId) ||
                                          "New thread"}
                                      </span>
                                    </SidebarMenuSubButton>
                                    <div
                                      className="absolute right-1 top-1/2 flex -translate-y-1/2 items-center gap-0.5 opacity-0 transition-opacity group-hover/thread:opacity-100 has-[[data-state=open]]:opacity-100"
                                      onClick={(e) => e.stopPropagation()}
                                      onKeyDown={(e) => e.stopPropagation()}
                                    >
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
              tooltip="Usage"
              onClick={() => {
                navigate({ to: "/usage" });
                closeIfMobile();
              }}
            >
              <BarChart3 />
              <span>Usage</span>
            </SidebarMenuButton>
          </SidebarMenuItem>
          <SidebarMenuItem>
            <SidebarMenuButton
              tooltip={
                hasProvisionFailures
                  ? `Settings (${provisionFailures.length} provisioning issue${provisionFailures.length === 1 ? "" : "s"})`
                  : "Settings"
              }
              onClick={() => {
                navigate({ to: "/settings" });
                closeIfMobile();
              }}
            >
              {/* Wrap the icon in a relative span so the absolute
                  red dot is positioned to the icon's top-right rather
                  than the SidebarMenuButton's. The dot is purely
                  decorative — the banner inside Settings tells the
                  user what actually broke. */}
              <span className="relative inline-flex">
                <Settings />
                {hasProvisionFailures && (
                  <span
                    aria-hidden="true"
                    className="absolute -right-0.5 -top-0.5 inline-block h-2 w-2 rounded-full bg-red-500 ring-1 ring-background"
                  />
                )}
              </span>
              <span>Settings</span>
            </SidebarMenuButton>
          </SidebarMenuItem>
        </SidebarMenu>
      </SidebarFooter>
      <SidebarRail />
    </Sidebar>
  );
}
