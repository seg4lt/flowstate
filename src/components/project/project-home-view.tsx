import * as React from "react";
import { useNavigate } from "@tanstack/react-router";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import {
  Check,
  ChevronDown,
  Diff as DiffIcon,
  FolderOpen,
  GitBranch,
  Loader2,
  MessageSquarePlus,
  Plus,
  Search,
  Trash2,
} from "lucide-react";

import { SidebarTrigger } from "@/components/ui/sidebar";
import { useApp } from "@/stores/app-store";
import { BranchSwitcher } from "@/components/chat/branch-switcher";
import {
  DiffPanel,
  type DiffStyle,
} from "@/components/chat/diff-panel";
import { ProviderDropdown } from "@/components/sidebar/provider-dropdown";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import {
  gitBranchQueryOptions,
  gitWorktreeListQueryOptions,
  prefetchProjectFiles,
} from "@/lib/queries";
import { useStreamedGitDiffSummary } from "@/lib/git-diff-stream";
import {
  openInEditor,
  removeGitWorktree,
  type GitWorktree,
} from "@/lib/api";
import { toast } from "@/hooks/use-toast";
import { useProviderEnabled } from "@/hooks/use-provider-enabled";
import { CreateWorktreeDialog } from "@/components/project/create-worktree-dialog";
import type {
  AggregatedFileDiff,
} from "@/lib/session-diff";
import type { ProviderKind, SessionSummary } from "@/lib/types";

interface EditorChoice {
  id: string;
  label: string;
  command: string;
}

// Kept in sync with `header-actions.tsx` — duplicated rather than
// lifted out because the list is three lines and extracting it to a
// shared module would only add import noise.
const KNOWN_EDITORS: EditorChoice[] = [
  { id: "zed", label: "Zed", command: "zed" },
  { id: "vscode", label: "VS Code", command: "code" },
  { id: "idea", label: "IntelliJ IDEA", command: "idea" },
];

const DEFAULT_EDITOR_KEY = "flowstate:default-editor";

function loadDefaultEditorId(): string | null {
  try {
    return window.localStorage.getItem(DEFAULT_EDITOR_KEY);
  } catch {
    return null;
  }
}

function saveDefaultEditorId(id: string): void {
  try {
    window.localStorage.setItem(DEFAULT_EDITOR_KEY, id);
  } catch {
    /* storage may be unavailable */
  }
}

interface ProjectHomeViewProps {
  projectId: string;
}

export function ProjectHomeView({ projectId }: ProjectHomeViewProps) {
  const { state, dispatch, send, createProject, linkProjectWorktree } =
    useApp();
  const { isProviderEnabled } = useProviderEnabled();
  const navigate = useNavigate();
  const queryClient = useQueryClient();

  const project = state.projects.find((p) => p.projectId === projectId);
  const displayName =
    state.projectDisplay.get(projectId)?.name ?? "Untitled project";
  const projectPath = project?.path ?? null;

  // Clicking a project row is an explicit exit from any open thread —
  // mirror the sidebar highlight by clearing the active session so the
  // sidebar's ThreadItem `isActive` state doesn't lag behind the route.
  React.useEffect(() => {
    dispatch({ type: "set_active_session", sessionId: null });
  }, [dispatch, projectId]);

  const branchQuery = useQuery(gitBranchQueryOptions(projectPath));
  const currentBranch = branchQuery.data ?? "";

  const worktreeQuery = useQuery(gitWorktreeListQueryOptions(projectPath));
  const worktrees = React.useMemo<GitWorktree[]>(
    () => worktreeQuery.data ?? [],
    [worktreeQuery.data],
  );

  // BranchSwitcher starts a new session when the user creates or
  // opens a worktree. From the project page we have no ambient
  // session to inherit from, so pick the first ready provider as
  // the default. Worktree-only operations (create branch, checkout)
  // never reach this code path, so a stale fallback here only
  // affects the "open worktree as thread" flow, which will surface
  // its own toast if the provider can't actually start a session.
  const defaultProvider: ProviderKind = React.useMemo(() => {
    const ready = state.providers.find(
      (p) => isProviderEnabled(p.kind) && p.status === "ready",
    );
    if (ready) return ready.kind;
    const any = state.providers.find((p) => isProviderEnabled(p.kind));
    if (any) return any.kind;
    return "claude";
  }, [state.providers, isProviderEnabled]);

  // Threads grouped by the SDK project's filesystem path. Used to
  // render the per-worktree thread chips — each worktree owns its
  // own SDK project whose `path` matches the worktree folder, so
  // looking sessions up by path gives us the right bucket for both
  // the main worktree and linked ones.
  const sessionsByPath = React.useMemo(() => {
    const byProjectPath = new Map<string, SessionSummary[]>();
    for (const session of state.sessions.values()) {
      if (!session.projectId) continue;
      const sdkProject = state.projects.find(
        (p) => p.projectId === session.projectId,
      );
      if (!sdkProject?.path) continue;
      const list = byProjectPath.get(sdkProject.path) ?? [];
      list.push(session);
      byProjectPath.set(sdkProject.path, list);
    }
    for (const list of byProjectPath.values()) {
      list.sort(
        (a, b) =>
          new Date(b.updatedAt).getTime() - new Date(a.updatedAt).getTime(),
      );
    }
    return byProjectPath;
  }, [state.sessions, state.projects]);

  // Open-in-editor — same logic as HeaderActions. Duplicated inline
  // so we don't have to thread sessionId-dependent props through a
  // shared component; the chat-view header still owns its own copy.
  const [defaultEditorId, setDefaultEditorId] = React.useState<string | null>(
    () => loadDefaultEditorId(),
  );
  const defaultEditor = React.useMemo<EditorChoice | null>(() => {
    if (!defaultEditorId) return null;
    return KNOWN_EDITORS.find((e) => e.id === defaultEditorId) ?? null;
  }, [defaultEditorId]);

  const launchEditor = React.useCallback(
    async (editor: EditorChoice, pathOverride?: string) => {
      const targetPath = pathOverride ?? projectPath;
      if (!targetPath) {
        toast({
          description: "This project has no path to open.",
          duration: 3000,
        });
        return;
      }
      try {
        await openInEditor(editor.command, targetPath);
      } catch (err) {
        toast({
          description: `Could not launch ${editor.label}: ${String(err)}`,
          duration: 4000,
        });
      }
    },
    [projectPath],
  );

  const handlePickEditor = React.useCallback(
    (editor: EditorChoice) => {
      setDefaultEditorId(editor.id);
      saveDefaultEditorId(editor.id);
      void launchEditor(editor);
    },
    [launchEditor],
  );

  // Per-worktree busy tracking. One at a time is fine — this isn't
  // a bulk action surface, and keying by path lets us show the
  // spinner on the exact row the user clicked.
  const [openingWtPath, setOpeningWtPath] = React.useState<string | null>(
    null,
  );
  const [removingWtPath, setRemovingWtPath] = React.useState<string | null>(
    null,
  );
  const [failedRemovalPath, setFailedRemovalPath] = React.useState<
    string | null
  >(null);

  // Create-worktree dialog visibility.
  const [createWtOpen, setCreateWtOpen] = React.useState(false);

  // Diff dialog target — null when closed, the selected worktree
  // when the user clicks a row's diff button. The dialog body
  // reuses <DiffPanel> unchanged, so it benefits from the same
  // lazy-mount + per-file caching the chat view relies on.
  const [diffFor, setDiffFor] = React.useState<GitWorktree | null>(null);

  // Route the search click to the most recent session on each
  // worktree. When a session exists we use /code/$sessionId;
  // otherwise we fall back to /browse?path=... which opens the
  // file browser directly without a session context.
  const firstSessionPathFor = React.useCallback(
    (wtPath: string): string | null => {
      const list = sessionsByPath.get(wtPath);
      return list && list.length > 0 ? list[0].sessionId : null;
    },
    [sessionsByPath],
  );

  const handleSearchForWorktree = React.useCallback(
    (wt: GitWorktree) => {
      prefetchProjectFiles(queryClient, wt.path);
      const sid = firstSessionPathFor(wt.path);
      if (sid) {
        navigate({ to: "/code/$sessionId", params: { sessionId: sid } });
      } else {
        navigate({ to: "/browse", search: { path: wt.path } });
      }
    },
    [firstSessionPathFor, navigate, queryClient],
  );

  // Start a new thread rooted in this worktree. Mirrors the
  // find-or-create flow in branch-switcher.tsx's openWorktreeSession:
  //   * Main worktree (wt.path === parent projectPath) → reuse the
  //     parent SDK project id directly.
  //   * Linked worktree already linked → reuse the existing SDK
  //     project id.
  //   * Linked worktree without an SDK project yet → create one with
  //     the branch as its display name, link it to the parent, then
  //     start a session under it. Future opens hit the reuse path.
  // The send() below goes through the app-store wrapper so the
  // resulting session_created lands in state.sessions before the
  // navigate fires.
  const startThreadOnWorktree = React.useCallback(
    async (wt: GitWorktree, provider: ProviderKind, model?: string) => {
      if (!projectPath) return;
      setOpeningWtPath(wt.path);
      try {
        const isMain = wt.path === projectPath;
        let wtProjectId =
          state.projects.find((p) => p.path === wt.path)?.projectId ?? null;
        if (!wtProjectId) {
          const name = wt.branch ?? "(worktree)";
          wtProjectId = await createProject(wt.path, name);
          if (!isMain) {
            await linkProjectWorktree(wtProjectId, projectId, wt.branch);
          }
        } else if (!isMain && !state.projectWorktrees.has(wtProjectId)) {
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
      } finally {
        setOpeningWtPath(null);
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
    ],
  );

  const removeWorktreeImpl = React.useCallback(
    async (wt: GitWorktree, force: boolean) => {
      if (!projectPath) return;
      setRemovingWtPath(wt.path);
      try {
        await removeGitWorktree(projectPath, wt.path, force);
        queryClient.invalidateQueries({
          queryKey: ["git", "worktree-list", projectPath],
        });
        queryClient.invalidateQueries({
          queryKey: ["git", "branch-list", projectPath],
        });
        setFailedRemovalPath(null);
        toast({
          title: "Worktree removed",
          description: wt.branch ?? wt.path,
          duration: 2500,
        });
      } catch (err) {
        const msg = err instanceof Error ? err.message : String(err);
        setFailedRemovalPath(wt.path);
        toast({
          title: `Failed to remove worktree ${wt.branch ?? ""}`.trim(),
          description: msg,
          duration: 6000,
        });
      } finally {
        setRemovingWtPath(null);
      }
    },
    [projectPath, queryClient],
  );

  const removeWorktree = React.useCallback(
    (wt: GitWorktree) => {
      if (!projectPath) return;
      if (wt.path === projectPath) return;
      const ok = window.confirm(
        `Remove worktree ${wt.branch ?? wt.path}?\n\n${wt.path}`,
      );
      if (!ok) return;
      void removeWorktreeImpl(wt, false);
    },
    [projectPath, removeWorktreeImpl],
  );

  const forceRemoveWorktree = React.useCallback(
    (wt: GitWorktree) => {
      const ok = window.confirm(
        `Force-remove worktree ${wt.branch ?? wt.path}?\n\nThis will discard any uncommitted changes inside it.`,
      );
      if (!ok) return;
      void removeWorktreeImpl(wt, true);
    },
    [removeWorktreeImpl],
  );

  if (!project || !projectPath) {
    return (
      <div className="flex h-svh flex-col">
        <header className="flex h-12 shrink-0 items-center gap-2 border-b border-border px-2 text-sm text-muted-foreground">
          <SidebarTrigger />
          <span>Project not found</span>
        </header>
        <div className="flex flex-1 items-center justify-center p-8 text-sm text-muted-foreground">
          This project may have been removed. Pick another from the sidebar.
        </div>
      </div>
    );
  }

  return (
    <div className="flex h-svh min-w-0 flex-col overflow-hidden">
      <header className="flex h-12 shrink-0 items-center gap-2 border-b border-border px-2 text-sm">
        <SidebarTrigger />
        <div className="flex min-w-0 flex-col leading-tight">
          <span className="truncate font-medium">{displayName}</span>
          <div className="flex items-center gap-2">
            <BranchSwitcher
              projectPath={projectPath}
              currentBranch={currentBranch || "HEAD"}
              parentProjectId={projectId}
              parentProjectPath={projectPath}
              provider={defaultProvider}
              model={null}
              onCheckedOut={() => {
                void branchQuery.refetch();
                void worktreeQuery.refetch();
              }}
            />
          </div>
        </div>
        <div className="ml-auto flex items-center gap-1">
          <ProviderDropdown
            projectId={projectId}
            trigger={
              <button
                type="button"
                className="inline-flex h-6 shrink-0 items-center gap-1 rounded-[min(var(--radius-md),10px)] border border-border bg-background px-2 text-xs font-medium hover:bg-muted hover:text-foreground dark:border-input dark:bg-input/30 dark:hover:bg-input/50"
              >
                <MessageSquarePlus className="h-3 w-3" />
                New thread
              </button>
            }
          />
          <DropdownMenu>
            <DropdownMenuTrigger asChild>
              <button
                type="button"
                title={
                  defaultEditor
                    ? `Open project in ${defaultEditor.label}`
                    : "Pick an editor to open the project in"
                }
                className="inline-flex h-6 shrink-0 items-center gap-1 rounded-[min(var(--radius-md),10px)] border border-border bg-background px-2 text-xs font-medium hover:bg-muted hover:text-foreground dark:border-input dark:bg-input/30 dark:hover:bg-input/50"
              >
                <FolderOpen className="h-3 w-3" />
                Open
                <ChevronDown className="h-3 w-3" />
              </button>
            </DropdownMenuTrigger>
            <DropdownMenuContent align="end" className="min-w-[160px]">
              {KNOWN_EDITORS.map((editor) => {
                const isDefault = defaultEditorId === editor.id;
                return (
                  <DropdownMenuItem
                    key={editor.id}
                    onClick={() => handlePickEditor(editor)}
                    className="flex items-center justify-between gap-2"
                  >
                    <span>{editor.label}</span>
                    {isDefault && (
                      <Check className="h-3 w-3 text-muted-foreground" />
                    )}
                  </DropdownMenuItem>
                );
              })}
            </DropdownMenuContent>
          </DropdownMenu>
        </div>
      </header>

      {/* Horizontal split: worktree list on the left, optional diff
          side panel on the right. Mirrors chat-view's layout so the
          diff affordance feels consistent across the two views. */}
      <div className="flex min-h-0 min-w-0 flex-1">
        <div className="min-w-0 flex-1 overflow-auto">
          <div className="mx-auto max-w-3xl p-6">
            <div className="mb-3 flex items-center justify-between gap-2">
              <h2 className="text-sm font-medium">Worktrees</h2>
              <div className="flex items-center gap-2">
                <span className="text-[11px] text-muted-foreground">
                  {worktreeQuery.isLoading
                    ? "Loading…"
                    : `${worktrees.length} ${worktrees.length === 1 ? "worktree" : "worktrees"}`}
                </span>
                <button
                  type="button"
                  onClick={() => setCreateWtOpen(true)}
                  className="inline-flex h-6 shrink-0 items-center gap-1 rounded-[min(var(--radius-md),10px)] border border-border bg-background px-2 text-xs font-medium hover:bg-muted hover:text-foreground dark:border-input dark:bg-input/30 dark:hover:bg-input/50"
                >
                  <Plus className="h-3 w-3" />
                  Create worktree
                </button>
              </div>
          </div>

          {worktreeQuery.isError ? (
            <div className="rounded-md border border-destructive/30 bg-destructive/5 p-3 text-[11px] text-destructive">
              {(worktreeQuery.error as Error).message}
            </div>
          ) : worktrees.length === 0 && !worktreeQuery.isLoading ? (
            <div className="rounded-md border border-dashed border-border p-6 text-center text-xs text-muted-foreground">
              No worktrees found.
            </div>
          ) : (
            <ul className="space-y-2">
              {worktrees.map((wt) => {
                const isMain = wt.path === projectPath;
                const label = wt.branch ?? "(detached)";
                const shortSha = wt.head ? wt.head.slice(0, 7) : "";
                const isOpening = openingWtPath === wt.path;
                const isRemoving = removingWtPath === wt.path;
                const failed = failedRemovalPath === wt.path;
                return (
                  <li
                    key={wt.path}
                    className="rounded-md border border-border bg-background"
                  >
                    <div className="flex items-center gap-2 px-3 py-2">
                      <GitBranch className="h-3.5 w-3.5 shrink-0 text-muted-foreground" />
                      <div className="flex min-w-0 flex-1 flex-col gap-0.5">
                        <span className="truncate text-xs font-medium">
                          {label}
                          {isMain && (
                            <span className="ml-1.5 rounded bg-muted px-1 py-0.5 text-[9px] font-normal uppercase tracking-wide text-muted-foreground">
                              main
                            </span>
                          )}
                        </span>
                        <span className="truncate text-[11px] text-muted-foreground">
                          {shortSha && (
                            <>
                              <span className="font-mono">{shortSha}</span>
                              <span className="mx-1 opacity-60">•</span>
                            </>
                          )}
                          {wt.path}
                        </span>
                      </div>
                      <button
                        type="button"
                        aria-label={`Show diff for ${label}`}
                        title="Show working-tree diff"
                        onClick={() => setDiffFor(wt)}
                        className="inline-flex h-6 w-6 shrink-0 items-center justify-center rounded-md text-muted-foreground outline-none hover:bg-muted hover:text-foreground"
                      >
                        <DiffIcon className="h-3.5 w-3.5" />
                      </button>
                      <button
                        type="button"
                        aria-label={`Search files in ${label}`}
                        title="Search files"
                        onMouseEnter={() =>
                          prefetchProjectFiles(queryClient, wt.path)
                        }
                        onFocus={() =>
                          prefetchProjectFiles(queryClient, wt.path)
                        }
                        onClick={() => handleSearchForWorktree(wt)}
                        className="inline-flex h-6 w-6 shrink-0 items-center justify-center rounded-md text-muted-foreground outline-none hover:bg-muted hover:text-foreground"
                      >
                        <Search className="h-3.5 w-3.5" />
                      </button>
                      <DropdownMenu>
                        <DropdownMenuTrigger asChild>
                          <button
                            type="button"
                            aria-label={`Open ${label} in editor`}
                            title={
                              defaultEditor
                                ? `Open in ${defaultEditor.label}`
                                : "Pick an editor"
                            }
                            className="inline-flex h-6 w-6 shrink-0 items-center justify-center rounded-md text-muted-foreground outline-none hover:bg-muted hover:text-foreground"
                          >
                            <FolderOpen className="h-3.5 w-3.5" />
                          </button>
                        </DropdownMenuTrigger>
                        <DropdownMenuContent
                          align="end"
                          className="min-w-[160px]"
                        >
                          {KNOWN_EDITORS.map((editor) => {
                            const isDefault = defaultEditorId === editor.id;
                            return (
                              <DropdownMenuItem
                                key={editor.id}
                                onClick={() => {
                                  setDefaultEditorId(editor.id);
                                  saveDefaultEditorId(editor.id);
                                  void launchEditor(editor, wt.path);
                                }}
                                className="flex items-center justify-between gap-2"
                              >
                                <span>{editor.label}</span>
                                {isDefault && (
                                  <Check className="h-3 w-3 text-muted-foreground" />
                                )}
                              </DropdownMenuItem>
                            );
                          })}
                        </DropdownMenuContent>
                      </DropdownMenu>
                      <ProviderDropdown
                        onSelect={(provider, model) =>
                          void startThreadOnWorktree(wt, provider, model)
                        }
                        trigger={
                          <button
                            type="button"
                            disabled={isOpening || isRemoving}
                            title={`Start a new thread in ${label}`}
                            className="inline-flex h-6 shrink-0 items-center gap-1 rounded-[min(var(--radius-md),10px)] border border-border bg-background px-2 text-xs font-medium outline-none hover:bg-muted hover:text-foreground disabled:opacity-50 dark:border-input dark:bg-input/30 dark:hover:bg-input/50"
                          >
                            {isOpening ? (
                              <Loader2 className="h-3 w-3 animate-spin" />
                            ) : (
                              <MessageSquarePlus className="h-3 w-3" />
                            )}
                            New thread
                          </button>
                        }
                      />
                      <button
                        type="button"
                        aria-label={
                          isMain
                            ? "Main worktree cannot be removed"
                            : `Remove worktree ${label}`
                        }
                        title={
                          isMain
                            ? "Main worktree cannot be removed"
                            : `Remove worktree ${label}`
                        }
                        disabled={isMain || isOpening || isRemoving}
                        onClick={() => removeWorktree(wt)}
                        className="inline-flex h-6 w-6 shrink-0 items-center justify-center rounded-md text-muted-foreground outline-none hover:bg-destructive/10 hover:text-destructive disabled:cursor-not-allowed disabled:opacity-40 disabled:hover:bg-transparent disabled:hover:text-muted-foreground"
                      >
                        {isRemoving ? (
                          <Loader2 className="h-3 w-3 animate-spin" />
                        ) : (
                          <Trash2 className="h-3.5 w-3.5" />
                        )}
                      </button>
                    </div>
                    {failed && !isMain && (
                      <div className="border-t border-destructive/30 px-3 py-2 text-[11px] text-destructive">
                        Removal failed.{" "}
                        <button
                          type="button"
                          onClick={() => forceRemoveWorktree(wt)}
                          className="underline underline-offset-2 hover:text-destructive/80"
                        >
                          Force delete
                        </button>
                      </div>
                    )}
                  </li>
                );
              })}
            </ul>
          )}
          </div>
        </div>

        {diffFor && (
          <aside
            className="flex w-[min(620px,55vw)] shrink-0 flex-col border-l border-border bg-background"
          >
            <WorktreeDiffBody
              key={diffFor.path}
              worktreePath={diffFor.path}
              onClose={() => setDiffFor(null)}
            />
          </aside>
        )}
      </div>

      <CreateWorktreeDialog
        open={createWtOpen}
        onOpenChange={setCreateWtOpen}
        projectPath={projectPath}
        currentBranch={currentBranch}
        onCreated={(wt) => {
          void worktreeQuery.refetch();
          void startThreadOnWorktree(wt, defaultProvider);
        }}
      />
    </div>
  );
}

// Isolates the diff query + local `refreshTick` inside the side
// panel so it only runs while the panel is actually open. Mounted
// with `key={diffFor.path}` so switching targets resets the query
// scope cleanly instead of leaking stale per-file cache entries
// from the previous worktree into the new one.
function WorktreeDiffBody({
  worktreePath,
  onClose,
}: {
  worktreePath: string;
  onClose: () => void;
}) {
  const [refreshTick, setRefreshTick] = React.useState(0);
  const diffStream = useStreamedGitDiffSummary(worktreePath, refreshTick, true);
  const diffs: AggregatedFileDiff[] = React.useMemo(
    () => diffStream.diffs,
    [diffStream.diffs],
  );
  const [style, setStyle] = React.useState<DiffStyle>("split");

  // Refresh on window focus so edits made from the terminal or an
  // external editor appear without a manual reload. Matches the
  // refresh-on-focus behavior the project page's header already
  // assumed for the working-tree diff.
  React.useEffect(() => {
    function onFocus() {
      setRefreshTick((t) => t + 1);
    }
    window.addEventListener("focus", onFocus);
    return () => window.removeEventListener("focus", onFocus);
  }, []);

  return (
    <DiffPanel
      projectPath={worktreePath}
      diffs={diffs}
      refreshKey={refreshTick}
      streamStatus={diffStream.status}
      style={style}
      onStyleChange={setStyle}
      onClose={onClose}
      isFullscreen={false}
      onToggleFullscreen={() => setRefreshTick((t) => t + 1)}
    />
  );
}
