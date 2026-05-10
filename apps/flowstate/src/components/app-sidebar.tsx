import * as React from "react";
import { useQueryClient } from "@tanstack/react-query";
import { useLocation, useNavigate } from "@tanstack/react-router";
import { open } from "@tauri-apps/plugin-dialog";
import {
  Archive,
  ArchiveRestore,
  BarChart3,
  ChevronRight,
  EllipsisVertical,
  FolderIcon,
  FolderMinus,
  KanbanSquare,
  MoreHorizontal,
  Plus,
  Settings,
  Sparkles,
  SquarePen,
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
  type DragStartEvent,
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
import { isMacOS } from "@/lib/popout";
import { basename } from "@/lib/worktree-utils";
import { WorktreeAwareNewThread } from "@/components/sidebar/worktree-new-thread-dropdown";
import { ThreadItem } from "@/components/sidebar/thread-item";
import { SortableThread } from "@/components/sidebar/sortable-thread";
import {
  SidebarDragSuppressionProvider,
  useSidebarDragSuppressed,
} from "@/components/sidebar/drag-suppression";
import { ADD_PROJECT_EVENT } from "@/lib/keyboard-shortcuts";
import { useDefaultProvider } from "@/hooks/use-default-provider";
import { startThreadOnProject } from "@/lib/start-thread";
import { prefetchSessionsBackground } from "@/lib/queries";
import { toast } from "@/hooks/use-toast";
import type { SessionSummary } from "@/lib/types";
import type { SessionDisplay } from "@/lib/api/display";

/** Sentinel groupId for the unassigned ("General") thread bucket. A
 *  literal string can't collide with a real project_id (those are
 *  UUIDs). Used by SortableThread + the top-level onDragEnd handler
 *  to scope thread drags to within one visual group. */
const GENERAL_GROUP_ID = "__general__";

/** Comparator for sessions inside one visual group.
 *  - Threads with sortOrder == null are "unordered" — they float to
 *    the top, sorted by createdAt DESC. Matches the existing reflex
 *    of "the most recent activity is at the top of the list."
 *  - Threads with sortOrder != null are "ordered" — they sit below
 *    the unordered ones, in fixed sortOrder ASC.
 *  - Tie-break by createdAt DESC then sessionId so the order is
 *    stable even if two sessions race to the same sortOrder via
 *    concurrent reorderSessions writes. */
function compareSessionsForGroup(
  a: SessionSummary,
  b: SessionSummary,
  sessionDisplay: Map<string, SessionDisplay>,
): number {
  const oa = sessionDisplay.get(a.sessionId)?.sortOrder ?? null;
  const ob = sessionDisplay.get(b.sessionId)?.sortOrder ?? null;
  if (oa == null && ob == null) {
    return new Date(b.createdAt).getTime() - new Date(a.createdAt).getTime();
  }
  if (oa == null) return -1;
  if (ob == null) return 1;
  if (oa !== ob) return oa - ob;
  const t = new Date(b.createdAt).getTime() - new Date(a.createdAt).getTime();
  return t !== 0 ? t : a.sessionId.localeCompare(b.sessionId);
}

/**
 * Wraps a single active-project row with dnd-kit's useSortable. The
 * wrapper div owns the visual transform (so the whole Collapsible
 * translates as one block during a drag), but exposes
 * `attributes`/`listeners` to the call site via a render prop so the
 * call site can spread them onto the project NAME row only — not
 * onto the entire wrapper. Putting listeners on the wrapper would
 * mean a click anywhere inside the expanded thread list starts a
 * project drag; threads inside the project need to be independently
 * draggable, so the project hitbox shrinks to just the
 * `SidebarMenuButton`.
 *
 * `defaultOpen` + `group/collapsible` stay on the per-project
 * <Collapsible> so the call site stays declarative.
 *
 * `data: { type: "project" }` is read by the top-level onDragEnd
 * handler to route the drag to handleProjectDragEnd vs. the thread
 * reorder path.
 */
function SortableProject({
  id,
  children,
}: {
  id: string;
  children: (handle: {
    attributes: ReturnType<typeof useSortable>["attributes"];
    listeners: ReturnType<typeof useSortable>["listeners"];
  }) => React.ReactNode;
}) {
  const {
    attributes,
    listeners,
    setNodeRef,
    transform,
    transition,
    isDragging,
  } = useSortable({ id, data: { type: "project" } });

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
      className={cn(
        // touch-none stops the browser from claiming pointer events
        // for native scroll on touch devices — required by dnd-kit
        // for touch-initiated drags. Lives on the wrapper because the
        // wrapper participates in dnd-kit's transform pipeline; the
        // actual {...listeners} are spread onto the row via the
        // render prop below.
        "touch-none",
        isDragging &&
          "rounded-md outline-dashed outline-2 outline-sidebar-accent-foreground/40",
      )}
    >
      <Collapsible defaultOpen className="group/collapsible">
        {children({ attributes, listeners })}
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
  const { state, send, createProject, reorderProjects, reorderSessions } =
    useApp();
  const navigate = useNavigate();
  const location = useLocation();
  const queryClient = useQueryClient();
  const { defaultProvider, loaded: defaultProviderLoaded } =
    useDefaultProvider();
  // Stable notify wrapper for `startThreadOnProject`. The General
  // pencil is the only call site here; keep the helper hoisted so a
  // future "new thread" entry inside this component can reuse it.
  const notifyNewThread = React.useCallback((message: string) => {
    toast({ title: "New thread", description: message, duration: 4000 });
  }, []);
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

  // Boot/idle prefetch of the most-recently-active sessions so that
  // clicking a thread feels instant (the per-row hover prefetch in
  // `thread-item.tsx:119` only helps when the cursor dwells long
  // enough; fast clickers and keyboard users still pay the cold
  // `load_session` round-trip without this).
  //
  // We sort by `updatedAt` (touched on every turn) rather than
  // `createdAt` so a long-lived thread that's still being used
  // outranks a recently-spawned-but-idle one. The cap (top 20) is a
  // soft ceiling chosen because it covers virtually any user's
  // active working set while still finishing in a few seconds at
  // ~1s per cold RPC with concurrency 4.
  //
  // We `.join('|')` the id list before memoising so the dependency
  // is a single string — without that, `recentSessionIds` would get
  // a new array identity on every minor `state.sessions` mutation
  // (a stream event touching one session re-creates the Map) and the
  // effect would tear down + re-arm its prefetch batch on every
  // tick. With the join, the effect re-runs only when the *set* of
  // top-20 ids actually changes.
  const recentSessionIds = React.useMemo(() => {
    const ids: { id: string; updatedAt: number }[] = [];
    for (const s of state.sessions.values()) {
      ids.push({ id: s.sessionId, updatedAt: new Date(s.updatedAt).getTime() });
    }
    ids.sort((a, b) => b.updatedAt - a.updatedAt);
    return ids.slice(0, 20).map((x) => x.id);
  }, [state.sessions]);
  const recentSessionKey = recentSessionIds.join("|");
  React.useEffect(() => {
    if (recentSessionIds.length === 0) return;
    const cancel = prefetchSessionsBackground(queryClient, recentSessionIds, {
      concurrency: 4,
    });
    return cancel;
    // recentSessionIds is derived from recentSessionKey; depending on
    // the key keeps the effect stable across unrelated re-renders.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [recentSessionKey, queryClient]);

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

  // Tracks which project / thread is currently being dragged so
  // <DragOverlay> can render a crisp, natural-size preview that
  // follows the cursor while the in-place source stays dimmed as the
  // "slot it came from". Only one of the two is non-null at a time —
  // the dnd-kit drag lifecycle is single-active by construction.
  const [activeProjectId, setActiveProjectId] = React.useState<string | null>(
    null,
  );
  const [activeThreadId, setActiveThreadId] = React.useState<string | null>(
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
    list.sort((a, b) => compareSessionsForGroup(a, b, state.sessionDisplay));
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
    list.sort((a, b) => compareSessionsForGroup(a, b, state.sessionDisplay));
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
    // `basename` from worktree-utils tolerates both `/` and `\`,
    // so picking `C:\Users\babal\ccc\T` on Windows yields `T`
    // instead of the full path being shoved into the project name.
    const name = basename(path) || path;
    await createProject(path, name);
  }

  // Bridge for the global ⌘⌥N shortcut. Same indirection as the
  // diff/context/editor toggles — the registry dispatches a window
  // CustomEvent and AppSidebar (already mounted in every main-window
  // route) handles the OS folder-picker + createProject call so the
  // dispatch site stays decoupled from Tauri APIs.
  React.useEffect(() => {
    function onAddProject() {
      void handleAddFolder();
    }
    window.addEventListener(ADD_PROJECT_EVENT, onAddProject);
    return () => window.removeEventListener(ADD_PROJECT_EVENT, onAddProject);
    // handleAddFolder closes over `createProject` and `open` (Tauri
    // dialog import) — both stable across renders, so the empty dep
    // array is correct and saves a per-render listener rebind.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

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

  // Resolve the current visual session order for a thread group.
  // groupId is one of:
  //   - GENERAL_GROUP_ID         → unassigned threads
  //   - "archived:<group.key>"   → an archived-project group
  //   - any other string         → a project_id (active project)
  // The returned ids match the order the JSX walks below; `arrayMove`
  // applied to this array produces the post-drop sequence that
  // reorderSessions writes 0..N-1 to.
  function orderedSessionIdsForGroup(groupId: string): string[] {
    if (groupId === GENERAL_GROUP_ID) {
      return unassigned.map((s) => s.sessionId);
    }
    if (groupId.startsWith("archived:")) {
      const key = groupId.slice("archived:".length);
      const group = archivedGroups.find((g) => g.key === key);
      return group ? group.sessions.map((s) => s.sessionId) : [];
    }
    return (sessionsByProject.get(groupId) ?? []).map((s) => s.sessionId);
  }

  // Unified drag handlers for the single top-level DndContext that
  // wraps the whole sidebar. Routes by `active.data.current.type`
  // (set on each useSortable call) to either the existing project
  // reorder path or the new thread reorder path. Cross-group thread
  // drops are silent no-ops via groupId equality on the data payload.
  function handleDragStart(event: DragStartEvent) {
    const type = event.active.data.current?.type;
    if (type === "project") {
      setActiveProjectId(String(event.active.id));
    } else if (type === "thread") {
      setActiveThreadId(String(event.active.id));
    }
  }

  function handleDragCancel() {
    setActiveProjectId(null);
    setActiveThreadId(null);
  }

  function handleDragEnd(event: DragEndEvent) {
    setActiveProjectId(null);
    setActiveThreadId(null);
    const { active, over } = event;
    if (!over || active.id === over.id) return;

    const type = active.data.current?.type;
    if (type === "project") {
      handleProjectDragEnd(event);
      return;
    }
    if (type === "thread") {
      const fromGroup = active.data.current?.groupId as string | undefined;
      const toGroup = over.data.current?.groupId as string | undefined;
      // Within-group only — cross-group drops are silent no-ops.
      // Comparing groupIds carried on the data payload is more
      // reliable than DOM ancestry once dnd-kit has moved things
      // mid-drag.
      if (!fromGroup || fromGroup !== toGroup) return;

      const groupOrder = orderedSessionIdsForGroup(fromGroup);
      const from = groupOrder.indexOf(String(active.id));
      const to = groupOrder.indexOf(String(over.id));
      if (from < 0 || to < 0) return;
      // Fire-and-forget — same fire-and-forget pattern as
      // handleProjectDragEnd; per-session dispatches inside
      // reorderSessions land synchronously so the visual reorder
      // doesn't block on the Tauri writes.
      void reorderSessions(arrayMove(groupOrder, from, to));
    }
  }

  return (
    <Sidebar collapsible="offcanvas">
      <SidebarHeader
        data-tauri-drag-region
        className="h-9 flex-row items-center justify-start border-b border-sidebar-border px-4 py-0"
      >
        {/* macOS traffic-light spacer when the sidebar is expanded —
            traffic lights overlay the window's top-left corner, which
            is here. Tagged as a drag region so the cleared area still
            drags the window. */}
        {isMacOS() && (
          <div className="-ml-2 w-16 shrink-0" data-tauri-drag-region />
        )}
        {/* Wordmark removed — the overlay-style titlebar uses the route
            header on the right of the split as the window's drag bar
            (see chat-view.tsx etc), so the sidebar's top-left now just
            holds the optional attention dot. The h-12 height stays so
            the divider lines up with the route header on the right. */}
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
              "inline-block size-1.5 rounded-full [[data-collapsible=icon]_&]:hidden",
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
              {/* One DndContext for the whole sidebar — projects,
                  General (unassigned) threads, and each archived-
                  project group share sensors so the suppression
                  toggle in CreateWorktreeDialog still works
                  uniformly. Drag routing is by `data.type` on each
                  useSortable: the same `onDragEnd` dispatches
                  to either handleProjectDragEnd or the thread
                  reorder path inside `handleDragEnd`. */}
              <DndContext
                sensors={activeSensors}
                collisionDetection={closestCenter}
                onDragStart={handleDragStart}
                onDragCancel={handleDragCancel}
                onDragEnd={handleDragEnd}
              >
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
                    {/* Project-less ("General") new-thread button.
                        Eager-creates a session with `project_id:
                        undefined` (folder-less; the daemon allows
                        this) using the user's default provider, then
                        navigates straight to `/chat/$sessionId` —
                        same flow as ⌘N and the per-project pencil.
                        Provider/model can be swapped from the chat
                        toolbar after the thread exists. */}
                    <button
                      type="button"
                      className="inline-flex h-6 w-6 shrink-0 items-center justify-center rounded-md text-muted-foreground opacity-0 transition-opacity hover:text-foreground group-hover/project:opacity-100"
                      onClick={(e) => {
                        e.stopPropagation();
                        void startThreadOnProject({
                          projectId: undefined,
                          defaultProvider,
                          defaultProviderLoaded,
                          send,
                          navigate: (sessionId) =>
                            navigate({
                              to: "/chat/$sessionId",
                              params: { sessionId },
                            }),
                          notify: notifyNewThread,
                        });
                      }}
                      aria-label="New thread"
                    >
                      <SquarePen className="h-3.5 w-3.5" />
                    </button>
                  </div>
                  <CollapsibleContent>
                    <SidebarMenuSub>
                      <SortableContext
                        items={unassigned.map((s) => s.sessionId)}
                        strategy={verticalListSortingStrategy}
                      >
                        {unassigned.map((session) => {
                          const wt = worktreeInfo(session);
                          return (
                            <SortableThread
                              key={session.sessionId}
                              id={session.sessionId}
                              groupId={GENERAL_GROUP_ID}
                            >
                              <ThreadItem
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
                            </SortableThread>
                          );
                        })}
                      </SortableContext>
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
                      {({ attributes, listeners }) => (
                        <SidebarMenuItem className="group/project">
                          <SidebarMenuButton
                            tooltip={projectName(project.projectId)}
                            isActive={isActive}
                            onClick={() =>
                              handleProjectClick(project.projectId)
                            }
                            className="cursor-grab pl-7 pr-14"
                            {...attributes}
                            {...listeners}
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
                              onPointerDown={(e) => e.stopPropagation()}
                              className="absolute left-1 top-1 z-10 flex h-6 w-6 items-center justify-center rounded-md text-muted-foreground outline-none hover:text-foreground"
                            >
                              <ChevronRight className="h-4 w-4 transition-transform duration-200 group-data-[state=open]/collapsible:rotate-90" />
                            </button>
                          </CollapsibleTrigger>
                          <div
                            className="absolute right-1 top-1 flex items-center gap-1.5"
                            onPointerDown={(e) => e.stopPropagation()}
                          >
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
                            <WorktreeAwareNewThread
                              projectId={project.projectId}
                              projectPath={project.path}
                            />
                          </div>
                          <CollapsibleContent>
                            <SidebarMenuSub>
                              <SortableContext
                                items={threads.map((s) => s.sessionId)}
                                strategy={verticalListSortingStrategy}
                              >
                                {threads.map((session) => {
                                  const wt = worktreeInfo(session);
                                  // Worktree threads roll up under the
                                  // PARENT project visually, so the
                                  // groupId must be the parent id (what
                                  // the rendering loop iterates) — not
                                  // session.projectId, which would point
                                  // at the worktree-sdk-id and cause
                                  // same-group rejection to fire.
                                  const groupId =
                                    effectiveProjectId(
                                      session.projectId ?? null,
                                    ) ?? project.projectId;
                                  return (
                                    <SortableThread
                                      key={session.sessionId}
                                      id={session.sessionId}
                                      groupId={groupId}
                                    >
                                      <ThreadItem
                                        sessionId={session.sessionId}
                                        title={sessionTitle(session.sessionId)}
                                        updatedAt={session.updatedAt}
                                        isActive={
                                          state.activeSessionId ===
                                          session.sessionId
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
                                    </SortableThread>
                                  );
                                })}
                              </SortableContext>
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
                      )}
                    </SortableProject>
                  );
                })}
              </SortableContext>
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
                                <SortableContext
                                  items={group.sessions.map((s) => s.sessionId)}
                                  strategy={verticalListSortingStrategy}
                                >
                                  {group.sessions.map((session) => (
                                    <SortableThread
                                      key={session.sessionId}
                                      id={session.sessionId}
                                      groupId={`archived:${group.key}`}
                                    >
                                      <SidebarMenuSubItem
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
                                          onPointerDown={(e) =>
                                            e.stopPropagation()
                                          }
                                        >
                                          <button
                                            type="button"
                                            title="Unarchive"
                                            aria-label="Unarchive thread"
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
                                                    session_id:
                                                      session.sessionId,
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
                                    </SortableThread>
                                  ))}
                                </SortableContext>
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

              {/* Portal preview that follows the cursor while a drag
                  is in flight. `dropAnimation={null}` skips the
                  default spring-back — the real list already renders
                  the new order at drop, so animating the overlay
                  back onto it would feel laggy. Renders the project
                  preview if a project is being dragged, otherwise
                  the thread preview if a thread is being dragged.
                  Only one of the two state slots is non-null at a
                  time. */}
              <DragOverlay dropAnimation={null}>
                {activeProjectId ? (
                  <div className="pointer-events-none flex items-center gap-2 rounded-md border border-sidebar-border bg-sidebar px-3 py-2 text-sm font-medium shadow-lg">
                    <FolderIcon className="h-4 w-4 shrink-0" />
                    <span className="max-w-[180px] truncate">
                      {state.projectDisplay.get(activeProjectId)?.name ??
                        "Untitled project"}
                    </span>
                  </div>
                ) : activeThreadId ? (
                  <div className="pointer-events-none flex items-center gap-2 rounded-md border border-sidebar-border bg-sidebar px-3 py-1.5 text-xs shadow-lg">
                    <span className="max-w-[200px] truncate">
                      {state.sessionDisplay.get(activeThreadId)?.title ||
                        "New thread"}
                    </span>
                  </div>
                ) : null}
              </DragOverlay>
              </DndContext>
            </SidebarMenu>
          </SidebarGroupContent>
        </SidebarGroup>
      </SidebarContent>

      <SidebarFooter className="border-t border-sidebar-border">
        <SidebarMenu>
          <MoreMenu
            hasProvisionFailures={hasProvisionFailures}
            provisionFailureCount={provisionFailures.length}
            onNavigate={(to) => {
              navigate({ to });
              closeIfMobile();
            }}
          />
        </SidebarMenu>
      </SidebarFooter>
      <SidebarRail />
    </Sidebar>
  );
}

/**
 * Footer "More" entry. Renders a single sidebar row that on hover
 * (or focus) reveals a side-aligned popover with the full menu —
 * Usage, Settings, Features. We use a controlled DropdownMenu rather
 * than a hover-card so keyboard users still get the menu role +
 * arrow-key navigation; mouse users get the hover affordance the
 * user asked for via onMouseEnter / onMouseLeave on both the trigger
 * and the content (a small close-delay keeps the menu open while
 * the cursor crosses the gap from trigger → menu).
 */
function MoreMenu({
  hasProvisionFailures,
  provisionFailureCount,
  onNavigate,
}: {
  hasProvisionFailures: boolean;
  provisionFailureCount: number;
  onNavigate: (
    to: "/usage" | "/settings" | "/features" | "/orchestrator",
  ) => void;
}) {
  const [open, setOpen] = React.useState(false);

  const ariaLabel = hasProvisionFailures
    ? `More (${provisionFailureCount} provisioning issue${provisionFailureCount === 1 ? "" : "s"})`
    : "More";

  // Click + keyboard are owned by Radix's DropdownMenuTrigger
  // (composed onPointerDown / onKeyDown route through our controlled
  // `open` via onOpenChange). We layer onMouseEnter on top so hover
  // also opens, per the original UX request. Close is purely Radix's
  // job — outside click, Escape, or per-item onClick all funnel
  // through onOpenChange.
  return (
    <SidebarMenuItem>
      <DropdownMenu open={open} onOpenChange={setOpen}>
        <DropdownMenuTrigger asChild>
          <SidebarMenuButton
            aria-label={ariaLabel}
            onMouseEnter={() => setOpen(true)}
          >
            <span className="relative inline-flex">
              <MoreHorizontal />
              {hasProvisionFailures ? (
                <span
                  aria-hidden="true"
                  className="absolute -right-0.5 -top-0.5 inline-block h-2 w-2 rounded-full bg-red-500 ring-1 ring-background"
                />
              ) : null}
            </span>
            <span>More</span>
          </SidebarMenuButton>
        </DropdownMenuTrigger>
        <DropdownMenuContent
          side="top"
          align="start"
          sideOffset={8}
          // Width matches the sidebar's footer width well at the
          // default sidebar width (256px). Items get larger hit
          // targets via py-2 + bigger gap so the menu reads as
          // peer-weight to the items in the sidebar above it,
          // instead of the cramped default.
          className="w-56 p-1.5"
        >
          <DropdownMenuItem
            onClick={() => onNavigate("/orchestrator")}
            className="gap-2.5 px-2.5 py-2 text-sm"
          >
            <KanbanSquare className="h-4 w-4" />
            <span>Orchestrator</span>
          </DropdownMenuItem>
          <DropdownMenuItem
            onClick={() => onNavigate("/usage")}
            className="gap-2.5 px-2.5 py-2 text-sm"
          >
            <BarChart3 className="h-4 w-4" />
            <span>Usage</span>
          </DropdownMenuItem>
          <DropdownMenuItem
            onClick={() => onNavigate("/settings")}
            className="gap-2.5 px-2.5 py-2 text-sm"
          >
            <span className="relative inline-flex">
              <Settings className="h-4 w-4" />
              {hasProvisionFailures ? (
                <span
                  aria-hidden="true"
                  className="absolute -right-0.5 -top-0.5 inline-block h-1.5 w-1.5 rounded-full bg-red-500 ring-1 ring-popover"
                />
              ) : null}
            </span>
            <span>Settings</span>
          </DropdownMenuItem>
          <DropdownMenuItem
            onClick={() => onNavigate("/features")}
            className="gap-2.5 px-2.5 py-2 text-sm"
          >
            <Sparkles className="h-4 w-4" />
            <span>Features</span>
          </DropdownMenuItem>
        </DropdownMenuContent>
      </DropdownMenu>
    </SidebarMenuItem>
  );
}
