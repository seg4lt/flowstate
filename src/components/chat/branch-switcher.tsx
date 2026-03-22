import * as React from "react";
import {
  useMutation,
  useQuery,
  useQueryClient,
} from "@tanstack/react-query";
import { GitBranch, Loader2, Plus } from "lucide-react";

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
import { gitCheckout, gitCreateBranch } from "@/lib/api";
import {
  gitBranchListQueryOptions,
  gitWorktreeListQueryOptions,
} from "@/lib/queries";
import { toast } from "@/hooks/use-toast";

interface BranchSwitcherProps {
  projectPath: string;
  currentBranch: string;
  onCheckedOut: () => void;
}

type Tab = "branches" | "worktrees";

export function BranchSwitcher({
  projectPath,
  currentBranch,
  onCheckedOut,
}: BranchSwitcherProps) {
  const [open, setOpen] = React.useState(false);
  const [tab, setTab] = React.useState<Tab>("branches");
  const [checkoutError, setCheckoutError] = React.useState<string | null>(null);
  const [pendingBranch, setPendingBranch] = React.useState<string | null>(null);

  const queryClient = useQueryClient();

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
      setTab("branches");
    }
  }, [open]);

  const handleCopyWorktreePath = React.useCallback(
    async (wtPath: string) => {
      try {
        await navigator.clipboard.writeText(wtPath);
        toast({
          title: "Copied worktree path",
          description: wtPath,
          duration: 2500,
        });
        setOpen(false);
      } catch (err) {
        const msg = err instanceof Error ? err.message : String(err);
        toast({
          title: "Failed to copy path",
          description: msg,
          duration: 4000,
        });
      }
    },
    [],
  );

  return (
    <Popover open={open} onOpenChange={setOpen}>
      <PopoverTrigger asChild>
        <button
          type="button"
          className="inline-flex min-w-0 cursor-pointer items-center gap-1 truncate text-[11px] text-muted-foreground outline-none transition-colors hover:text-foreground"
        >
          <GitBranch className="h-3 w-3 shrink-0" />
          <span className="truncate">{currentBranch}</span>
        </button>
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
            onSelect={handleCopyWorktreePath}
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
  onSelect,
}: {
  query: ReturnType<typeof useQuery<import("@/lib/api").GitWorktree[]>>;
  onSelect: (path: string) => void;
}) {
  const worktrees = query.data ?? [];
  return (
    <Command>
      <CommandInput placeholder="Select worktree…" autoFocus />
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
            <CommandEmpty>No worktree matches.</CommandEmpty>
            <CommandGroup>
              {worktrees.map((wt) => {
                const label = wt.branch ?? "(detached)";
                const shortSha = wt.head ? wt.head.slice(0, 7) : "";
                const searchValue = `${label} ${wt.path} ${shortSha}`;
                return (
                  <CommandItem
                    key={wt.path}
                    value={searchValue}
                    onSelect={() => onSelect(wt.path)}
                    className="items-start gap-2 py-2"
                  >
                    <GitBranch className="mt-0.5 shrink-0 opacity-70" />
                    <div className="flex min-w-0 flex-col gap-0.5">
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
                  </CommandItem>
                );
              })}
            </CommandGroup>
          </>
        )}
      </CommandList>
    </Command>
  );
}

