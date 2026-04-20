import * as React from "react";
import { Undo2 } from "lucide-react";
import type { AttachmentRef } from "@/lib/types";
import { PersistedAttachmentChip } from "../attachment-chip";
import { CopyButton } from "./copy-button";
import { RewindDialog } from "./rewind-dialog";
import { useRewindFiles } from "@/hooks/useRewindFiles";
import { useSessionContext } from "../session-context";

interface UserMessageProps {
  input: string;
  attachments?: AttachmentRef[];
  onOpenAttachment?: (attachment: AttachmentRef) => void;
  /** Turn id this message belongs to. Required for the rewind
   * affordance; when absent (e.g. the streaming-echo row before the
   * real turn_started arrives) the rewind button is hidden. */
  turnId?: string;
}

function UserMessageInner({
  input,
  attachments,
  onOpenAttachment,
  turnId,
}: UserMessageProps) {
  const hasAttachments = attachments && attachments.length > 0;
  const session = useSessionContext();
  const sessionId = session?.sessionId;

  // Rewind state machine is per-message: each user message gets its
  // own hook instance so the dialog can be open for one message while
  // another message is idle.
  const canRewind = Boolean(sessionId && turnId);
  const rewind = useRewindFiles(sessionId ?? "");
  const [dialogOpen, setDialogOpen] = React.useState(false);

  const openRewind = React.useCallback(() => {
    if (!canRewind || !turnId) return;
    setDialogOpen(true);
    // Kick off the dry-run preview immediately so the dialog skips
    // the "loading" state on fast captures.
    void rewind.preview(turnId);
  }, [canRewind, turnId, rewind]);

  return (
    <div className="group flex items-start justify-end gap-1">
      {/* Hover-revealed toolbar outside-left of the right-aligned bubble.
          Buttons share a transition so they reveal/hide in lockstep. */}
      <div className="flex items-start gap-1 opacity-0 transition-opacity group-hover:opacity-100 focus-within:opacity-100">
        {canRewind && (
          <button
            type="button"
            onClick={openRewind}
            aria-label="Revert file changes since this message"
            title="Revert file changes since this message"
            className="mt-0.5 inline-flex h-6 w-6 items-center justify-center rounded-md text-muted-foreground hover:bg-muted hover:text-foreground"
          >
            <Undo2 className="h-3.5 w-3.5" />
          </button>
        )}
        {input.length > 0 && (
          <CopyButton
            text={input}
            title="Copy message"
            label="Copied message"
            className="mt-0.5"
          />
        )}
      </div>
      <div className="max-w-[80%] rounded-lg bg-primary px-3 py-2 text-sm text-primary-foreground">
        {input.length > 0 && (
          <p className="whitespace-pre-wrap">{input}</p>
        )}
        {hasAttachments && (
          <div
            className={
              input.length > 0
                ? "mt-2 flex flex-wrap gap-1"
                : "flex flex-wrap gap-1"
            }
          >
            {attachments!.map((att) => (
              <PersistedAttachmentChip
                key={att.id}
                attachment={att}
                onOpen={() => onOpenAttachment?.(att)}
              />
            ))}
          </div>
        )}
      </div>
      {canRewind && (
        <RewindDialog
          open={dialogOpen}
          onOpenChange={setDialogOpen}
          rewind={rewind}
        />
      )}
    </div>
  );
}

export const UserMessage = React.memo(UserMessageInner, (prev, next) => {
  if (prev.input !== next.input) return false;
  if (prev.onOpenAttachment !== next.onOpenAttachment) return false;
  if (prev.turnId !== next.turnId) return false;
  const a = prev.attachments;
  const b = next.attachments;
  if (a === b) return true;
  if (!a || !b) return false;
  if (a.length !== b.length) return false;
  for (let i = 0; i < a.length; i++) {
    if (a[i].id !== b[i].id) return false;
  }
  return true;
});
