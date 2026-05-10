import * as React from "react";
import { useNavigate } from "@tanstack/react-router";
import { invoke } from "@tauri-apps/api/core";
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
import { useDefaultProvider } from "@/hooks/use-default-provider";
import { startThreadOnProject } from "@/lib/start-thread";
import { toast } from "@/hooks/use-toast";

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
 * straight to a specific worktree). Selecting a row eagerly calls
 * `start_session` with the user's default provider/model and
 * navigates straight to `/chat/$sessionId` — same flow ⌘N uses. The
 * chat toolbar lets the user swap provider/model on the live thread
 * if they want to.
 */
export function ProjectPicker({
  open,
  onOpenChange,
}: ProjectPickerProps) {
  const { state, send } = useApp();
  const navigate = useNavigate();
  const { defaultProvider, loaded: defaultProviderLoaded } =
    useDefaultProvider();
  const notify = React.useCallback((message: string) => {
    toast({ title: "New thread", description: message, duration: 4000 });
  }, []);

  // Paths confirmed to be missing on disk. Worktree directories
  // deleted out-of-band (`git worktree remove`, `rm -rf`, branch
  // cleanup) leave their `project_worktree` rows behind — without
  // this filter the picker accumulates dozens of stale entries
  // pointing at directories that no longer exist. We probe via the
  // existing `path_exists` Tauri command rather than wiring a
  // backend prune so the picker self-heals without coordinating
  // with the daemon's own lifecycle. Only entries explicitly
  // confirmed missing are filtered; pending probes render
  // optimistically so the picker stays responsive on first open.
  const [missingPaths, setMissingPaths] = React.useState<
    ReadonlySet<string>
  >(() => new Set());

  React.useEffect(() => {
    if (!open) {
      // Reset on close so the next open re-probes — picks up any
      // worktrees that have been re-created at the same path.
      setMissingPaths(new Set());
      return;
    }
    // Build the unique set of worktree paths to probe. Parents
    // aren't in scope for this fix (the user's report is
    // worktree-specific) and `path === undefined` rows have
    // nothing to verify.
    const paths = new Set<string>();
    for (const p of state.projects) {
      if (!state.projectWorktrees.has(p.projectId)) continue;
      if (typeof p.path === "string" && p.path.length > 0) {
        paths.add(p.path);
      }
    }
    if (paths.size === 0) return;
    let cancelled = false;
    const pathList = Array.from(paths);
    void Promise.all(
      pathList.map((path) =>
        invoke<boolean>("path_exists", { path }).catch(() => true),
      ),
    ).then((results) => {
      if (cancelled) return;
      const missing = new Set<string>();
      results.forEach((exists, i) => {
        if (!exists) missing.add(pathList[i]);
      });
      setMissingPaths(missing);
    });
    return () => {
      cancelled = true;
    };
  }, [open, state.projects, state.projectWorktrees]);

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
      // Drop worktrees whose directory has been confirmed missing
      // on disk. Pending probes (path not yet in the set) fall
      // through and render — the effect re-runs and the row
      // disappears once the probe resolves.
      if (typeof p.path === "string" && missingPaths.has(p.path)) continue;
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
  }, [
    state.projects,
    state.projectDisplay,
    state.projectWorktrees,
    missingPaths,
  ]);

  const handlePick = React.useCallback(
    (row: ProjectRow) => {
      onOpenChange(false);
      void startThreadOnProject({
        projectId: row.projectId,
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
    },
    [
      navigate,
      onOpenChange,
      defaultProvider,
      defaultProviderLoaded,
      send,
      notify,
    ],
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
                  className="items-start py-2"
                >
                  {p.isWorktree && (
                    <GitBranch className="mr-2 mt-0.5 h-3.5 w-3.5 shrink-0 text-muted-foreground" />
                  )}
                  {/* Two-line stack: name (+ branch chip) on top,
                      full path below with wrap. Path is no longer
                      truncated — long worktree paths break across
                      lines so they remain identifiable. */}
                  <div className="flex min-w-0 flex-1 flex-col gap-0.5">
                    <div className="flex min-w-0 items-baseline">
                      <span
                        className={
                          p.isWorktree
                            ? "truncate text-muted-foreground"
                            : "truncate"
                        }
                      >
                        {p.name}
                      </span>
                      {p.isWorktree && (
                        <span className="ml-1.5 truncate text-xs text-muted-foreground">
                          · {p.branch ?? "(detached)"}
                        </span>
                      )}
                    </div>
                    {p.path && (
                      <span className="whitespace-normal break-all text-[10px] leading-snug text-muted-foreground">
                        {p.path}
                      </span>
                    )}
                  </div>
                </CommandItem>
              ))}
            </CommandGroup>
          </CommandList>
        </Command>
      </DialogContent>
    </Dialog>
  );
}
