import * as React from "react";
import { useSortable } from "@dnd-kit/sortable";
import { CSS } from "@dnd-kit/utilities";
import { cn } from "@/lib/utils";

interface SortableThreadProps {
  /** session_id — also the dnd-kit sortable id. */
  id: string;
  /** Identifier of the visual group this thread belongs to. One of:
   *    - a project_id (the parent project for worktree threads —
   *      always pass `effectiveProjectId(...)`, not session.projectId
   *      directly, so worktree threads share a group with their
   *      parent project's threads)
   *    - the literal string `"__general__"` for unassigned threads
   *    - `archived:<projectKey>` for archived-project groups
   *
   *  Carried on the `data` payload of every drag event so the
   *  top-level onDragEnd can reject cross-group drops by comparing
   *  active.data.current.groupId to over.data.current.groupId. */
  groupId: string;
  children: React.ReactNode;
}

/**
 * Drag wrapper for a thread row in the sidebar. The whole wrapped
 * element is the drag handle — ThreadItem's inner controls (rename
 * input, action cluster, copy button) already use e.stopPropagation()
 * on pointerdown / keydown so clicking those controls won't activate
 * the drag. The 6px PointerSensor distance constraint at the parent
 * DndContext level separates a click-to-navigate from a drag.
 *
 * `touch-none` is required by dnd-kit on touch devices: without it
 * the browser claims pointer events for native scroll before the
 * sensor sees them.
 */
export function SortableThread({ id, groupId, children }: SortableThreadProps) {
  const {
    attributes,
    listeners,
    setNodeRef,
    transform,
    transition,
    isDragging,
  } = useSortable({ id, data: { type: "thread", groupId } });

  const style: React.CSSProperties = {
    // Translate-only transform — same reasoning as SortableProject:
    // the default Transform string includes a scaleX/scaleY computed
    // from the sibling size delta, which would propagate to every
    // child of the dragged element and "zoom" the icon/title.
    transform: CSS.Translate.toString(transform),
    transition,
    // Source dims to ~empty-slot while the DragOverlay renders the
    // crisp preview elsewhere — same UX as the project drag.
    opacity: isDragging ? 0.35 : undefined,
    zIndex: isDragging ? 10 : undefined,
  };

  return (
    <div
      ref={setNodeRef}
      style={style}
      {...attributes}
      {...listeners}
      className={cn("touch-none", isDragging && "cursor-grabbing")}
    >
      {children}
    </div>
  );
}
