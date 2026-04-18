import * as React from "react";
import { AlertTriangle } from "lucide-react";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Button } from "@/components/ui/button";
import { useToast } from "@/hooks/use-toast";
import { rewindFiles } from "@/lib/api";
import type { TurnRecord } from "@/lib/types";

interface RevertFilesDialogProps {
  open: boolean;
  onOpenChange: (next: boolean) => void;
  sessionId: string;
  /** Anchor turn — every file change recorded by this turn and any
   *  later turn is undone. `null` while no message is selected; the
   *  dialog short-circuits its render when null. */
  anchorTurnId: string | null;
  /** All turns in the current session (already paginated by chat-view's
   *  React Query cache). The dialog scans these to compute the affected
   *  paths preview. We accept the array rather than re-querying so the
   *  preview matches exactly what runtime-core will see at rewind
   *  time. */
  turns: TurnRecord[];
}

/**
 * Confirm dialog for the destructive "Revert file changes since
 * here" action. Renders a preview list of the files that will be
 * touched (restored or deleted) so the user can sanity-check the
 * blast radius before clicking through. The preview is best-effort
 * — runtime-core is the source of truth at execution time, and
 * the dialog only surfaces what it sees in the cache. If a
 * concurrent turn lands between dialog open and click, the
 * runtime's broadcast `FilesRewound` event will carry the actual
 * touched lists.
 */
export function RevertFilesDialog({
  open,
  onOpenChange,
  sessionId,
  anchorTurnId,
  turns,
}: RevertFilesDialogProps) {
  const { toast } = useToast();
  const [submitting, setSubmitting] = React.useState(false);

  // Compute the preview from the cache. We do this every render
  // (cheap — bounded by the number of file changes in scope) so
  // the dialog re-renders correctly if a concurrent turn lands
  // while it's open.
  const preview = React.useMemo(() => {
    if (!anchorTurnId) {
      return { paths: [] as string[], wouldDeleteCount: 0, anchorIndex: -1 };
    }
    const idx = turns.findIndex((t) => t.turnId === anchorTurnId);
    if (idx < 0) {
      return { paths: [] as string[], wouldDeleteCount: 0, anchorIndex: -1 };
    }
    const span = turns.slice(idx);
    // Mirror the runtime's "first record per path wins" rule so the
    // preview matches what's actually going to happen on disk.
    const earliestBefore = new Map<string, string | undefined>();
    for (const turn of span) {
      for (const change of turn.fileChanges ?? []) {
        if (!earliestBefore.has(change.path)) {
          earliestBefore.set(change.path, change.before);
        }
      }
    }
    const paths = Array.from(earliestBefore.keys()).sort();
    const wouldDeleteCount = Array.from(earliestBefore.values()).filter(
      (before) => before === undefined || before === null,
    ).length;
    return { paths, wouldDeleteCount, anchorIndex: idx };
  }, [anchorTurnId, turns]);

  const turnsAffected = preview.anchorIndex >= 0
    ? turns.length - preview.anchorIndex
    : 0;

  async function handleConfirm() {
    if (!anchorTurnId) return;
    setSubmitting(true);
    try {
      await rewindFiles(sessionId, anchorTurnId);
      // Success toast carries the runtime-side counts when the
      // FilesRewound event arrives at chat-view; this in-dialog
      // toast just confirms the request was accepted.
      toast({
        description: `Reverted ${preview.paths.length} file${preview.paths.length === 1 ? "" : "s"}.`,
        duration: 3000,
      });
      onOpenChange(false);
    } catch (err) {
      toast({
        description: `Revert failed: ${err instanceof Error ? err.message : String(err)}`,
        duration: 5000,
      });
    } finally {
      setSubmitting(false);
    }
  }

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="max-w-lg">
        <DialogHeader>
          <DialogTitle className="flex items-center gap-2">
            <AlertTriangle className="h-4 w-4 text-amber-500" />
            Revert file changes?
          </DialogTitle>
          <DialogDescription>
            This will restore every file the agent touched in this message
            and the {turnsAffected - 1} message{turnsAffected - 1 === 1 ? "" : "s"} after it
            back to its prior state. Files created during the span will be
            deleted. The action cannot be undone.
          </DialogDescription>
        </DialogHeader>

        {preview.paths.length === 0 ? (
          <p className="py-4 text-sm text-muted-foreground">
            No file changes recorded for this span — nothing to revert.
          </p>
        ) : (
          <div className="max-h-56 overflow-y-auto rounded-md border border-border bg-muted/30 p-2 text-xs">
            <ul className="space-y-1 font-mono">
              {preview.paths.map((p) => (
                <li
                  key={p}
                  className="flex items-baseline justify-between gap-2 truncate"
                >
                  <span className="truncate">{p}</span>
                </li>
              ))}
            </ul>
            {preview.wouldDeleteCount > 0 && (
              <p className="mt-2 text-[11px] text-amber-700 dark:text-amber-400">
                {preview.wouldDeleteCount} file
                {preview.wouldDeleteCount === 1 ? "" : "s"} will be deleted
                (created in this span).
              </p>
            )}
          </div>
        )}

        <DialogFooter>
          <Button
            variant="outline"
            onClick={() => onOpenChange(false)}
            disabled={submitting}
          >
            Cancel
          </Button>
          <Button
            variant="destructive"
            onClick={handleConfirm}
            disabled={submitting || preview.paths.length === 0}
          >
            {submitting ? "Reverting…" : "Revert"}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
