import * as React from "react";
import {
  useMutation,
  useQuery,
  useQueryClient,
} from "@tanstack/react-query";
import { useNavigate } from "@tanstack/react-router";
import { FolderGit2, GitBranch, Loader2, Plus, Trash2 } from "lucide-react";

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
import {
  gitCheckout,
  gitCreateBranch,
  gitDeleteBranch,
  type GitWorktree,
} from "@/lib/api";
import { gitBranchListQueryOptions } from "@/lib/queries";
import { toast } from "@/hooks/use-toast";
import { useApp } from "@/stores/app-store";
import { CreateWorktreeDialog } from "@/components/project/create-worktree-dialog";
import { samePath } from "@/lib/worktree-utils";
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
  /** Override the default popover trigger. Used by the project-home
   *  page to render the button as a sized action card instead of the
   *  tiny inline branch label the chat header shows. */
  trigger?: React.ReactElement;
}

export function BranchSwitcher({
  projectPath,
  currentBranch,
  parentProjectId,
  parentProjectPath,
  provider,
  model,
  onCheckedOut,
  trigger,
}: BranchSwitcherProps) {
  const [open, setOpen] = React.useState(false);
  const [checkoutError, setCheckoutError] = React.useState<string | null>(null);
  const [pendingBranch, setPendingBranch] = React.useState<string | null>(null);

  // Worktree dialog state — tracks whether the dialog is open and
  // what initial values to seed it with. Opened either from the
  // "Create Worktree" suggestion (new branch) or from the FolderGit2
  // icon button on an existing branch row (checkout existing).
  const [wtDialog, setWtDialog] = React.useState<{
    open: boolean;
    branchName: string;
    checkoutExisting: boolean;
  }>({ open: false, branchName: "", checkoutExisting: false });

  const queryClient = useQueryClient();
  const navigate = useNavigate();
  const { state, send, createProject, linkProjectWorktree } = useApp();

  const branchListQuery = useQuery({
    ...gitBranchListQueryOptions(projectPath),
    enabled: open,
  });

  const invalidateBranchQueries = React.useCallback(() => {
    queryClient.invalidateQueries({
      queryKey: ["git", "branch", projectPath],
    });
    queryClient.invalidateQueries({
      queryKey: ["git", "branch-list", projectPath],
    });
  }, [projectPath, queryClient]);

  const invalidateAfterBranchChange = React.useCallback(() => {
    invalidateBranchQueries();
    onCheckedOut();
    setOpen(false);
  }, [invalidateBranchQueries, onCheckedOut]);

  // Fire-and-forget refresh of the current-branch label whenever the
  // user opens the popover. This catches out-of-band checkouts (e.g.
  // the user ran `git checkout` in a terminal) without blocking the
  // click — React Query refetches in the background and the label
  // updates on the next render.
  const handleOpenChange = React.useCallback(
    (next: boolean) => {
      setOpen(next);
      if (next) {
        void queryClient.invalidateQueries({
          queryKey: ["git", "branch", projectPath],
        });
        void queryClient.invalidateQueries({
          queryKey: ["git", "branch-list", projectPath],
        });
      }
    },
    [projectPath, queryClient],
  );

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

  const deleteMutation = useMutation({
    mutationFn: async (name: string) => {
      setPendingBranch(name);
      await gitDeleteBranch(projectPath, name);
    },
    onSuccess: (_data, name) => {
      setPendingBranch(null);
      setCheckoutError(null);
      // Don't close the popover or notify parent — we only delete
      // non-current branches, so the checked-out branch is unchanged
      // and the user likely wants to keep browsing/deleting.
      invalidateBranchQueries();
      toast({
        title: `Deleted branch ${name}`,
        duration: 2500,
      });
    },
    onError: (err, name) => {
      setPendingBranch(null);
      const msg = err instanceof Error ? err.message : String(err);
      setCheckoutError(msg);
      toast({
        title: `Failed to delete branch ${name}`,
        description: msg,
        duration: 6000,
      });
    },
  });

  React.useEffect(() => {
    if (open) {
      setCheckoutError(null);
    }
  }, [open]);

  // Find-or-create flow for opening a worktree as a flowstate thread.
  // Each worktree has its own SDK project (so the agent SDK's existing
  // cwd resolution picks up the worktree folder without any SDK-level
  // worktree awareness). If a thread already exists under that SDK
  // project we focus it; otherwise we start a new session and link it
  // to the parent project via the flowstate-side `project_worktree`
  // table. The parent link is what the sidebar reads to visually
  // group worktree threads under the main repo's project header.
  const openWorktreeSession = React.useCallback(
    async (wt: GitWorktree) => {
      // Normalize path comparisons — git porcelain and the file picker
      // can disagree on trailing slashes, and a mismatch here means we
      // double-create a project for the same worktree path and/or skip
      // the parent-link, which leaves the worktree thread as a
      // top-level project in the sidebar instead of grouped under the
      // main repo.
      let wtProjectId =
        state.projects.find((p) => samePath(p.path, wt.path))?.projectId ??
        null;

      if (!wtProjectId) {
        const displayName = wt.branch ?? "(worktree)";
        // Create + link in one flow so the parent link lands in the
        // same render as project_created — avoids a top-level
        // "Untitled project" flash before the worktree regroups.
        wtProjectId = await createProject(wt.path, displayName, {
          parentProjectId,
          branch: wt.branch,
        });
      } else if (
        wtProjectId !== parentProjectId &&
        !state.projectWorktrees.has(wtProjectId)
      ) {
        // Existing project that somehow lost its parent row — relink.
        await linkProjectWorktree(wtProjectId, parentProjectId, wt.branch);
      }

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

  const openWorktreeDialog = React.useCallback(
    (branchName: string, checkoutExisting: boolean) => {
      setWtDialog({ open: true, branchName, checkoutExisting });
    },
    [],
  );

  return (
    <>
      <Popover open={open} onOpenChange={handleOpenChange}>
        <PopoverTrigger asChild>
          {trigger ?? (
            <button
              type="button"
              className="inline-flex shrink-0 cursor-pointer items-center gap-1 text-[11px] text-muted-foreground outline-none transition-colors hover:text-foreground"
            >
              <GitBranch className="h-3 w-3 shrink-0" />
              <span>{currentBranch}</span>
            </button>
          )}
        </PopoverTrigger>
        <PopoverContent
          align="start"
          sideOffset={6}
          className="w-80 gap-0 p-0"
        >
          <BranchesPanel
            query={branchListQuery}
            currentBranch={currentBranch}
            pendingBranch={pendingBranch}
            isBusy={checkoutMutation.isPending || createMutation.isPending || deleteMutation.isPending}
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
            onDeleteBranch={(name) => {
              const ok = window.confirm(`Delete local branch "${name}"?`);
              if (!ok) return;
              deleteMutation.mutate(name);
            }}
            onOpenWorktreeDialog={openWorktreeDialog}
            checkoutError={checkoutError}
          />
        </PopoverContent>
      </Popover>

      <CreateWorktreeDialog
        open={wtDialog.open}
        onOpenChange={(v) => setWtDialog((prev) => ({ ...prev, open: v }))}
        projectPath={parentProjectPath}
        currentBranch={currentBranch}
        initialBranchName={wtDialog.branchName}
        initialCheckoutExisting={wtDialog.checkoutExisting}
        onCreated={(wt) => {
          setOpen(false);
          void openWorktreeSession(wt);
        }}
      />
    </>
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
  onDeleteBranch,
  onOpenWorktreeDialog,
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
  onDeleteBranch: (name: string) => void;
  /** Open the create-worktree dialog pre-filled with the given branch
   *  name and checkout-existing toggle value. */
  onOpenWorktreeDialog: (branchName: string, checkoutExisting: boolean) => void;
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
                      Create branch: "{trimmedSearch}"
                    </span>
                    <span className="truncate text-[11px] text-muted-foreground">
                      Based off {current}
                    </span>
                  </div>
                  {pendingBranch === trimmedSearch && (
                    <Loader2 className="ml-auto animate-spin" />
                  )}
                </CommandItem>
                <CommandItem
                  forceMount
                  value={`__create_worktree__${trimmedSearch}`}
                  keywords={[trimmedSearch]}
                  disabled={isBusy}
                  onSelect={() =>
                    onOpenWorktreeDialog(trimmedSearch, false)
                  }
                  className="items-start gap-2 py-2"
                >
                  <FolderGit2 className="mt-0.5 shrink-0" />
                  <div className="flex min-w-0 flex-col gap-0.5">
                    <span className="truncate text-sm">
                      Create worktree: "{trimmedSearch}"
                    </span>
                    <span className="truncate text-[11px] text-muted-foreground">
                      New branch based off {current}
                    </span>
                  </div>
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
                      className="pr-2"
                    >
                      <GitBranch className="shrink-0 opacity-70" />
                      <span className="min-w-0 flex-1 truncate">{name}</span>
                      {isPending ? (
                        <Loader2 className="order-1 shrink-0 animate-spin" />
                      ) : (
                        <span className="order-1 flex shrink-0 items-center gap-0.5">
                          <button
                            type="button"
                            aria-label={`Create worktree from ${name}`}
                            title="Create worktree from this branch"
                            className="inline-flex h-6 w-6 items-center justify-center rounded-md text-muted-foreground outline-none hover:bg-muted hover:text-foreground"
                            onClick={(e) => {
                              e.preventDefault();
                              e.stopPropagation();
                              onOpenWorktreeDialog(name, true);
                            }}
                            onMouseDown={(e) => e.stopPropagation()}
                          >
                            <FolderGit2 className="h-3.5 w-3.5" />
                          </button>
                          {!isCurrent && (
                            <button
                              type="button"
                              aria-label={`Delete branch ${name}`}
                              title="Delete local branch"
                              className="inline-flex h-6 w-6 items-center justify-center rounded-md text-muted-foreground outline-none hover:bg-destructive/10 hover:text-destructive"
                              onClick={(e) => {
                                e.preventDefault();
                                e.stopPropagation();
                                onDeleteBranch(name);
                              }}
                              onMouseDown={(e) => e.stopPropagation()}
                            >
                              <Trash2 className="h-3.5 w-3.5" />
                            </button>
                          )}
                        </span>
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
                      className="pr-2"
                    >
                      <GitBranch className="shrink-0 opacity-50" />
                      <span className="min-w-0 flex-1 truncate">{remoteRef}</span>
                      {isPending ? (
                        <Loader2 className="order-1 shrink-0 animate-spin" />
                      ) : (
                        <button
                          type="button"
                          aria-label={`Create worktree from ${localName}`}
                          title="Create worktree from this branch"
                          className="order-1 inline-flex h-6 w-6 shrink-0 items-center justify-center rounded-md text-muted-foreground outline-none hover:bg-muted hover:text-foreground"
                          onClick={(e) => {
                            e.preventDefault();
                            e.stopPropagation();
                            onOpenWorktreeDialog(localName, true);
                          }}
                          onMouseDown={(e) => e.stopPropagation()}
                        >
                          <FolderGit2 className="h-3.5 w-3.5" />
                        </button>
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
