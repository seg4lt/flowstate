import * as React from "react";
import type { AttachmentRef } from "@/lib/types";
import { PersistedAttachmentChip } from "../attachment-chip";

interface UserMessageProps {
  input: string;
  attachments?: AttachmentRef[];
  onOpenAttachment?: (attachment: AttachmentRef) => void;
}

function UserMessageInner({ input, attachments, onOpenAttachment }: UserMessageProps) {
  const hasAttachments = attachments && attachments.length > 0;
  return (
    <div className="flex justify-end">
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
