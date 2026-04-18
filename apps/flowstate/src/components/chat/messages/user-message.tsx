import * as React from "react";
import { RotateCcw } from "lucide-react";
import type { AttachmentRef } from "@/lib/types";
import { PersistedAttachmentChip } from "../attachment-chip";
import { CopyButton } from "./copy-button";

interface UserMessageProps {
  input: string;
  attachments?: AttachmentRef[];
  onOpenAttachment?: (attachment: AttachmentRef) => void;
  /** When provided, renders the hover-revealed "Revert file changes
   *  since this message" button outside-left of the bubble. The
   *  click handler is responsible for confirmation (chat-view owns
   *  the dialog). Undefined hides the button entirely — used for
   *  providers without `features.fileCheckpoints`. */
  onRevert?: () => void;
}

function UserMessageInner({
  input,
  attachments,
  onOpenAttachment,
  onRevert,
}: UserMessageProps) {
  const hasAttachments = attachments && attachments.length > 0;
  return (
    <div className="group flex items-start justify-end gap-1">
      {/* Hover-revealed action stack outside-left of the right-aligned
          bubble. Order matters: revert (destructive) sits FARTHER from
          the bubble so it's harder to click by accident, copy sits
          adjacent. Both follow the existing CopyButton class shape so
          they hide/show in lockstep with hover/focus. */}
      {onRevert && (
        <button
          type="button"
          onClick={onRevert}
          title="Revert file changes since this message"
          aria-label="Revert file changes since this message"
          className="mt-0.5 inline-flex h-6 w-6 shrink-0 items-center justify-center rounded text-muted-foreground opacity-0 transition-opacity hover:bg-muted hover:text-foreground group-hover:opacity-100 focus-visible:opacity-100"
        >
          <RotateCcw className="h-3 w-3" />
        </button>
      )}
      {input.length > 0 && (
        <CopyButton
          text={input}
          title="Copy message"
          label="Copied message"
          className="mt-0.5 opacity-0 transition-opacity group-hover:opacity-100 focus-visible:opacity-100"
        />
      )}
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
    </div>
  );
}

export const UserMessage = React.memo(UserMessageInner, (prev, next) => {
  if (prev.input !== next.input) return false;
  if (prev.onOpenAttachment !== next.onOpenAttachment) return false;
  if (prev.onRevert !== next.onRevert) return false;
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
