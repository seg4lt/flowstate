import * as React from "react";
import type { AttachmentRef } from "@/lib/types";
import { PersistedAttachmentChip } from "../attachment-chip";
import { CopyButton } from "./copy-button";

interface UserMessageProps {
  input: string;
  attachments?: AttachmentRef[];
  onOpenAttachment?: (attachment: AttachmentRef) => void;
  /** `"injected"` paints the bubble in muted neutral colors and adds
   *  a subtle left-border accent so it's visually clear the human
   *  did not type this turn — runtime-injected wakeups (and peer
   *  sends/spawns) use this. Default is the standard primary
   *  bubble. The authorship chip above the bubble carries the
   *  textual label; this only handles the bubble itself. */
  variant?: "default" | "injected";
}

function UserMessageInner({
  input,
  attachments,
  onOpenAttachment,
  variant = "default",
}: UserMessageProps) {
  const hasAttachments = attachments && attachments.length > 0;
  const bubbleClass =
    variant === "injected"
      ? "max-w-[80%] rounded-lg border-l-2 border-muted-foreground/40 bg-muted/40 px-3 py-2 text-sm text-foreground"
      : "max-w-[80%] rounded-lg bg-primary px-3 py-2 text-sm text-primary-foreground";
  return (
    <div className="group flex items-start justify-end gap-1">
      {/* Hover-revealed copy button outside-left of the right-aligned
          bubble. The rewind affordance lives on the `RewindDivider`
          above this row — deliberately always-visible there, not
          hidden on hover, because discoverability of "you can undo
          the agent's work" matters more than a tidy toolbar. */}
      {input.length > 0 && (
        <CopyButton
          text={input}
          title="Copy message"
          label="Copied message"
          className="mt-0.5 opacity-0 transition-opacity group-hover:opacity-100 focus-visible:opacity-100"
        />
      )}
      <div className={bubbleClass}>
        {input.length > 0 && (
          <p className="whitespace-pre-wrap [overflow-wrap:anywhere]">{input}</p>
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
  if (prev.variant !== next.variant) return false;
  if (prev.onOpenAttachment !== next.onOpenAttachment) return false;
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
