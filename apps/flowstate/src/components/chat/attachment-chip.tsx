import { FileAudio, FileVideo, Paperclip, X } from "lucide-react";
import { useQuery } from "@tanstack/react-query";
import { cn } from "@/lib/utils";
import { attachmentQueryOptions } from "@/lib/queries";
import type { AttachedImage, AttachmentRef } from "@/lib/types";

/** Pick the right fallback icon for a non-image attachment. Drops
 *  can bring audio / video in addition to images — the chip shows
 *  a music note or film icon so the user can tell at a glance what
 *  they attached even without a visual thumbnail. */
function fallbackIconFor(mediaType: string) {
  if (mediaType.startsWith("audio/")) return FileAudio;
  if (mediaType.startsWith("video/")) return FileVideo;
  return Paperclip;
}

/**
 * Pre-send (in-flux) chip rendered above the textarea. Shows a tiny
 * thumbnail of the attached image plus its filename, or a media icon
 * fallback for audio / video attachments (which have no renderable
 * preview). The `×` remove button is hidden by default and reveals on
 * hover (group-hover).
 *
 * Clicking the chip body opens a lightbox with the local blob URL —
 * no disk read, no network round-trip. Audio / video chips are click-
 * through no-ops (the lightbox can't render those formats today), but
 * the `onOpen` prop is still fired so callers can evolve the preview
 * surface later without touching this component.
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
  const isImage =
    image.mediaType.startsWith("image/") && image.previewUrl.length > 0;
  const FallbackIcon = fallbackIconFor(image.mediaType);
  return (
    <button
      type="button"
      onClick={onOpen}
      className={cn(
        "group relative inline-flex items-center gap-2 rounded-full border border-border bg-muted/40 py-0.5 pl-1 pr-7 text-xs hover:bg-muted",
      )}
      title={image.name}
    >
      {isImage ? (
        <img
          src={image.previewUrl}
          alt={image.name}
          className="h-5 w-5 shrink-0 rounded-full object-cover"
        />
      ) : (
        <span className="flex h-5 w-5 shrink-0 items-center justify-center rounded-full bg-muted text-muted-foreground">
          <FallbackIcon className="h-3 w-3" />
        </span>
      )}
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
 * Persisted (post-send) chip rendered under a turn's input cell.
 *
 * For image attachments we render a ~96×96 thumbnail by lazily fetching
 * the bytes via `attachmentQueryOptions` and building a `data:` URI —
 * same path the lightbox uses, so the second open hits the cache. For
 * non-image attachments (audio / video / unknown) we fall back to the
 * paperclip + short-hash pill.
 *
 * On click the parent opens the lightbox.
 */
export function PersistedAttachmentChip({
  attachment,
  onOpen,
}: {
  attachment: AttachmentRef;
  onOpen: () => void;
}) {
  const isImage = attachment.mediaType.startsWith("image/");
  // Hooks must run unconditionally — gate the network call by passing
  // `null` for non-images so `attachmentQueryOptions`' `enabled: !!id`
  // short-circuits without a fetch.
  const query = useQuery(attachmentQueryOptions(isImage ? attachment.id : null));
  const label = attachment.name ?? attachment.id;

  if (isImage) {
    const src = query.data
      ? `data:${query.data.mediaType};base64,${query.data.dataBase64}`
      : null;
    return (
      <button
        type="button"
        onClick={onOpen}
        // Fixed 96×96 footprint (≈ "100×100 or similar") so the
        // skeleton, broken-image fallback, and loaded thumbnail share
        // the same layout — no shift when the lazy fetch resolves.
        className={cn(
          "group relative inline-flex h-24 w-24 shrink-0 items-center justify-center overflow-hidden rounded-md border border-border bg-muted/40 hover:bg-muted",
        )}
        title={label}
      >
        {src ? (
          <img
            src={src}
            alt={label}
            className="h-full w-full object-cover"
            loading="lazy"
          />
        ) : query.isError ? (
          // Don't blow up the whole bubble if a single attachment fails
          // — fall back to the old hash pill so the user still has
          // *something* to click into the lightbox with.
          <span className="flex flex-col items-center gap-1 text-muted-foreground">
            <Paperclip className="h-4 w-4" />
            <span className="font-mono text-[10px]">
              {attachment.id.slice(0, 8)}
            </span>
          </span>
        ) : (
          // Loading state: simple shimmer-free placeholder. The query
          // is fast (local IPC + base64) so a static muted block is
          // less noisy than an animated skeleton here.
          <span className="h-full w-full bg-muted/60" />
        )}
      </button>
    );
  }

  // Non-image (audio / video / unknown): unchanged short-hash pill.
  const FallbackIcon = fallbackIconFor(attachment.mediaType);
  return (
    <button
      type="button"
      onClick={onOpen}
      className="inline-flex items-center gap-1.5 rounded-full border border-border bg-muted/40 px-2 py-0.5 text-[11px] hover:bg-muted"
      title={label}
    >
      <FallbackIcon className="h-3 w-3 text-muted-foreground" />
      <span className="font-mono">{attachment.id.slice(0, 8)}</span>
    </button>
  );
}
