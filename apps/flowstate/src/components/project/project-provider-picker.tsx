import * as React from "react";
import { useNavigate } from "@tanstack/react-router";
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
import { ProviderDropdown } from "@/components/sidebar/provider-dropdown";
import { toast } from "@/hooks/use-toast";
import type { ProviderKind } from "@/lib/types";

interface ProjectProviderPickerProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
}

interface ProjectRow {
  projectId: string;
  name: string;
  path: string | undefined;
}

/**
 * Two-step new-thread picker fired by ⌘⇧N.
 *
 * Step 1: cmdk command palette listing every active project (same
 * filter the sidebar uses — worktree-children roll up under their
 * parent and aren't pickable). Type to filter; arrow + Enter selects.
 *
 * Step 2: render `ProviderDropdown` with `open` driven true, so the
 * provider menu pops automatically once a project is chosen. Provider
 * selection routes through this component's `handleStartSession` so
 * the wrapping dialog closes after navigation.
 *
 * Pressing Esc inside step 2 returns to step 1 (rather than closing
 * the whole picker) so users can change their mind without retyping
 * the whole flow.
 */
export function ProjectProviderPicker({
  open,
  onOpenChange,
}: ProjectProviderPickerProps) {
  const { state, send } = useApp();
  const navigate = useNavigate();

  const projects = React.useMemo<ProjectRow[]>(() => {
    const worktreeIds = new Set(state.projectWorktrees.keys());
    const nameFor = (projectId: string) =>
      state.projectDisplay.get(projectId)?.name ?? "Untitled project";
    // Same sort as app-sidebar.tsx's `sortedActiveProjects` so the
    // picker order matches the sidebar's, and users can navigate by
    // muscle memory.
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
      })
      .map((p) => ({
        projectId: p.projectId,
        name: nameFor(p.projectId),
        path: p.path,
      }));
  }, [state.projects, state.projectDisplay, state.projectWorktrees]);

  // `picked` non-null = step 2; null = step 1. Reset whenever the
  // dialog opens so a re-fire doesn't strand the user in a stale
  // step-2 view.
  const [picked, setPicked] = React.useState<ProjectRow | null>(null);
  React.useEffect(() => {
    if (open) setPicked(null);
  }, [open]);

  // Drive ProviderDropdown's open state. Goes true on entering step 2,
  // false on leaving (either via project change or by closing the
  // whole picker). Keeping this as state (rather than just `picked != null`)
  // lets Radix manage focus restoration cleanly when it transitions.
  const [providerOpen, setProviderOpen] = React.useState(false);
  React.useEffect(() => {
    setProviderOpen(picked !== null);
  }, [picked]);

  const handleStartSession = React.useCallback(
    async (provider: ProviderKind, model: string | undefined) => {
      if (!picked) return;
      try {
        const res = await send({
          type: "start_session",
          provider,
          model,
          project_id: picked.projectId,
        });
        if (res?.type === "session_created") {
          onOpenChange(false);
          navigate({
            to: "/chat/$sessionId",
            params: { sessionId: res.session.sessionId },
          });
        }
      } catch (err) {
        toast({
          description: `Failed to start thread: ${String(err)}`,
          duration: 4000,
        });
      }
    },
    [picked, send, navigate, onOpenChange],
  );

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="max-w-lg overflow-hidden p-0">
        <DialogHeader className="sr-only">
          <DialogTitle>New thread</DialogTitle>
          <DialogDescription>
            Pick a project, then choose a provider to start a new thread on.
          </DialogDescription>
        </DialogHeader>
        {picked ? (
          <div className="flex flex-col gap-3 px-4 py-3">
            <button
              type="button"
              onClick={() => setPicked(null)}
              className="self-start text-[11px] text-muted-foreground hover:text-foreground"
            >
              ← Choose a different project
            </button>
            <div className="flex items-center justify-between gap-2">
              <div className="min-w-0">
                <div className="truncate text-sm font-medium">{picked.name}</div>
                {picked.path && (
                  <div className="truncate text-[10px] text-muted-foreground">
                    {picked.path}
                  </div>
                )}
              </div>
              <ProviderDropdown
                projectId={picked.projectId}
                projectPath={picked.path}
                open={providerOpen}
                onOpenChange={(next) => {
                  setProviderOpen(next);
                  // Closing via Esc / outside-click returns to step 1
                  // instead of dismissing the whole picker. Selecting
                  // a provider also closes the menu, but
                  // handleStartSession has already navigated +
                  // dismissed by then so the back-step is a no-op.
                  if (!next) setPicked(null);
                }}
                onSelect={handleStartSession}
                trigger={
                  <button
                    type="button"
                    className="inline-flex h-7 shrink-0 items-center gap-1 rounded-md border border-border bg-background px-3 text-xs font-medium hover:bg-muted hover:text-foreground"
                  >
                    Choose provider…
                  </button>
                }
              />
            </div>
          </div>
        ) : (
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
                    // Combine name + path so users can type either to
                    // find a row. cmdk's filter is substring on `value`.
                    value={`${p.name} ${p.path ?? ""}`}
                    onSelect={() => setPicked(p)}
                  >
                    <span className="flex-1 truncate">{p.name}</span>
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
        )}
      </DialogContent>
    </Dialog>
  );
}
