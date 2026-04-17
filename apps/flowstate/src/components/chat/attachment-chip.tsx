import { Paperclip, X } from "lucide-react";
import { cn } from "@/lib/utils";
import type { AttachedImage, AttachmentRef } from "@/lib/types";

/**
 * Pre-send (in-flux) chip rendered above the textarea. Shows a tiny
 * thumbnail of the pasted image plus its filename. The `×` remove
 * button is hidden by default and reveals on hover (group-hover).
 *
 * Clicking the chip body opens a lightbox with the local blob URL —
 * no disk read, no network round-trip.
 */
export function InFluxAttachmentChip({
  image,
  onRemove,
  onOpen,
}: {
  image: AttachedImage;
  onRemove: () => void;
  onOpen: () => void;
}) {
  return (
    <button
      type="button"
      onClick={onOpen}
      className={cn(
        "group relative inline-flex items-center gap-2 rounded-full border border-border bg-muted/40 py-0.5 pl-1 pr-7 text-xs hover:bg-muted",
      )}
      title={image.name}
    >
      <img
        src={image.previewUrl}
        alt={image.name}
        className="h-5 w-5 shrink-0 rounded-full object-cover"
      />
      <span className="max-w-[160px] truncate">{image.name}</span>
      <span
        role="button"
        aria-label="Remove attachment"
        onClick={(e) => {
          e.stopPropagation();
          onRemove();
        }}
        className="absolute right-1 top-1/2 -translate-y-1/2 hidden rounded-full p-0.5 text-muted-foreground hover:bg-destructive/10 hover:text-destructive group-hover:inline-flex"
      >
        <X className="h-3 w-3" />
      </span>
    </button>
  );
}

/**
 * Persisted (post-send) chip rendered under a turn's input cell. Shows
 * a paperclip icon + the first 8 chars of the attachment UUID — no
 * thumbnail and no disk read until the user clicks.
 *
 * On click the parent opens the lightbox, which fetches the bytes via
 * `attachmentQueryOptions` (cached after first fetch).
 */
export function PersistedAttachmentChip({
  attachment,
  onOpen,
}: {
  attachment: AttachmentRef;
  onOpen: () => void;
}) {
  return (
    <button
      type="button"
      onClick={onOpen}
      className="inline-flex items-center gap-1.5 rounded-full border border-border bg-muted/40 px-2 py-0.5 text-[11px] hover:bg-muted"
      title={attachment.name ?? attachment.id}
    >
      <Paperclip className="h-3 w-3 text-muted-foreground" />
      <span className="font-mono">{attachment.id.slice(0, 8)}</span>
    </button>
  );
}
