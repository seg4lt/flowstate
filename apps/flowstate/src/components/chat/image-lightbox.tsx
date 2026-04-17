import * as React from "react";
import { useQuery } from "@tanstack/react-query";
import { Button } from "@/components/ui/button";
import { attachmentQueryOptions } from "@/lib/queries";
import type { AttachedImage, AttachmentRef } from "@/lib/types";

/**
 * Source for the lightbox — either an in-flux pasted image (we already
 * have the blob URL + base64 in memory) or a persisted reference
 * (we have only the UUID and need to fetch via get_attachment).
 */
export type LightboxSource =
  | { kind: "inflight"; image: AttachedImage }
  | { kind: "persisted"; ref: AttachmentRef };

/**
 * Simple full-screen image preview. Backdrop click + Esc both close.
 * `onRemove` is optional — only the in-flux call site wires it up;
 * persisted attachments live on disk and aren't deletable from here.
 */
export function ImageLightbox({
  source,
  onClose,
  onRemove,
}: {
  source: LightboxSource;
  onClose: () => void;
  onRemove?: () => void;
}) {
  // Hooks must run unconditionally regardless of source variant; the
  // query is gated by `enabled` and short-circuits cleanly when this
  // is an in-flight render.
  const persistedId = source.kind === "persisted" ? source.ref.id : null;
  const query = useQuery(attachmentQueryOptions(persistedId));

  const imgSrc =
    source.kind === "inflight"
      ? source.image.previewUrl
      : query.data
        ? `data:${query.data.mediaType};base64,${query.data.dataBase64}`
        : null;

  const name =
    source.kind === "inflight"
      ? source.image.name
      : (source.ref.name ?? source.ref.id);

  React.useEffect(() => {
    function onKey(e: KeyboardEvent) {
      if (e.key === "Escape") onClose();
    }
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose]);

  return (
    <div
      role="dialog"
      aria-modal="true"
      className="fixed inset-0 z-50 flex items-center justify-center bg-background/80 backdrop-blur-sm"
      onClick={onClose}
    >
      <div
        className="relative flex max-h-[90vh] max-w-[90vw] flex-col gap-2 rounded-lg border border-border bg-background p-2 shadow-xl"
        onClick={(e) => e.stopPropagation()}
      >
        {imgSrc ? (
          <img
            src={imgSrc}
            alt={name}
            className="max-h-[80vh] max-w-[85vw] rounded object-contain"
          />
        ) : query.isError ? (
          <div className="p-8 text-sm text-destructive">
            Failed to load image: {(query.error as Error)?.message ?? "unknown error"}
          </div>
        ) : (
          <div className="p-8 text-sm text-muted-foreground">Loading…</div>
        )}
        <div className="flex items-center justify-between gap-2">
          <span className="truncate text-xs text-muted-foreground">{name}</span>
          <div className="flex items-center gap-1">
            {onRemove && (
              <Button
                variant="outline"
                size="xs"
                onClick={() => {
                  onRemove();
                  onClose();
                }}
              >
                Remove
              </Button>
            )}
            <Button variant="secondary" size="xs" onClick={onClose}>
              Close
            </Button>
          </div>
        </div>
      </div>
    </div>
  );
}
