import * as React from "react";
import { Loader2, AlertTriangle } from "lucide-react";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Button } from "@/components/ui/button";
import type {
  RewindConflictWire,
  RewindUnavailableReason,
} from "@/lib/types";
import type { UseRewindFiles } from "@/hooks/useRewindFiles";

/**
 * Preview-and-confirm modal for rewinding a session's workspace back
 * to its pre-turn state.
 *
 * The dialog always opens in "loading" for the initial dry-run round
 * trip, then transitions through one of three branches:
 *
 * - applied preview  → user sees the files that would change, clicks
 *                      Apply to commit (second round trip, dry_run=false).
 * - needs_confirmation → user sees a list of files touched by other
 *                        sessions/editors since this one last observed
 *                        them, and has to opt in to overwriting.
 * - unavailable       → rewind isn't possible for this turn; dialog
 *                      explains why. The UI could also hide the
 *                      affordance upstream, but rendering the reason
 *                      beats silent nothing.
 */
export function RewindDialog({
  open,
  onOpenChange,
  rewind,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  rewind: UseRewindFiles;
}) {
  // When the user closes the dialog, reset the hook state so the next
  // open starts clean. Done in an effect so this component stays pure
  // with respect to the open prop.
  React.useEffect(() => {
    if (!open && rewind.state.kind !== "idle") {
      rewind.cancel();
    }
  }, [open, rewind]);

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="max-w-lg">
        <DialogHeader>
          <DialogTitle>Revert file changes since this message</DialogTitle>
          <DialogDescription>
            Restore the workspace to its state just before this message
            was sent. Uses a captured snapshot — undoes every file
            change made during and after this turn, including changes
            from bash commands and outside tools.
          </DialogDescription>
        </DialogHeader>
        <RewindBody rewind={rewind} onDone={() => onOpenChange(false)} />
      </DialogContent>
    </Dialog>
  );
}

function RewindBody({
  rewind,
  onDone,
}: {
  rewind: UseRewindFiles;
  onDone: () => void;
}) {
  const { state, apply, cancel } = rewind;

  if (state.kind === "idle" || state.kind === "loading") {
    return (
      <div className="flex items-center gap-2 py-6 text-sm text-muted-foreground">
        <Loader2 className="h-4 w-4 animate-spin" />
        Loading preview…
      </div>
    );
  }

  if (state.kind === "unavailable") {
    return (
      <>
        <UnavailableBody reason={state.outcome.reason} />
        <DialogFooter>
          <Button variant="secondary" onClick={onDone}>
            Close
          </Button>
        </DialogFooter>
      </>
    );
  }

  if (state.kind === "error") {
    return (
      <>
        <div className="flex gap-2 rounded-md bg-destructive/10 p-3 text-sm text-destructive">
          <AlertTriangle className="h-4 w-4 shrink-0" />
          <div>
            <div className="font-medium">Rewind failed</div>
            <div className="mt-1 text-xs">{state.message}</div>
          </div>
        </div>
        <DialogFooter>
          <Button variant="secondary" onClick={onDone}>
            Close
          </Button>
        </DialogFooter>
      </>
    );
  }

  if (state.kind === "previewed") {
    const { paths_restored, paths_deleted, paths_skipped } = state.outcome;
    const total =
      paths_restored.length + paths_deleted.length + paths_skipped.length;
    return (
      <>
        <PreviewBody
          pathsRestored={paths_restored}
          pathsDeleted={paths_deleted}
          pathsSkipped={paths_skipped}
        />
        <DialogFooter>
          <Button variant="secondary" onClick={cancel}>
            Cancel
          </Button>
          <Button
            variant="default"
            onClick={apply}
            disabled={total === 0}
          >
            {total === 0 ? "Nothing to revert" : "Apply rewind"}
          </Button>
        </DialogFooter>
      </>
    );
  }

  if (state.kind === "conflicts_pending") {
    return (
      <>
        <ConflictsBody conflicts={state.outcome.conflicts} />
        <DialogFooter>
          <Button variant="secondary" onClick={cancel}>
            Cancel
          </Button>
          <Button variant="destructive" onClick={apply}>
            Overwrite anyway
          </Button>
        </DialogFooter>
      </>
    );
  }

  if (state.kind === "applying") {
    return (
      <div className="flex items-center gap-2 py-6 text-sm text-muted-foreground">
        <Loader2 className="h-4 w-4 animate-spin" />
        Applying rewind…
      </div>
    );
  }

  // state.kind === "applied"
  const { paths_restored, paths_deleted, paths_skipped } = state.outcome;
  return (
    <>
      <div className="rounded-md border border-border/50 bg-muted/20 p-3 text-sm">
        <div className="font-medium">Rewind complete</div>
        <ul className="mt-2 space-y-0.5 text-xs text-muted-foreground">
          <li>{paths_restored.length} file(s) restored</li>
          <li>{paths_deleted.length} file(s) deleted</li>
          {paths_skipped.length > 0 && (
            <li>
              {paths_skipped.length} file(s) skipped (no pre-state
              captured — left as-is)
            </li>
          )}
        </ul>
      </div>
      <DialogFooter>
        <Button variant="default" onClick={onDone}>
          Close
        </Button>
      </DialogFooter>
    </>
  );
}

function PreviewBody({
  pathsRestored,
  pathsDeleted,
  pathsSkipped,
}: {
  pathsRestored: string[];
  pathsDeleted: string[];
  pathsSkipped: string[];
}) {
  return (
    <div className="space-y-3">
      <Section title="Files to restore" paths={pathsRestored} tone="neutral" />
      <Section title="Files to delete" paths={pathsDeleted} tone="destructive" />
      {pathsSkipped.length > 0 && (
        <Section
          title="Can't be restored (no pre-state captured)"
          paths={pathsSkipped}
          tone="warning"
        />
      )}
    </div>
  );
}

function ConflictsBody({ conflicts }: { conflicts: RewindConflictWire[] }) {
  return (
    <div className="space-y-3">
      <div className="flex gap-2 rounded-md bg-amber-500/10 p-3 text-sm text-amber-700 dark:text-amber-300">
        <AlertTriangle className="h-4 w-4 shrink-0" />
        <div>
          <div className="font-medium">
            {conflicts.length} file(s) modified outside this session
          </div>
          <div className="mt-1 text-xs">
            These files were changed by another thread or your editor
            since this session last observed them. Applying will
            overwrite those changes.
          </div>
        </div>
      </div>
      <Section
        title="Conflicting files"
        paths={conflicts.map((c) => c.path)}
        tone="warning"
      />
    </div>
  );
}

function UnavailableBody({ reason }: { reason: RewindUnavailableReason }) {
  const message: Record<RewindUnavailableReason, string> = {
    no_checkpoint:
      "No snapshot was captured for this message. Sessions created before the rewind feature landed, or turns where capture failed silently, can't be rewound.",
    no_workspace:
      "This session has no workspace directory, so there's nothing to snapshot or restore.",
    session_not_found:
      "The session is no longer active. Reload the app and try again.",
    disabled:
      "File checkpoints are disabled for this session. Enable them in settings to use rewind.",
  };
  return (
    <div className="rounded-md bg-muted/30 p-3 text-sm text-muted-foreground">
      {message[reason] ??
        "Rewind isn't available for this turn."}
    </div>
  );
}

function Section({
  title,
  paths,
  tone,
}: {
  title: string;
  paths: string[];
  tone: "neutral" | "warning" | "destructive";
}) {
  if (paths.length === 0) return null;
  const toneClass =
    tone === "destructive"
      ? "text-destructive"
      : tone === "warning"
        ? "text-amber-700 dark:text-amber-300"
        : "text-muted-foreground";
  return (
    <div>
      <div className={`text-xs font-medium ${toneClass}`}>
        {title} · {paths.length}
      </div>
      <ul className="mt-1 max-h-40 overflow-y-auto rounded-md border border-border/40 bg-muted/20 p-2 font-mono text-[11px]">
        {paths.map((p) => (
          <li key={p} className="truncate">
            {p}
          </li>
        ))}
      </ul>
    </div>
  );
}
