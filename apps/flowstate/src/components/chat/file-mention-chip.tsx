import { FileText, X } from "lucide-react";
import { cn } from "@/lib/utils";

/** Pre-send chip for a file mention (`@path/to/foo.ts`) rendered
 *  above the textarea alongside image attachment chips. Visually
 *  distinct from `InFluxAttachmentChip`: a file icon instead of a
 *  thumbnail, basename-only display, full relative path on hover.
 *
 *  Clicking the chip body does nothing — files don't have a
 *  lightbox equivalent. Only the hover-revealed `X` is actionable;
 *  it calls `onRemove` which both drops the chip and strips the
 *  matching `@<path>` token out of the draft text. */
export function FileMentionChip({
  path,
  onRemove,
}: {
  path: string;
  onRemove: () => void;
}) {
  const slash = path.lastIndexOf("/");
  const basename = slash >= 0 ? path.slice(slash + 1) : path;
  return (
    <span
      className={cn(
        "group relative inline-flex items-center gap-1.5 rounded-full border border-border bg-muted/40 py-0.5 pl-2 pr-7 text-xs font-mono hover:bg-muted",
      )}
      title={path}
    >
      <FileText className="h-3 w-3 shrink-0 text-muted-foreground" />
      <span className="max-w-[200px] truncate">{basename}</span>
      <span
        role="button"
        aria-label="Remove file mention"
        onMouseDown={(e) => {
          // mouseDown + stopPropagation so the textarea keeps focus
          // and the parent's pointer handlers don't see this event.
          e.preventDefault();
          e.stopPropagation();
          onRemove();
        }}
        className="absolute right-1 top-1/2 -translate-y-1/2 hidden rounded-full p-0.5 text-muted-foreground hover:bg-destructive/10 hover:text-destructive group-hover:inline-flex"
      >
        <X className="h-3 w-3" />
      </span>
    </span>
  );
}
