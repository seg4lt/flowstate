import * as React from "react";
import { useMutation, useQueryClient } from "@tanstack/react-query";
import { Loader2 } from "lucide-react";

import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { Button } from "@/components/ui/button";
import { Switch } from "@/components/ui/switch";
import { createGitWorktree, type GitWorktree } from "@/lib/api";
import { gitBranchListQueryOptions } from "@/lib/queries";
import { readWorktreeBasePath } from "@/lib/worktree-settings";
import { deriveWorktreePath } from "@/lib/worktree-utils";
import { toast } from "@/hooks/use-toast";

interface CreateWorktreeDialogProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  /** Filesystem path of the parent (main) project. */
  projectPath: string;
  /** Current branch — used as base_ref when creating a new branch. */
  currentBranch: string;
  /** Called after successful creation with the new worktree entry. */
  onCreated: (wt: GitWorktree) => void;
  /** Pre-fill the branch name field. When set, the branch field is
   *  treated as "touched" so auto-fill from name won't overwrite it. */
  initialBranchName?: string;
  /** Pre-fill the "checkout existing branch" toggle. */
  initialCheckoutExisting?: boolean;
}

export function CreateWorktreeDialog({
  open,
  onOpenChange,
  projectPath,
  currentBranch,
  onCreated,
  initialBranchName,
  initialCheckoutExisting,
}: CreateWorktreeDialogProps) {
  const queryClient = useQueryClient();

  const [name, setName] = React.useState("");
  const [branchName, setBranchName] = React.useState("");
  const [branchTouched, setBranchTouched] = React.useState(false);
  const [checkoutExisting, setCheckoutExisting] = React.useState(false);
  const [error, setError] = React.useState<string | null>(null);

  // Reset form state whenever the dialog opens, applying initial
  // values when provided (e.g. opened from a branch row).
  React.useEffect(() => {
    if (open) {
      setName("");
      setBranchName(initialBranchName ?? "");
      setBranchTouched(!!initialBranchName);
      setCheckoutExisting(initialCheckoutExisting ?? false);
      setError(null);
    }
  }, [open, initialBranchName, initialCheckoutExisting]);

  // Auto-fill branch from name when the user hasn't manually edited
  // the branch field. Uses the same sanitization as deriveWorktreePath
  // so the folder and branch name stay in sync.
  const handleNameChange = React.useCallback(
    (value: string) => {
      setName(value);
      setError(null);
      if (!branchTouched) {
        setBranchName(value);
      }
    },
    [branchTouched],
  );

  const handleBranchChange = React.useCallback((value: string) => {
    setBranchName(value);
    setBranchTouched(true);
    setError(null);
  }, []);

  const trimmedName = name.trim();
  const trimmedBranch = branchName.trim();
  const canSubmit = trimmedName.length > 0 && trimmedBranch.length > 0;

  const mutation = useMutation({
    mutationFn: async () => {
      // When checking out an existing branch, validate it exists first.
      if (checkoutExisting) {
        const branchData = await queryClient.ensureQueryData(
          gitBranchListQueryOptions(projectPath),
        );
        const existsLocal = branchData.local.includes(trimmedBranch);
        const existsRemote = branchData.remote.some((ref) => {
          const slash = ref.indexOf("/");
          const localName = slash >= 0 ? ref.slice(slash + 1) : ref;
          return ref === trimmedBranch || localName === trimmedBranch;
        });
        if (!existsLocal && !existsRemote) {
          throw new Error(
            `Branch "${trimmedBranch}" does not exist locally or on any remote.`,
          );
        }
      }

      const configuredBase = await readWorktreeBasePath();
      const wtPath = deriveWorktreePath(
        projectPath,
        trimmedName,
        configuredBase,
      );
      return createGitWorktree(
        projectPath,
        wtPath,
        trimmedBranch,
        currentBranch,
        checkoutExisting,
      );
    },
    onSuccess: (wt) => {
      queryClient.invalidateQueries({
        queryKey: ["git", "worktree-list", projectPath],
      });
      queryClient.invalidateQueries({
        queryKey: ["git", "branch-list", projectPath],
      });
      toast({
        title: checkoutExisting
          ? `Checked out worktree ${trimmedBranch}`
          : `Created worktree ${trimmedName}`,
        description: checkoutExisting
          ? wt.path
          : `Based off ${currentBranch}`,
        duration: 2500,
      });
      onOpenChange(false);
      onCreated(wt);
    },
    onError: (err) => {
      setError(err instanceof Error ? err.message : String(err));
    },
  });

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="sm:max-w-md">
        <DialogHeader>
          <DialogTitle>Create worktree</DialogTitle>
          <DialogDescription>
            Create a new git worktree for this project.
          </DialogDescription>
        </DialogHeader>

        <form
          onSubmit={(e) => {
            e.preventDefault();
            if (canSubmit && !mutation.isPending) {
              mutation.mutate();
            }
          }}
          className="flex flex-col gap-4"
        >
          <div className="flex flex-col gap-1.5">
            <label htmlFor="wt-name" className="text-xs font-medium">
              Name
            </label>
            <Input
              id="wt-name"
              placeholder="e.g. my-feature"
              value={name}
              onChange={(e) => handleNameChange(e.target.value)}
              autoFocus
              disabled={mutation.isPending}
            />
            <p className="text-[11px] text-muted-foreground">
              Used as the folder name for the worktree.
            </p>
          </div>

          <div className="flex flex-col gap-1.5">
            <label htmlFor="wt-branch" className="text-xs font-medium">
              Branch name
            </label>
            <Input
              id="wt-branch"
              placeholder="e.g. feature/my-feature"
              value={branchName}
              onChange={(e) => handleBranchChange(e.target.value)}
              disabled={mutation.isPending}
            />
            {!checkoutExisting && (
              <p className="text-[11px] text-muted-foreground">
                A new branch will be created based off{" "}
                <span className="font-mono">{currentBranch}</span>.
              </p>
            )}
          </div>

          <div className="flex items-center gap-2">
            <Switch
              id="wt-checkout"
              checked={checkoutExisting}
              onCheckedChange={(checked) => {
                setCheckoutExisting(checked);
                setError(null);
              }}
              disabled={mutation.isPending}
            />
            <label
              htmlFor="wt-checkout"
              className="cursor-pointer text-xs font-medium"
            >
              Checkout existing branch
            </label>
          </div>
          {checkoutExisting && (
            <p className="-mt-2 text-[11px] text-muted-foreground">
              Check out an existing local or remote branch instead of creating a
              new one. Shows an error if the branch doesn't exist.
            </p>
          )}

          {error && (
            <div className="rounded-md border border-destructive/30 bg-destructive/5 p-2 font-mono text-[11px] whitespace-pre-wrap text-destructive">
              {error}
            </div>
          )}
        </form>

        <DialogFooter>
          <Button
            variant="outline"
            size="sm"
            onClick={() => onOpenChange(false)}
            disabled={mutation.isPending}
          >
            Cancel
          </Button>
          <Button
            size="sm"
            disabled={!canSubmit || mutation.isPending}
            onClick={() => mutation.mutate()}
          >
            {mutation.isPending && (
              <Loader2 className="h-3 w-3 animate-spin" />
            )}
            {checkoutExisting ? "Checkout" : "Create"}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
