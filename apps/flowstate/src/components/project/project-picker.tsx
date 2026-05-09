import * as React from "react";
import { useNavigate } from "@tanstack/react-router";
import { GitBranch } from "lucide-react";
import {
  Command,
  CommandEmpty,
  CommandGroup,
  CommandInput,
  CommandItem,
  CommandList,
} from "@/components/ui/command";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { useApp } from "@/stores/app-store";

interface ProjectPickerProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
}

interface ProjectRow {
  projectId: string;
  /** Display label for the row. For worktree rows this is the parent
   *  project name; the branch name is rendered separately so it can be
   *  styled distinctly. */
  name: string;
  path: string | undefined;
  /** Set on worktree rows; `null` for detached HEAD worktrees. */
  branch?: string | null;
  /** True when this row is a git worktree of another project. */
  isWorktree: boolean;
}

/**
 * One-step new-thread project picker fired by ⌘⇧N.
 *
 * Lists every active project (worktree-children roll up under their
 * parents and ARE pickable as their own row, so the user can jump
 * straight to a specific worktree). Selecting a row navigates to
 * `/chat/draft/$projectId` — provider selection happens in the chat
 * toolbar from there. No `start_session` fires here.
 *
 * The component name still includes "Provider" for back-compat with
 * its sole import site (router.tsx); rename is a follow-up cleanup.
 */
export function ProjectPicker({
  open,
  onOpenChange,
}: ProjectPickerProps) {
  const { state } = useApp();
  const navigate = useNavigate();

  const projects = React.useMemo<ProjectRow[]>(() => {
    const nameFor = (projectId: string) =>
      state.projectDisplay.get(projectId)?.name ?? "Untitled project";
    const projectById = new Map(state.projects.map((p) => [p.projectId, p]));

    // Group worktrees by parent so each parent's worktrees can be
    // emitted immediately after the parent row.
    const worktreesByParent = new Map<string, typeof state.projects>();
    const orphanWorktrees: typeof state.projects = [];
    for (const p of state.projects) {
      const link = state.projectWorktrees.get(p.projectId);
      if (!link) continue;
      const parent = projectById.get(link.parentProjectId);
      if (!parent) {
        // Defensive: parent project record is missing (shouldn't happen
        // in normal use). Surface the worktree at the end so it's still
        // pickable rather than silently dropped.
        orphanWorktrees.push(p);
        continue;
      }
      const list = worktreesByParent.get(link.parentProjectId) ?? [];
      list.push(p);
      worktreesByParent.set(link.parentProjectId, list);
    }

    // Sort parents the same way `app-sidebar.tsx`'s
    // `sortedActiveProjects` does, so the picker order matches the
    // sidebar's and users can navigate by muscle memory.
    const parents = state.projects
      .filter((p) => !state.projectWorktrees.has(p.projectId))
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

    const rows: ProjectRow[] = [];
    for (const p of parents) {
      rows.push({
        projectId: p.projectId,
        name: nameFor(p.projectId),
        path: p.path,
        isWorktree: false,
      });
      const childWorktrees = worktreesByParent.get(p.projectId);
      if (!childWorktrees) continue;
      // Sort worktrees of a given parent by branch name for stable
      // ordering. Detached HEADs (branch === null) sink to the end.
      const sorted = childWorktrees.slice().sort((a, b) => {
        const ba = state.projectWorktrees.get(a.projectId)?.branch ?? "";
        const bb = state.projectWorktrees.get(b.projectId)?.branch ?? "";
        if (ba === "" && bb === "") return 0;
        if (ba === "") return 1;
        if (bb === "") return -1;
        return ba.localeCompare(bb);
      });
      for (const wt of sorted) {
        const link = state.projectWorktrees.get(wt.projectId);
        rows.push({
          projectId: wt.projectId,
          // Show the parent's name as the row label; the branch is
          // rendered as a secondary chip alongside the GitBranch icon.
          name: nameFor(p.projectId),
          path: wt.path,
          branch: link?.branch ?? null,
          isWorktree: true,
        });
      }
    }
    for (const wt of orphanWorktrees) {
      const link = state.projectWorktrees.get(wt.projectId);
      rows.push({
        projectId: wt.projectId,
        name: nameFor(wt.projectId),
        path: wt.path,
        branch: link?.branch ?? null,
        isWorktree: true,
      });
    }
    return rows;
  }, [state.projects, state.projectDisplay, state.projectWorktrees]);

  const handlePick = React.useCallback(
    (row: ProjectRow) => {
      onOpenChange(false);
      navigate({
        to: "/chat/draft/$projectId",
        params: { projectId: row.projectId },
      });
    },
    [navigate, onOpenChange],
  );

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="max-w-lg overflow-hidden p-0">
        <DialogHeader className="sr-only">
          <DialogTitle>New thread</DialogTitle>
          <DialogDescription>
            Pick a project to open a new thread on. Provider and model
            are picked in the chat toolbar after.
          </DialogDescription>
        </DialogHeader>
        <Command className="rounded-xl">
          <CommandInput
            placeholder="Pick a project to start a new thread in…"
            autoFocus
          />
          <CommandList className="max-h-[60vh]">
            <CommandEmpty>No matching project.</CommandEmpty>
            <CommandGroup heading="Projects">
              {projects.map((p) => (
                <CommandItem
                  key={p.projectId}
                  // Combine name + branch + path so users can type
                  // any of them to find a row (e.g. "fix/login" jumps
                  // straight to that worktree). cmdk's filter is
                  // substring on `value`.
                  value={`${p.name} ${p.branch ?? ""} ${p.path ?? ""}`}
                  onSelect={() => handlePick(p)}
                >
                  {p.isWorktree && (
                    <GitBranch className="mr-2 h-3.5 w-3.5 shrink-0 text-muted-foreground" />
                  )}
                  <span
                    className={
                      p.isWorktree
                        ? "truncate text-muted-foreground"
                        : "flex-1 truncate"
                    }
                  >
                    {p.name}
                  </span>
                  {p.isWorktree && (
                    <span className="ml-1.5 flex-1 truncate text-xs">
                      · {p.branch ?? "(detached)"}
                    </span>
                  )}
                  {p.path && (
                    <span className="ml-2 truncate text-[10px] text-muted-foreground">
                      {p.path}
                    </span>
                  )}
                </CommandItem>
              ))}
            </CommandGroup>
          </CommandList>
        </Command>
      </DialogContent>
    </Dialog>
  );
}
