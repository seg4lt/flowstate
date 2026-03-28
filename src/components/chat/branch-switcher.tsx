import * as React from "react";
import {
  useMutation,
  useQuery,
  useQueryClient,
} from "@tanstack/react-query";
import { useNavigate } from "@tanstack/react-router";
import { GitBranch, Loader2, Plus, Trash2 } from "lucide-react";

import {
  Popover,
  PopoverContent,
  PopoverTrigger,
} from "@/components/ui/popover";
import {
  Command,
  CommandEmpty,
  CommandGroup,
  CommandInput,
  CommandItem,
  CommandList,
} from "@/components/ui/command";
import { cn } from "@/lib/utils";
import {
  createGitWorktree,
  gitCheckout,
  gitCreateBranch,
  removeGitWorktree,
  type GitWorktree,
} from "@/lib/api";
import {
  gitBranchListQueryOptions,
  gitWorktreeListQueryOptions,
} from "@/lib/queries";
import { toast } from "@/hooks/use-toast";
import { useApp } from "@/stores/app-store";
import { readWorktreeBasePath } from "@/lib/worktree-settings";
import type { ProviderKind } from "@/lib/types";

interface BranchSwitcherProps {
  /** Path of the current session's SDK project — terminal/diff/git
   *  branch queries run against this. For a worktree thread this is
   *  the worktree folder, for a main thread it's the repo root. */
  projectPath: string;
  currentBranch: string;
  /** SDK project_id of the main repo — the parent under which all
   *  worktree threads live. Equal to the current session's project_id
   *  when the current session is itself on the main worktree. */
  parentProjectId: string;
  /** Filesystem path of the main repo. Used to derive new worktree
   *  folder paths via the `<parent>/worktrees/...` convention and as
   *  the working directory for `git worktree add` / `remove`. */
  parentProjectPath: string;
  /** Provider + model inherited by any worktree thread we auto-create
   *  so the new thread uses the same backend as the current one. */
  provider: ProviderKind;
  model: string | null;
  onCheckedOut: () => void;
  /** Which tab opens first. Defaults to "branches" to match the
   *  chat-header usage. The project-home page passes "worktrees"
   *  for its Worktrees action button. */
  initialTab?: Tab;
  /** Override the default popover trigger. Used by the project-home
   *  page to render the button as a sized action card instead of the
   *  tiny inline branch label the chat header shows. */
  trigger?: React.ReactElement;
}

type Tab = "branches" | "worktrees";

export function BranchSwitcher({
  projectPath,
  currentBranch,
  parentProjectId,
  parentProjectPath,
  provider,
  model,
  onCheckedOut,
  initialTab = "branches",
  trigger,
}: BranchSwitcherProps) {
  const [open, setOpen] = React.useState(false);
  const [tab, setTab] = React.useState<Tab>(initialTab);
  const [checkoutError, setCheckoutError] = React.useState<string | null>(null);
  const [pendingBranch, setPendingBranch] = React.useState<string | null>(null);

  // Worktree-side state. Separate from branch state so errors don't
  // cross-contaminate and the force-delete retry can remember which
  // worktree to retry against.
  const [worktreeError, setWorktreeError] = React.useState<string | null>(
    null,
  );
  const [pendingWorktreePath, setPendingWorktreePath] = React.useState<
    string | null
  >(null);
  const [failedRemoval, setFailedRemoval] = React.useState<string | null>(null);

  const queryClient = useQueryClient();
  const navigate = useNavigate();
  const { state, send, createProject, linkProjectWorktree } = useApp();

  const branchListQuery = useQuery({
    ...gitBranchListQueryOptions(projectPath),
    enabled: open && tab === "branches",
  });
  const worktreeListQuery = useQuery({
    ...gitWorktreeListQueryOptions(projectPath),
    enabled: open && tab === "worktrees",
  });

  const invalidateAfterBranchChange = React.useCallback(() => {
    queryClient.invalidateQueries({
      queryKey: ["git", "branch", projectPath],
    });
    queryClient.invalidateQueries({
      queryKey: ["git", "branch-list", projectPath],
    });
    onCheckedOut();
    setOpen(false);
  }, [onCheckedOut, projectPath, queryClient]);

  const checkoutMutation = useMutation({
    mutationFn: async (args: {
      branch: string;
      createTrack: string | null;
    }) => {
      setPendingBranch(args.branch);
      await gitCheckout(projectPath, args.branch, args.createTrack);
    },
    onSuccess: () => {
      setPendingBranch(null);
      setCheckoutError(null);
      invalidateAfterBranchChange();
    },
    onError: (err, vars) => {
      setPendingBranch(null);
      const msg = err instanceof Error ? err.message : String(err);
      setCheckoutError(msg);
      toast({
        title: `Failed to checkout ${vars.branch}`,
        description: msg,
        duration: 6000,
      });
    },
  });

  const createMutation = useMutation({
    mutationFn: async (name: string) => {
      setPendingBranch(name);
      await gitCreateBranch(projectPath, name);
    },
    onSuccess: (_data, name) => {
      setPendingBranch(null);
      setCheckoutError(null);
      invalidateAfterBranchChange();
      toast({
        title: `Created branch ${name}`,
        description: `Based off ${currentBranch}`,
        duration: 2500,
      });
    },
    onError: (err, name) => {
      setPendingBranch(null);
      const msg = err instanceof Error ? err.message : String(err);
      setCheckoutError(msg);
      toast({
        title: `Failed to create branch ${name}`,
        description: msg,
        duration: 6000,
      });
    },
  });

  React.useEffect(() => {
    if (open) {
      setCheckoutError(null);
      setWorktreeError(null);
      setFailedRemoval(null);
      setTab(initialTab);
    }
  }, [open, initialTab]);

  // Find-or-create flow for opening a worktree as a flowzen thread.
  // Each worktree has its own SDK project (so the agent SDK's existing
  // cwd resolution picks up the worktree folder without any SDK-level
  // worktree awareness). If a thread already exists under that SDK
  // project we focus it; otherwise we start a new session and link it
  // to the parent project via the flowzen-side `project_worktree`
  // table. The parent link is what the sidebar reads to visually
  // group worktree threads under the main repo's project header.
  const openWorktreeSession = React.useCallback(
    async (wt: GitWorktree) => {
      // 1. Find the SDK project whose path matches this worktree (or
      //    create it if we haven't linked it yet). The main repo is
      //    already its own SDK project — clicking "main" in the
      //    worktree list resolves to the parent project_id directly
      //    and no new project is created.
      let wtProjectId =
        state.projects.find((p) => p.path === wt.path)?.projectId ?? null;

      const isParent = wtProjectId === parentProjectId;
      if (!wtProjectId) {
        const displayName = wt.branch ?? "(worktree)";
        wtProjectId = await createProject(wt.path, displayName);
        await linkProjectWorktree(wtProjectId, parentProjectId, wt.branch);
      } else if (!isParent && !state.projectWorktrees.has(wtProjectId)) {
        // SDK project existed but wasn't linked yet (e.g. orphan from
        // a previous run). Link it now so the sidebar groups it.
        await linkProjectWorktree(wtProjectId, parentProjectId, wt.branch);
      }

      // 2. Find an existing session for this SDK project, or start one.
      const existing = Array.from(state.sessions.values()).find(
        (s) => s.projectId === wtProjectId,
      );
      if (existing) {
        navigate({
          to: "/chat/$sessionId",
          params: { sessionId: existing.sessionId },
        });
        toast({
          title: "Worktree already open",
          description: wt.branch ?? wt.path,
          duration: 2500,
        });
        setOpen(false);
        return;
      }

      const res = await send({
        type: "start_session",
        provider,
        model: model ?? undefined,
        project_id: wtProjectId,
      });
      if (res?.type === "session_created") {
        navigate({
          to: "/chat/$sessionId",
          params: { sessionId: res.session.sessionId },
        });
        toast({
          title: `Opened worktree ${wt.branch ?? "(detached)"}`,
          duration: 2500,
        });
      }
      setOpen(false);
    },
    [
      state.projects,
      state.projectWorktrees,
      state.sessions,
      parentProjectId,
      provider,
      model,
      createProject,
      linkProjectWorktree,
      send,
      navigate,
    ],
  );

  const createWorktreeMutation = useMutation({
    mutationFn: async (typedName: string) => {
      setPendingBranch(typedName);
      // Read the user-overridable base path each time — cheap
      // sqlite lookup, and keeping it off state means the setting
      // takes effect immediately without a popover remount.
      const configuredBase = await readWorktreeBasePath();
      const wtPath = deriveWorktreePath(
        parentProjectPath,
        typedName,
        configuredBase,
      );
      const wt = await createGitWorktree(
        parentProjectPath,
        wtPath,
        typedName,
        currentBranch,
      );
      return wt;
    },
    onSuccess: async (wt, name) => {
      setPendingBranch(null);
      setCheckoutError(null);
      queryClient.invalidateQueries({
        queryKey: ["git", "worktree-list", projectPath],
      });
      queryClient.invalidateQueries({
        queryKey: ["git", "worktree-list", parentProjectPath],
      });
      queryClient.invalidateQueries({
        queryKey: ["git", "branch-list", projectPath],
      });
      toast({
        title: `Created worktree ${name}`,
        description: `Based off ${currentBranch}`,
        duration: 2500,
      });
      await openWorktreeSession(wt);
    },
    onError: (err, name) => {
      setPendingBranch(null);
      const msg = err instanceof Error ? err.message : String(err);
      setWorktreeError(msg);
      toast({
        title: `Failed to create worktree ${name}`,
        description: msg,
        duration: 6000,
      });
    },
  });

  const removeWorktreeMutation = useMutation({
    mutationFn: async (args: { wtPath: string; force: boolean }) => {
      setPendingWorktreePath(args.wtPath);
      await removeGitWorktree(parentProjectPath, args.wtPath, args.force);
      return args.wtPath;
    },
    onSuccess: (wtPath) => {
      setPendingWorktreePath(null);
      setWorktreeError(null);
      setFailedRemoval(null);
      queryClient.invalidateQueries({
        queryKey: ["git", "worktree-list", projectPath],
      });
      queryClient.invalidateQueries({
        queryKey: ["git", "worktree-list", parentProjectPath],
      });
      toast({
        title: "Worktree removed",
        description: wtPath,
        duration: 2500,
      });
    },
    onError: (err, vars) => {
      setPendingWorktreePath(null);
      const msg = err instanceof Error ? err.message : String(err);
      setWorktreeError(msg);
      // Stash the path so the inline "Force delete" button knows
      // what to retry without reopening the row.
      setFailedRemoval(vars.wtPath);
    },
  });

  return (
    <Popover open={open} onOpenChange={setOpen}>
      <PopoverTrigger asChild>
        {trigger ?? (
          <button
            type="button"
            className="inline-flex min-w-0 cursor-pointer items-center gap-1 truncate text-[11px] text-muted-foreground outline-none transition-colors hover:text-foreground"
          >
            <GitBranch className="h-3 w-3 shrink-0" />
            <span className="truncate">{currentBranch}</span>
          </button>
        )}
      </PopoverTrigger>
      <PopoverContent
        align="start"
        sideOffset={6}
        className="w-80 gap-0 p-0"
      >
        <div
          role="tablist"
          aria-label="Branch switcher tabs"
          className="flex items-center gap-1 border-b border-border p-1"
          onKeyDown={(e) => {
            if (e.key === "ArrowLeft" || e.key === "ArrowRight") {
              e.preventDefault();
              setTab((t) => (t === "branches" ? "worktrees" : "branches"));
            }
          }}
        >
          <TabButton
            active={tab === "branches"}
            onClick={() => setTab("branches")}
          >
            Branches
          </TabButton>
          <TabButton
            active={tab === "worktrees"}
            onClick={() => setTab("worktrees")}
          >
            Worktrees
          </TabButton>
        </div>

        {tab === "branches" ? (
          <BranchesPanel
            query={branchListQuery}
            currentBranch={currentBranch}
            pendingBranch={pendingBranch}
            isBusy={checkoutMutation.isPending || createMutation.isPending}
            onCheckoutLocal={(name) =>
              checkoutMutation.mutate({ branch: name, createTrack: null })
            }
            onCheckoutRemote={(remoteRef, localName, localExists) =>
              checkoutMutation.mutate({
                branch: localName,
                createTrack: localExists ? null : remoteRef,
              })
            }
            onCreateBranch={(name) => createMutation.mutate(name)}
            checkoutError={checkoutError}
          />
        ) : (
          <WorktreesPanel
            query={worktreeListQuery}
            currentBranch={currentBranch}
            currentSessionProjectPath={projectPath}
            pendingWorktreePath={pendingWorktreePath}
            pendingCreateName={
              createWorktreeMutation.isPending ? pendingBranch : null
            }
            isBusy={
              createWorktreeMutation.isPending ||
              removeWorktreeMutation.isPending
            }
            onOpenWorktree={(wt) => void openWorktreeSession(wt)}
            onCreateWorktree={(name) => createWorktreeMutation.mutate(name)}
            onRemoveWorktree={(wtPath) =>
              removeWorktreeMutation.mutate({ wtPath, force: false })
            }
            worktreeError={worktreeError}
            failedRemoval={failedRemoval}
            onForceDelete={() => {
              if (failedRemoval) {
                removeWorktreeMutation.mutate({
                  wtPath: failedRemoval,
                  force: true,
                });
              }
            }}
          />
        )}
      </PopoverContent>
    </Popover>
  );
}

function TabButton({
  active,
  onClick,
  children,
}: {
  active: boolean;
  onClick: () => void;
  children: React.ReactNode;
}) {
  return (
    <button
      type="button"
      role="tab"
      aria-selected={active}
      tabIndex={active ? 0 : -1}
      onClick={onClick}
      className={cn(
        "rounded-md px-2.5 py-1 text-xs font-medium outline-none transition-colors",
        active
          ? "bg-muted text-foreground"
          : "text-muted-foreground hover:text-foreground",
      )}
    >
      {children}
    </button>
  );
}

function BranchesPanel({
  query,
  currentBranch,
  pendingBranch,
  isBusy,
  onCheckoutLocal,
  onCheckoutRemote,
  onCreateBranch,
  checkoutError,
}: {
  query: ReturnType<typeof useQuery<import("@/lib/api").GitBranchList>>;
  currentBranch: string;
  pendingBranch: string | null;
  isBusy: boolean;
  onCheckoutLocal: (name: string) => void;
  onCheckoutRemote: (
    remoteRef: string,
    localName: string,
    localExists: boolean,
  ) => void;
  onCreateBranch: (name: string) => void;
  checkoutError: string | null;
}) {
  const [search, setSearch] = React.useState("");

  const data = query.data;
  const current = data?.current ?? currentBranch;
  const locals = data?.local ?? [];
  const remotes = data?.remote ?? [];

  const trimmedSearch = search.trim();
  const showCreate = React.useMemo(() => {
    if (trimmedSearch === "") return false;
    if (locals.includes(trimmedSearch)) return false;
    // `origin/feature/x` → local candidate `feature/x`; if the typed
    // name matches any remote-derived local candidate, treat it as an
    // existing branch rather than offering create.
    for (const remoteRef of remotes) {
      const slash = remoteRef.indexOf("/");
      const localName =
        slash >= 0 ? remoteRef.slice(slash + 1) : remoteRef;
      if (localName === trimmedSearch) return false;
    }
    return true;
  }, [trimmedSearch, locals, remotes]);

  return (
    <Command>
      <CommandInput
        placeholder="Filter branches…"
        autoFocus
        value={search}
        onValueChange={setSearch}
      />
      <CommandList className="max-h-[min(60vh,24rem)] overflow-y-auto">
        {query.isLoading && !data ? (
          <div className="px-3 py-4 text-xs text-muted-foreground">
            Loading branches…
          </div>
        ) : query.isError ? (
          <div className="px-3 py-4 text-xs text-destructive">
            {(query.error as Error).message}
          </div>
        ) : (
          <>
            {!showCreate && <CommandEmpty>No branch matches.</CommandEmpty>}
            {showCreate && (
              <CommandGroup forceMount>
                <CommandItem
                  forceMount
                  value={`__create_branch__${trimmedSearch}`}
                  keywords={[trimmedSearch]}
                  disabled={isBusy}
                  onSelect={() => onCreateBranch(trimmedSearch)}
                  className="items-start gap-2 py-2"
                >
                  <Plus className="mt-0.5 shrink-0" />
                  <div className="flex min-w-0 flex-col gap-0.5">
                    <span className="truncate text-sm">
                      Create Branch: "{trimmedSearch}"
                    </span>
                    <span className="truncate text-[11px] text-muted-foreground">
                      Based off {current}
                    </span>
                  </div>
                  {pendingBranch === trimmedSearch && (
                    <Loader2 className="ml-auto animate-spin" />
                  )}
                </CommandItem>
              </CommandGroup>
            )}
            {locals.length > 0 && (
              <CommandGroup heading="Local">
                {locals.map((name) => {
                  const isCurrent = name === current;
                  const isPending = pendingBranch === name;
                  return (
                    <CommandItem
                      key={`local-${name}`}
                      value={name}
                      data-checked={isCurrent ? "true" : undefined}
                      disabled={isCurrent || isBusy}
                      onSelect={() => {
                        if (isCurrent) return;
                        onCheckoutLocal(name);
                      }}
                    >
                      <GitBranch className="shrink-0 opacity-70" />
                      <span className="truncate">{name}</span>
                      {isPending && (
                        <Loader2 className="ml-auto animate-spin" />
                      )}
                    </CommandItem>
                  );
                })}
              </CommandGroup>
            )}
            {remotes.length > 0 && (
              <CommandGroup heading="Remote">
                {remotes.map((remoteRef) => {
                  const slash = remoteRef.indexOf("/");
                  const localName =
                    slash >= 0 ? remoteRef.slice(slash + 1) : remoteRef;
                  const localExists = locals.includes(localName);
                  const isPending = pendingBranch === localName;
                  return (
                    <CommandItem
                      key={`remote-${remoteRef}`}
                      value={remoteRef}
                      disabled={isBusy}
                      onSelect={() => {
                        onCheckoutRemote(remoteRef, localName, localExists);
                      }}
                    >
                      <GitBranch className="shrink-0 opacity-50" />
                      <span className="truncate">{remoteRef}</span>
                      {isPending && (
                        <Loader2 className="ml-auto animate-spin" />
                      )}
                    </CommandItem>
                  );
                })}
              </CommandGroup>
            )}
          </>
        )}
      </CommandList>
      {checkoutError && (
        <div className="max-h-40 overflow-y-auto border-t border-border bg-destructive/10 p-2 font-mono text-[11px] whitespace-pre-wrap text-destructive">
          {checkoutError}
        </div>
      )}
    </Command>
  );
}

function WorktreesPanel({
  query,
  currentBranch,
  currentSessionProjectPath,
  pendingWorktreePath,
  pendingCreateName,
  isBusy,
  onOpenWorktree,
  onCreateWorktree,
  onRemoveWorktree,
  worktreeError,
  failedRemoval,
  onForceDelete,
}: {
  query: ReturnType<typeof useQuery<GitWorktree[]>>;
  currentBranch: string;
  /** The current session's project path — used to mark the row the
   *  user is currently "in" and hide its delete button (git refuses
   *  to remove the active worktree anyway, but hiding the button up
   *  front avoids confusing errors). */
  currentSessionProjectPath: string;
  pendingWorktreePath: string | null;
  pendingCreateName: string | null;
  isBusy: boolean;
  onOpenWorktree: (wt: GitWorktree) => void;
  onCreateWorktree: (name: string) => void;
  onRemoveWorktree: (wtPath: string) => void;
  worktreeError: string | null;
  failedRemoval: string | null;
  onForceDelete: () => void;
}) {
  const [search, setSearch] = React.useState("");
  const worktrees = query.data ?? [];

  const trimmedSearch = search.trim();
  const showCreate = React.useMemo(() => {
    if (trimmedSearch === "") return false;
    // Only show create if the typed name doesn't exactly match an
    // existing worktree's branch or last path segment.
    for (const wt of worktrees) {
      if (wt.branch === trimmedSearch) return false;
      const tail = wt.path.split("/").filter(Boolean).pop() ?? "";
      if (tail === trimmedSearch) return false;
    }
    return true;
  }, [trimmedSearch, worktrees]);

  return (
    <Command>
      <CommandInput
        placeholder="Select or create worktree…"
        autoFocus
        value={search}
        onValueChange={setSearch}
      />
      <CommandList className="max-h-[min(60vh,24rem)] overflow-y-auto">
        {query.isLoading && !query.data ? (
          <div className="px-3 py-4 text-xs text-muted-foreground">
            Loading worktrees…
          </div>
        ) : query.isError ? (
          <div className="px-3 py-4 text-xs text-destructive">
            {(query.error as Error).message}
          </div>
        ) : (
          <>
            {!showCreate && <CommandEmpty>No worktree matches.</CommandEmpty>}
            {showCreate && (
              <CommandGroup forceMount>
                <CommandItem
                  forceMount
                  value={`__create_worktree__${trimmedSearch}`}
                  keywords={[trimmedSearch]}
                  disabled={isBusy}
                  onSelect={() => onCreateWorktree(trimmedSearch)}
                  className="items-start gap-2 py-2"
                >
                  <Plus className="mt-0.5 shrink-0" />
                  <div className="flex min-w-0 flex-col gap-0.5">
                    <span className="truncate text-sm">
                      Create Worktree: "{trimmedSearch}"
                    </span>
                    <span className="truncate text-[11px] text-muted-foreground">
                      Based off {currentBranch}
                    </span>
                  </div>
                  {pendingCreateName === trimmedSearch && (
                    <Loader2 className="ml-auto animate-spin" />
                  )}
                </CommandItem>
              </CommandGroup>
            )}
            <CommandGroup>
              {worktrees.map((wt) => {
                const label = wt.branch ?? "(detached)";
                const shortSha = wt.head ? wt.head.slice(0, 7) : "";
                const searchValue = `${label} ${wt.path} ${shortSha}`;
                const isCurrent = wt.path === currentSessionProjectPath;
                const isPending = pendingWorktreePath === wt.path;
                return (
                  <CommandItem
                    key={wt.path}
                    value={searchValue}
                    disabled={isBusy}
                    onSelect={() => onOpenWorktree(wt)}
                    className="items-start gap-2 py-2 pr-2"
                  >
                    <GitBranch className="mt-0.5 shrink-0 opacity-70" />
                    <div className="flex min-w-0 flex-1 flex-col gap-0.5">
                      <span className="truncate text-sm">{label}</span>
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
                    {isPending ? (
                      <Loader2 className="ml-auto animate-spin" />
                    ) : (
                      !isCurrent && (
                        <button
                          type="button"
                          aria-label={`Delete worktree ${label}`}
                          className="ml-auto inline-flex h-6 w-6 shrink-0 items-center justify-center rounded-md text-muted-foreground outline-none hover:bg-destructive/10 hover:text-destructive"
                          onClick={(e) => {
                            // Prevent cmdk from treating this as a
                            // row select — deleting a worktree should
                            // not also open it.
                            e.preventDefault();
                            e.stopPropagation();
                            onRemoveWorktree(wt.path);
                          }}
                          onMouseDown={(e) => e.stopPropagation()}
                        >
                          <Trash2 className="h-3.5 w-3.5" />
                        </button>
                      )
                    )}
                  </CommandItem>
                );
              })}
            </CommandGroup>
          </>
        )}
      </CommandList>
      {worktreeError && (
        <div className="max-h-40 overflow-y-auto border-t border-border bg-destructive/10 p-2 text-[11px] text-destructive">
          <pre className="whitespace-pre-wrap font-mono">{worktreeError}</pre>
          {failedRemoval && (
            <button
              type="button"
              className="mt-2 inline-flex h-6 items-center justify-center rounded-md border border-destructive/40 px-2 text-[11px] font-medium text-destructive outline-none hover:bg-destructive/20"
              onClick={onForceDelete}
            >
              Force delete
            </button>
          )}
        </div>
      )}
    </Command>
  );
}

// Derive the on-disk folder path for a new worktree. Convention:
// `<base>/<project-name>-worktrees/<project-name>-<sanitized>`
// where `<base>` is either the user's configured worktree base path
// from Settings or — when unset — `<dirname(parent-project-path)>/worktrees`,
// `<project-name>` is the basename of the main project path, and
// `<sanitized>` is the typed branch name lowercased with
// non-alphanumeric characters collapsed to hyphens.
function deriveWorktreePath(
  parentProjectPath: string,
  name: string,
  configuredBase: string | null,
): string {
  const projectName = basename(parentProjectPath);
  const sanitized = name
    .toLowerCase()
    .replace(/[^a-z0-9._-]/g, "-")
    .replace(/-+/g, "-")
    .replace(/^-|-$/g, "");
  const base =
    configuredBase && configuredBase.length > 0
      ? configuredBase
      : `${dirname(parentProjectPath)}/worktrees`;
  return `${base}/${projectName}-worktrees/${projectName}-${sanitized}`;
}

function basename(p: string): string {
  const stripped = p.endsWith("/") ? p.slice(0, -1) : p;
  const idx = stripped.lastIndexOf("/");
  return idx >= 0 ? stripped.slice(idx + 1) : stripped;
}

function dirname(p: string): string {
  const stripped = p.endsWith("/") ? p.slice(0, -1) : p;
  const idx = stripped.lastIndexOf("/");
  return idx >= 0 ? stripped.slice(0, idx) : ".";
}

