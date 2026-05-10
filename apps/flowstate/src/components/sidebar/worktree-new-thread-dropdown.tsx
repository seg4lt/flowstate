import * as React from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { useNavigate } from "@tanstack/react-router";
import { GitBranch, Loader2, Plus, SquarePen } from "lucide-react";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { useApp } from "@/stores/app-store";
import type { GitWorktree } from "@/lib/api";
import {
  gitWorktreeListQueryOptions,
  gitBranchQueryOptions,
} from "@/lib/queries";
import { CreateWorktreeDialog } from "@/components/project/create-worktree-dialog";
import { samePath } from "@/lib/worktree-utils";
import { toast } from "@/hooks/use-toast";
import { useDefaultProvider } from "@/hooks/use-default-provider";
import { startThreadOnProject } from "@/lib/start-thread";
import { useSuppressSidebarDrag } from "./drag-suppression";

interface WorktreeAwareNewThreadProps {
  projectId: string;
  projectPath: string | undefined;
}

/**
 * Sidebar "new thread" pencil button. Provider selection has been
 * deferred to the chat view's toolbar (see commit 83c9f97 for the
 * mid-session swap support), so this dropdown's only job now is to
 * pick *where* the new thread runs:
 *
 *   - On the main project's directory (the default).
 *   - On a specific git worktree (when the project has more than one).
 *   - On a brand-new worktree (via the create-worktree dialog).
 *
 * After the pick we provision the worktree-as-project if needed and
 * eagerly call `start_session` (via `startThreadOnProject`) to spawn
 * the real thread — same flow ⌘N uses. The session row appears in
 * the sidebar instantly and ChatView mounts on the real
 * `/chat/$sessionId` route, so the daemon's first stream of
 * permission / tool-call events lands on a fully-mounted view
 * instead of racing the deferred-create navigation that the old
 * `/chat/draft/$projectId` path suffered.
 */
export function WorktreeAwareNewThread({
  projectId,
  projectPath,
}: WorktreeAwareNewThreadProps) {
  // Without a project path we can't enumerate worktrees; navigate to
  // the draft route directly with no dropdown.
  if (!projectPath) {
    return <DirectDraftButton projectId={projectId} />;
  }

  return (
    <WorktreeDropdownInner projectId={projectId} projectPath={projectPath} />
  );
}

// ── No-path projects: bare button that eager-creates a thread ──────
//
// Folder-less and path-less projects skip the worktree dropdown
// entirely (no worktrees to enumerate). Click-to-eager-create mirrors
// the ⌘N flow: spawn the session with the user's default provider,
// then navigate straight into `/chat/$sessionId`.

function DirectDraftButton({ projectId }: { projectId: string }) {
  const navigate = useNavigate();
  const { send } = useApp();
  const { defaultProvider, loaded: defaultProviderLoaded } =
    useDefaultProvider();
  const notify = React.useCallback((message: string) => {
    toast({ title: "New thread", description: message, duration: 4000 });
  }, []);
  return (
    <button
      type="button"
      className="inline-flex h-6 w-6 shrink-0 items-center justify-center rounded-md text-muted-foreground opacity-0 transition-opacity hover:text-foreground group-hover/project:opacity-100"
      onClick={(e) => {
        e.stopPropagation();
        void startThreadOnProject({
          projectId,
          defaultProvider,
          defaultProviderLoaded,
          send,
          navigate: (sessionId) =>
            navigate({
              to: "/chat/$sessionId",
              params: { sessionId },
            }),
          notify,
        });
      }}
      aria-label="New thread"
    >
      <SquarePen className="h-3.5 w-3.5" />
    </button>
  );
}

// ── Inner component — only rendered when projectPath is defined ─────

function WorktreeDropdownInner({
  projectId,
  projectPath,
}: {
  projectId: string;
  projectPath: string;
}) {
  const [open, setOpen] = React.useState(false);
  const [createWtOpen, setCreateWtOpen] = React.useState(false);
  // Disable sidebar drag sensors while this dialog is open. Without
  // this, Space/Enter inside the dialog can re-fire on a still-
  // focused sortable project row and start a keyboard drag.
  useSuppressSidebarDrag(createWtOpen);

  const { state, send, createProject, linkProjectWorktree } = useApp();
  const navigate = useNavigate();
  const queryClient = useQueryClient();
  const { defaultProvider, loaded: defaultProviderLoaded } =
    useDefaultProvider();
  const notify = React.useCallback((message: string) => {
    toast({ title: "New thread", description: message, duration: 4000 });
  }, []);
  const navigateToSession = React.useCallback(
    (sessionId: string) => {
      navigate({ to: "/chat/$sessionId", params: { sessionId } });
    },
    [navigate],
  );

  // Fire-and-forget: the ChatView we're about to navigate into reads
  // the cached branch for its project path. Poking the cache here means
  // the new thread loads with a fresh branch even if git state moved
  // out-of-band since the last fetch.
  const refreshBranchAsync = React.useCallback(
    (path: string) => {
      void queryClient.invalidateQueries({
        queryKey: ["git", "branch", path],
      });
    },
    [queryClient],
  );

  // ── Worktree query (lazy — only when dropdown is open) ────────
  const worktreeQuery = useQuery({
    ...gitWorktreeListQueryOptions(projectPath),
    enabled: open,
  });
  const worktrees = worktreeQuery.data ?? [];
  const hasMultipleWorktrees = worktrees.length > 1;

  // Current branch — needed by CreateWorktreeDialog as baseRef.
  const branchQuery = useQuery({
    ...gitBranchQueryOptions(projectPath),
    enabled: createWtOpen,
  });
  const currentBranch = branchQuery.data ?? "";

  // ── Eager-create a thread on a specific worktree ──────────────
  // Provisions the worktree-as-project record if needed (so its
  // threads run with cwd = worktree folder), then `start_session`s
  // immediately and navigates straight to `/chat/$sessionId`. Order
  // matters: the worktree project record MUST exist before
  // `start_session` references its `projectId`, otherwise the daemon
  // can't resolve the cwd and the spawn fails.
  const startThreadOnWorktree = React.useCallback(
    async (wt: GitWorktree) => {
      try {
        refreshBranchAsync(wt.path);
        // Normalize trailing slashes when deciding whether this is the
        // main project vs. a secondary worktree. Git porcelain and the
        // file-picker disagree on trailing `/`, and a false negative
        // here leads to the main project being linked as a worktree of
        // itself (or a secondary worktree never getting linked at all,
        // which is why it would then appear as a separate top-level
        // project in the sidebar instead of grouped under its parent).
        const isMain = samePath(wt.path, projectPath);
        let wtProjectId =
          state.projects.find((p) => samePath(p.path, wt.path))?.projectId ??
          null;

        if (!wtProjectId) {
          const name = wt.branch ?? "(worktree)";
          // Pass worktreeOf so the parent link is dispatched atomically
          // with project_created — no "Untitled project" flash at the
          // top of the sidebar while the worktree metadata settles.
          wtProjectId = await createProject(
            wt.path,
            name,
            isMain
              ? undefined
              : { parentProjectId: projectId, branch: wt.branch },
          );
        } else if (
          !isMain &&
          wtProjectId !== projectId &&
          !state.projectWorktrees.has(wtProjectId)
        ) {
          // Existing project but no parent link (e.g. recovered after a
          // partial failure). Re-link it so it regroups under the main
          // project. Guarded against self-parenting.
          await linkProjectWorktree(wtProjectId, projectId, wt.branch);
        }

        await startThreadOnProject({
          projectId: wtProjectId,
          defaultProvider,
          defaultProviderLoaded,
          send,
          navigate: navigateToSession,
          notify,
        });
      } catch (err) {
        toast({
          title: "Failed to open new thread",
          description: String(err),
          duration: 4000,
        });
      }
    },
    [
      projectPath,
      projectId,
      state.projects,
      state.projectWorktrees,
      createProject,
      linkProjectWorktree,
      refreshBranchAsync,
      defaultProvider,
      defaultProviderLoaded,
      send,
      navigateToSession,
      notify,
    ],
  );

  // Eager-create on the main project (no worktree provisioning).
  function startThreadOnMain() {
    refreshBranchAsync(projectPath);
    void startThreadOnProject({
      projectId,
      defaultProvider,
      defaultProviderLoaded,
      send,
      navigate: navigateToSession,
      notify,
    });
  }

  // ── Render ─────────────────────────────────────────────────────

  return (
    <>
      <DropdownMenu open={open} onOpenChange={setOpen}>
        <DropdownMenuTrigger asChild>
          <button
            type="button"
            className="inline-flex h-6 w-6 shrink-0 items-center justify-center rounded-md text-muted-foreground opacity-0 transition-opacity hover:text-foreground group-hover/project:opacity-100"
            onClick={(e) => e.stopPropagation()}
          >
            <SquarePen className="h-3.5 w-3.5" />
          </button>
        </DropdownMenuTrigger>
        <DropdownMenuContent align="end" className="w-56">
          {worktreeQuery.isLoading && (
            <DropdownMenuLabel className="flex items-center gap-2 text-xs text-muted-foreground">
              <Loader2 className="h-3 w-3 animate-spin" />
              Loading worktrees...
            </DropdownMenuLabel>
          )}

          {/* Error loading worktrees — show error and let the user at
              least open a draft on the main project. The new-thread
              button must never be a dead end. */}
          {worktreeQuery.isError && (
            <>
              <DropdownMenuLabel className="text-xs text-destructive">
                {(worktreeQuery.error as Error).message}
              </DropdownMenuLabel>
              <DropdownMenuSeparator />
              <DropdownMenuItem
                onClick={() => {
                  setOpen(false);
                  startThreadOnMain();
                }}
              >
                <SquarePen className="mr-2 h-3.5 w-3.5" />
                New thread on main
              </DropdownMenuItem>
            </>
          )}

          {/* Loaded with no extra worktrees — single "new thread"
              entry plus the create-worktree affordance. */}
          {!worktreeQuery.isLoading &&
            !worktreeQuery.isError &&
            !hasMultipleWorktrees && (
              <>
                <DropdownMenuItem
                  onClick={() => {
                    setOpen(false);
                    startThreadOnMain();
                  }}
                >
                  <SquarePen className="mr-2 h-3.5 w-3.5" />
                  New thread
                </DropdownMenuItem>
                <DropdownMenuSeparator />
                <DropdownMenuItem
                  onClick={() => {
                    setOpen(false);
                    setCreateWtOpen(true);
                  }}
                >
                  <Plus className="mr-2 h-3.5 w-3.5" />
                  Create worktree...
                </DropdownMenuItem>
              </>
            )}

          {/* Multiple worktrees — single-level menu, one row per
              worktree. The branch that matches `projectPath` is
              labeled "main"; everything else shows its branch name. */}
          {!worktreeQuery.isLoading &&
            !worktreeQuery.isError &&
            hasMultipleWorktrees && (
              <>
                <DropdownMenuLabel className="text-xs text-muted-foreground">
                  Pick a worktree
                </DropdownMenuLabel>
                {worktrees.map((wt) => {
                  const isMain = samePath(wt.path, projectPath);
                  const label = wt.branch ?? "(detached)";
                  return (
                    <DropdownMenuItem
                      key={wt.path}
                      onClick={() => {
                        setOpen(false);
                        void startThreadOnWorktree(wt);
                      }}
                    >
                      <GitBranch className="mr-2 h-3.5 w-3.5 shrink-0 text-muted-foreground" />
                      <span className="flex-1 truncate">{label}</span>
                      {isMain && (
                        <span className="ml-1.5 rounded bg-muted px-1 py-0.5 text-[9px] font-normal uppercase tracking-wide text-muted-foreground">
                          main
                        </span>
                      )}
                    </DropdownMenuItem>
                  );
                })}
                <DropdownMenuSeparator />
                <DropdownMenuItem
                  onClick={() => {
                    setOpen(false);
                    setCreateWtOpen(true);
                  }}
                >
                  <Plus className="mr-2 h-3.5 w-3.5" />
                  Create worktree...
                </DropdownMenuItem>
              </>
            )}
        </DropdownMenuContent>
      </DropdownMenu>

      <CreateWorktreeDialog
        open={createWtOpen}
        onOpenChange={setCreateWtOpen}
        projectPath={projectPath}
        currentBranch={currentBranch}
        onCreated={(wt) => {
          // Newly-created worktree → eager-create a thread on it
          // using the user's default provider/model. The chat toolbar
          // still allows swapping provider mid-thread.
          void startThreadOnWorktree(wt);
        }}
      />
    </>
  );
}
