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
  DropdownMenuSub,
  DropdownMenuSubContent,
  DropdownMenuSubTrigger,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { useApp } from "@/stores/app-store";
import type { ProviderKind } from "@/lib/types";
import type { GitWorktree } from "@/lib/api";
import { readDefaultModel } from "@/lib/defaults-settings";
import { useProviderEnabled } from "@/hooks/use-provider-enabled";
import { useDefaultProvider } from "@/hooks/use-default-provider";
import {
  gitWorktreeListQueryOptions,
  gitBranchQueryOptions,
} from "@/lib/queries";
import { CreateWorktreeDialog } from "@/components/project/create-worktree-dialog";
import { samePath } from "@/lib/worktree-utils";
import { toast } from "@/hooks/use-toast";
import { ALL_PROVIDERS, PROVIDER_COLORS, statusBadge } from "./provider-constants";
import { ProviderDropdown } from "./provider-dropdown";
import { useSuppressSidebarDrag } from "./drag-suppression";

interface WorktreeAwareNewThreadProps {
  projectId: string;
  projectPath: string | undefined;
}

/**
 * Sidebar "new thread" button that, for projects with multiple git
 * worktrees, adds a worktree selection step before the provider
 * picker. For projects without worktrees (or with only the main one)
 * it falls through to the standard ProviderDropdown behavior.
 */
export function WorktreeAwareNewThread({
  projectId,
  projectPath,
}: WorktreeAwareNewThreadProps) {
  // If the project has no filesystem path we can't query worktrees —
  // fall back to the plain provider dropdown.
  if (!projectPath) {
    return <ProviderDropdown projectId={projectId} />;
  }

  return (
    <WorktreeDropdownInner
      projectId={projectId}
      projectPath={projectPath}
    />
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
  const { isProviderEnabled } = useProviderEnabled();
  const navigate = useNavigate();
  const queryClient = useQueryClient();

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

  // ── Provider readiness ────────────────────────────────────────
  const providerMap = new Map(state.providers.map((p) => [p.kind, p]));
  const stillLoading = !state.ready;

  const [defaultModels, setDefaultModels] = React.useState<
    Map<ProviderKind, string>
  >(new Map());

  React.useEffect(() => {
    let cancelled = false;
    const readyProviders = state.providers.filter(
      (p) => isProviderEnabled(p.kind) && p.status === "ready",
    );
    Promise.all(
      readyProviders.map(async (p) => {
        const model = await readDefaultModel(p.kind);
        return [p.kind, model] as const;
      }),
    ).then((entries) => {
      if (cancelled) return;
      const map = new Map<ProviderKind, string>();
      for (const [kind, model] of entries) {
        if (model) map.set(kind, model);
      }
      setDefaultModels(map);
    });
    return () => {
      cancelled = true;
    };
  }, [state.providers, isProviderEnabled]);

  // User's configured default provider (Settings → Defaults → Default
  // provider). Used when starting a thread on a freshly-created
  // worktree where we have no ambient session/provider to inherit.
  // Falls back to the first ready enabled provider, then to
  // `DEFAULT_PROVIDER` — see `useDefaultProvider` for the full chain.
  // `loaded` gates the create-worktree menu item so a fast click
  // during the async SQLite read can't silently fall back to a
  // non-preferred provider (see project-home-view for the same
  // pattern).
  const { defaultProvider, loaded: defaultProviderLoaded } =
    useDefaultProvider();

  // ── Thread creation (mirrors project-home-view startThreadOnWorktree) ──

  const startThreadOnWorktree = React.useCallback(
    async (wt: GitWorktree, provider: ProviderKind, model?: string) => {
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
            isMain ? undefined : { parentProjectId: projectId, branch: wt.branch },
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

        const res = await send({
          type: "start_session",
          provider,
          model,
          project_id: wtProjectId,
        });
        if (res?.type === "session_created") {
          navigate({
            to: "/chat/$sessionId",
            params: { sessionId: res.session.sessionId },
          });
        } else if (res?.type === "error") {
          toast({
            title: "Failed to start thread",
            description: res.message,
            duration: 4000,
          });
        }
      } catch (err) {
        toast({
          title: "Failed to start thread",
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
      send,
      navigate,
      refreshBranchAsync,
    ],
  );

  // Direct thread on the main project (no worktree provisioning).
  async function createThreadDirect(provider: ProviderKind, model?: string) {
    refreshBranchAsync(projectPath);
    const resolvedModel = model ?? defaultModels.get(provider);
    const res = await send({
      type: "start_session",
      provider,
      model: resolvedModel,
      project_id: projectId,
    });
    if (res && res.type === "session_created") {
      navigate({
        to: "/chat/$sessionId",
        params: { sessionId: res.session.sessionId },
      });
    }
  }

  // ── Provider items renderer (reused in both modes) ────────────

  function renderProviderItems(
    onPick: (provider: ProviderKind, model?: string) => void,
  ) {
    return (
      <>
        {stillLoading && (
          <>
            <DropdownMenuLabel className="flex items-center gap-2 text-xs text-muted-foreground">
              <Loader2 className="h-3 w-3 animate-spin" />
              Checking providers...
            </DropdownMenuLabel>
            <DropdownMenuSeparator />
          </>
        )}
        {ALL_PROVIDERS.map(({ kind, label }) => {
          const info = providerMap.get(kind);
          if (!isProviderEnabled(kind)) return null;
          const isReady = info?.status === "ready";
          const hasModels = info && info.models.length > 0;

          if (hasModels && isReady) {
            return (
              <DropdownMenuSub key={kind}>
                <DropdownMenuSubTrigger>
                  <span
                    className={`mr-2 inline-block h-2 w-2 shrink-0 rounded-full ${PROVIDER_COLORS[kind]}`}
                  />
                  New {label} thread
                </DropdownMenuSubTrigger>
                <DropdownMenuSubContent>
                  {info.models.map((m) => (
                    <DropdownMenuItem
                      key={m.value}
                      onClick={() => onPick(kind, m.value)}
                    >
                      {m.label}
                    </DropdownMenuItem>
                  ))}
                </DropdownMenuSubContent>
              </DropdownMenuSub>
            );
          }

          return (
            <DropdownMenuItem
              key={kind}
              disabled={!isReady}
              onClick={() => isReady && onPick(kind)}
            >
              <span
                className={`mr-2 inline-block h-2 w-2 shrink-0 rounded-full ${isReady ? PROVIDER_COLORS[kind] : "bg-muted-foreground/30"}`}
              />
              New {label} thread
              {statusBadge(info)}
            </DropdownMenuItem>
          );
        })}
      </>
    );
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
        <DropdownMenuContent align="end" className="w-64">
          {/* Loading worktrees */}
          {worktreeQuery.isLoading && (
            <DropdownMenuLabel className="flex items-center gap-2 text-xs text-muted-foreground">
              <Loader2 className="h-3 w-3 animate-spin" />
              Loading worktrees...
            </DropdownMenuLabel>
          )}

          {/* Error loading worktrees — show error + fallback to direct provider items */}
          {worktreeQuery.isError && (
            <>
              <DropdownMenuLabel className="text-xs text-destructive">
                {(worktreeQuery.error as Error).message}
              </DropdownMenuLabel>
              <DropdownMenuSeparator />
              {renderProviderItems((provider, model) => {
                const resolvedModel = model ?? defaultModels.get(provider);
                void createThreadDirect(provider, resolvedModel);
              })}
            </>
          )}

          {/* Loaded but no multiple worktrees — direct provider list + create worktree */}
          {!worktreeQuery.isLoading &&
            !worktreeQuery.isError &&
            !hasMultipleWorktrees && (
              <>
                {renderProviderItems((provider, model) => {
                  const resolvedModel = model ?? defaultModels.get(provider);
                  void createThreadDirect(provider, resolvedModel);
                })}
                <DropdownMenuSeparator />
                <DropdownMenuItem
                  disabled={!defaultProviderLoaded}
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

          {/* Loaded with multiple worktrees — two-level menu */}
          {!worktreeQuery.isLoading &&
            !worktreeQuery.isError &&
            hasMultipleWorktrees && (
              <>
                <DropdownMenuLabel className="text-xs text-muted-foreground">
                  Pick a worktree
                </DropdownMenuLabel>
                {worktrees.map((wt) => {
                  const isMain = wt.path === projectPath;
                  const label = wt.branch ?? "(detached)";
                  return (
                    <DropdownMenuSub key={wt.path}>
                      <DropdownMenuSubTrigger>
                        <GitBranch className="mr-2 h-3.5 w-3.5 shrink-0 text-muted-foreground" />
                        <span className="flex-1 truncate">{label}</span>
                        {isMain && (
                          <span className="ml-1.5 rounded bg-muted px-1 py-0.5 text-[9px] font-normal uppercase tracking-wide text-muted-foreground">
                            main
                          </span>
                        )}
                      </DropdownMenuSubTrigger>
                      <DropdownMenuSubContent className="w-56">
                        {renderProviderItems((provider, model) => {
                          const resolvedModel =
                            model ?? defaultModels.get(provider);
                          setOpen(false);
                          void startThreadOnWorktree(
                            wt,
                            provider,
                            resolvedModel,
                          );
                        })}
                      </DropdownMenuSubContent>
                    </DropdownMenuSub>
                  );
                })}
                <DropdownMenuSeparator />
                <DropdownMenuItem
                  disabled={!defaultProviderLoaded}
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
          // Respect the user's configured default provider (with
          // ready/enabled fallbacks handled by the `defaultProvider`
          // memo above).
          const model = defaultModels.get(defaultProvider);
          void startThreadOnWorktree(wt, defaultProvider, model);
        }}
      />
    </>
  );
}
