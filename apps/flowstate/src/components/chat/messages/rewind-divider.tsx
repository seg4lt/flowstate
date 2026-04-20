import * as React from "react";
import { Undo2 } from "lucide-react";
import { useRewindFiles } from "@/hooks/useRewindFiles";
import { useSessionContext } from "../session-context";
import { RewindDialog } from "./rewind-dialog";

/**
 * Divider-style affordance rendered above each user message. Visually
 * separates one exchange from the next and surfaces the "Restore to
 * before this message" action as a discoverable control instead of a
 * hover-only icon.
 *
 * Clicking the pill kicks off a dry-run preview and opens the same
 * `RewindDialog` the hover button used before. The dialog owns the
 * state machine (preview → conflicts → applied); this component is
 * just the trigger.
 *
 * Renders `null` when the session context or turn id is missing —
 * happens on the streaming-echo row before `turn_started` lands, so
 * we never show a button that would point at an uncaptured turn.
 */
export function RewindDivider({ turnId }: { turnId: string | undefined }) {
  const session = useSessionContext();
  const sessionId = session?.sessionId;
  const rewind = useRewindFiles(sessionId ?? "");
  const [dialogOpen, setDialogOpen] = React.useState(false);

  const canRewind = Boolean(sessionId && turnId);

  const openRewind = React.useCallback(() => {
    if (!canRewind || !turnId) return;
    setDialogOpen(true);
    // Fire the dry-run preview immediately so the dialog skips the
    // loading state on fast captures.
    void rewind.preview(turnId);
  }, [canRewind, turnId, rewind]);

  if (!canRewind) return null;

  return (
    <>
      <div
        className="relative my-1 flex items-center"
        aria-hidden={false}
      >
        {/* Hairline rail — muted so it's clearly a separator, not a
            card border. */}
        <div className="h-px flex-1 bg-border/60" />
        <button
          type="button"
          onClick={openRewind}
          aria-label="Restore files to the state before this message"
          title="Restore files to the state before this message"
          className="mx-2 inline-flex items-center gap-1.5 rounded-full border border-border/60 bg-background px-2.5 py-0.5 text-[11px] text-muted-foreground transition-colors hover:border-border hover:bg-muted hover:text-foreground"
        >
          <Undo2 className="h-3 w-3" />
          Restore to before
        </button>
        <div className="h-px flex-1 bg-border/60" />
      </div>
      <RewindDialog
        open={dialogOpen}
        onOpenChange={setDialogOpen}
        rewind={rewind}
      />
    </>
  );
}
