import * as React from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import {
  ChevronDown,
  ChevronRight,
  Copy,
  FilePlus,
  FileText,
  Folder,
  FolderPlus,
  Pencil,
  Trash2,
} from "lucide-react";
import { directoryQueryOptions } from "@/lib/queries";
import {
  createProjectDir,
  createProjectFile,
  moveProjectPath,
  renameProjectPath,
  trashProjectPath,
  type DirEntry,
} from "@/lib/api";
import {
  ContextMenu,
  ContextMenuContent,
  ContextMenuItem,
  ContextMenuSeparator,
  ContextMenuTrigger,
} from "@/components/ui/context-menu";
import { cn } from "@/lib/utils";

// Lazy, per-directory file tree. Each folder fetches its immediate
// children on first expansion via the `list_directory` Tauri command,
// which INCLUDES gitignored entries (flagged as dimmed). This means
// heavy directories like `node_modules/` and `dist/` show up at the
// top level but never get walked until the user explicitly opens
// them.
//
// In addition to the read-only view, the tree supports:
//
//   - Drag-and-drop: drag any file/folder onto another folder (or the
//     root) to move it. The destination auto-expands so the moved
//     entry is visible in its new home. Same-parent / self / into-own-
//     descendant drops are rejected pre-flight (no UI highlight).
//   - Right-click context menu (Radix `ContextMenu`): New file, New
//     folder, Rename, Copy path, Copy relative path, Move to trash.
//   - Inline placeholder rows for create/rename — Enter commits,
//     Esc/blur cancels.
//
// Mutations route through five `#[tauri::command]`s in `src-tauri/src/
// lib.rs` (`create_project_file`, `create_project_dir`,
// `rename_project_path`, `move_project_path`, `trash_project_path`).
// All paths are forward-slash project-relative so the frontend never
// has to think about absolute paths.
//
// The data flow is:
//   - Root level: one query for `subPath = ""`, rendered below the
//     root placeholder.
//   - Each folder: its own query for `subPath = <folder.path>`,
//     fired only when the folder is expanded (React Query's
//     `enabled: expanded`).
//
// Results are cached per `(projectPath, subPath)` pair in the React
// Query cache (staleTime: Infinity, gcTime: 30 min), so closing and
// reopening a folder is instant and no extra round-trip fires. After
// every mutation we invalidate the affected directory's query so
// the new state appears without a manual refresh.

/**
 * MIME used by the tree's drag operation. The exact value isn't
 * inspected by anything outside this file — it just lets dragenter /
 * dragover decide whether to react (so dragging a *file from Finder*
 * over the tree doesn't trigger our move-into-folder UI).
 */
const TREE_DRAG_MIME = "application/x-flowstate-tree";

/**
 * Module-scope handle on the in-flight drag. `dataTransfer.getData`
 * isn't readable during `dragover` for security reasons, so we mirror
 * the dragged sub-path here while a tree-row drag is in flight and
 * clear it on `dragend`. Only one drag happens at a time.
 */
let draggedSourcePath: string | null = null;

/**
 * Pointer coords at `dragstart`, used by the root drop zone to gate
 * "click + release without moving" sequences. WebKit will fire a
 * dragstart → dragend pair from a tiny mousedown/up that the user
 * intended as a click; without a movement check the bubbled drop
 * lands at the project root and silently moves the file there.
 *
 * Cleared on `dragend` together with `draggedSourcePath`.
 */
let dragStartCoords: { x: number; y: number } | null = null;

/** Squared minimum-movement threshold (px²) before the root drop
 *  zone considers a drag "real". 6 px in any direction. Keeps a
 *  no-intent click + release from accidentally moving a file. */
const ROOT_DROP_MIN_MOVE_PX_SQ = 6 * 6;

/**
 * Predicate: would moving `source` into `targetDir` be a meaningful,
 * legal operation? Used to gate the drop-target highlight so the user
 * only sees a glow on rows where letting go will actually do
 * something.
 *
 *   - Refuse drops onto the source itself.
 *   - Refuse drops into a descendant of the source (would corrupt the
 *     tree — same check the backend enforces).
 *   - Refuse drops into the source's existing parent (no-op move).
 *
 * `targetDir` of `""` means the project root.
 */
function canDropInto(source: string, targetDir: string): boolean {
  if (!source) return false;
  if (source === targetDir) return false;
  // Drop into descendant — `targetDir` lives under `source/`.
  if (targetDir.startsWith(`${source}/`)) return false;
  // No-op — `source`'s parent already equals `targetDir`.
  const lastSlash = source.lastIndexOf("/");
  const parent = lastSlash > 0 ? source.slice(0, lastSlash) : "";
  if (parent === targetDir) return false;
  return true;
}

/**
 * Compute the parent sub-path of `subPath`. Returns `""` when
 * `subPath` is a top-level entry (i.e. its parent is the project
 * root).
 */
function parentSubPath(subPath: string): string {
  const i = subPath.lastIndexOf("/");
  return i > 0 ? subPath.slice(0, i) : "";
}

/**
 * Inline-edit state. At most one edit operation is active at a time,
 * so a single union value driven by `useState` is enough — no global
 * store needed.
 */
type EditingState =
  | { kind: "rename"; path: string; seed: string }
  | { kind: "create"; parentDir: string; childKind: "file" | "folder" }
  | null;

interface FileTreeProps {
  /** Project root. Required — the tree renders nothing without it. */
  projectPath: string | null;
  /** Currently-focused file's project-relative path, for highlighting
   *  and auto-expanding parents when the selection changes. */
  selectedPath: string | null;
  /** Fired when the user clicks a file row. Parent owns open-in-tab
   *  + url routing behavior; this component is purely presentational. */
  onSelect: (path: string) => void;
  /** Optional callback fired when a path is removed (trashed) or its
   *  parent directory was. Receives the sub-path of the removed entry;
   *  the parent should close any open editor tabs whose path matches
   *  or sits under it. */
  onPathRemoved?: (subPath: string) => void;
  /** Optional callback fired when a path is renamed or moved. Receives
   *  both the old and new sub-paths; the parent should re-target any
   *  open editor tab whose path matches the old path or sits under
   *  it. */
  onPathRenamed?: (oldSubPath: string, newSubPath: string) => void;
}

/**
 * Internal context used by descendants to dispatch tree-wide actions
 * (toggle folders, change inline-edit state) without prop-drilling
 * through every level. Lives only inside this module — the public
 * surface is still `FileTreeProps`.
 */
interface FileTreeCtx {
  projectPath: string;
  selectedPath: string | null;
  expanded: Set<string>;
  toggle: (path: string) => void;
  ensureExpanded: (path: string) => void;
  editing: EditingState;
  setEditing: React.Dispatch<React.SetStateAction<EditingState>>;
  invalidateDir: (subPath: string) => void;
  onSelect: (path: string) => void;
  onPathRemoved?: (subPath: string) => void;
  onPathRenamed?: (oldSubPath: string, newSubPath: string) => void;
}

const FileTreeContext = React.createContext<FileTreeCtx | null>(null);

function useTreeCtx(): FileTreeCtx {
  const ctx = React.useContext(FileTreeContext);
  if (!ctx) {
    throw new Error("FileTreeContext is missing — render inside <FileTree>");
  }
  return ctx;
}

export function FileTree({
  projectPath,
  selectedPath,
  onSelect,
  onPathRemoved,
  onPathRenamed,
}: FileTreeProps) {
  // Which folder paths are currently expanded. Keyed by forward-slash
  // full project-relative path. Survives tree re-renders.
  const [expanded, setExpanded] = React.useState<Set<string>>(
    () => new Set<string>(),
  );
  const [editing, setEditing] = React.useState<EditingState>(null);
  const queryClient = useQueryClient();

  const toggle = React.useCallback((path: string) => {
    setExpanded((prev) => {
      const next = new Set(prev);
      if (next.has(path)) next.delete(path);
      else next.add(path);
      return next;
    });
  }, []);

  const ensureExpanded = React.useCallback((path: string) => {
    setExpanded((prev) => {
      if (prev.has(path)) return prev;
      const next = new Set(prev);
      next.add(path);
      return next;
    });
  }, []);

  // Invalidate the cached listing for one directory after a mutation.
  // Triggers a single re-fetch for that level only — cheap, avoids
  // walking unrelated subtrees.
  const invalidateDir = React.useCallback(
    (subPath: string) => {
      if (!projectPath) return;
      void queryClient.invalidateQueries({
        queryKey: ["code", "directory", projectPath, subPath],
      });
    },
    [projectPath, queryClient],
  );

  // Auto-expand every ancestor of the selected file so that a pick
  // from the file picker (or a deep-link URL) makes the row visible
  // in the tree. Cheap: walks the path components once.
  React.useEffect(() => {
    if (!selectedPath) return;
    const segments = selectedPath.split("/");
    if (segments.length <= 1) return;
    setExpanded((prev) => {
      let changed = false;
      const next = new Set(prev);
      let acc = "";
      for (let i = 0; i < segments.length - 1; i++) {
        acc = acc ? `${acc}/${segments[i]}` : segments[i];
        if (!next.has(acc)) {
          next.add(acc);
          changed = true;
        }
      }
      return changed ? next : prev;
    });
  }, [selectedPath]);

  if (!projectPath) {
    return (
      <div className="px-3 py-4 text-center text-[11px] text-muted-foreground">
        No project for this session.
      </div>
    );
  }

  const ctx: FileTreeCtx = {
    projectPath,
    selectedPath,
    expanded,
    toggle,
    ensureExpanded,
    editing,
    setEditing,
    invalidateDir,
    onSelect,
    onPathRemoved,
    onPathRenamed,
  };

  // Mount the auto-scroll behaviour against whatever scroll container
  // hosts this tree. The hook walks up from the wrapper to find the
  // nearest scrollable ancestor on each drag — letting any layout
  // (e.g. a future fullscreen or popout) work without wiring.
  const wrapperRef = React.useRef<HTMLDivElement | null>(null);
  useDragAutoScroll(wrapperRef);

  return (
    <div ref={wrapperRef} className="contents">
      <FileTreeContext.Provider value={ctx}>
        <RootDropZone>
          <ul role="tree" className="py-1">
            {editing?.kind === "create" && editing.parentDir === "" ? (
              <CreatePlaceholder parentDir="" indentDepth={0} />
            ) : null}
            <DirectoryChildren parentPath="" depth={0} />
          </ul>
        </RootDropZone>
      </FileTreeContext.Provider>
    </div>
  );
}

/**
 * While a tree drag is in flight, scroll the tree's nearest scrollable
 * ancestor when the cursor approaches the top or bottom edge. Without
 * this, large trees become un-droppable past the viewport — the user
 * can see the destination folder above the fold but has no way to
 * bring it into the viewport with a held drag.
 *
 * Implementation:
 *   - Listen for `dragover` on `window`. The event fires on whatever
 *     element the cursor is currently over, including non-droppable
 *     ones, which is exactly what we want for edge detection.
 *   - Compute the cursor's distance from the scroller's top/bottom.
 *     Inside an `EDGE` band, set a target velocity proportional to
 *     how deep the cursor is into the band; outside, velocity is 0.
 *   - rAF loop applies the velocity until either the cursor leaves
 *     the band or the drag ends. Velocity is clamped at `MAX_SPEED`
 *     so reaching the bottom of a 50k-file tree is fast but not
 *     teleport-fast.
 *
 * Only fires while `draggedSourcePath` is non-null so external drags
 * (e.g. dragging a file from Finder) don't twitch the tree.
 */
function useDragAutoScroll(wrapperRef: React.RefObject<HTMLDivElement | null>) {
  React.useEffect(() => {
    const wrapper = wrapperRef.current;
    if (!wrapper) return;
    let scroller: HTMLElement | null = null;
    let raf: number | null = null;
    let velocityY = 0;
    const EDGE = 36;
    const MAX_SPEED = 14;

    const findScroller = (): HTMLElement | null => {
      // Walk up from the wrapper. The first element that has a
      // scrollable overflow setting and a clipped height wins.
      let node: HTMLElement | null = wrapper.parentElement;
      while (node && node !== document.body) {
        const style = window.getComputedStyle(node);
        const overflowY = style.overflowY;
        if (
          (overflowY === "auto" || overflowY === "scroll") &&
          node.scrollHeight > node.clientHeight
        ) {
          return node;
        }
        node = node.parentElement;
      }
      return null;
    };

    const tick = () => {
      raf = null;
      if (!draggedSourcePath || velocityY === 0 || !scroller) return;
      scroller.scrollTop += velocityY;
      raf = requestAnimationFrame(tick);
    };

    const onMove = (e: DragEvent) => {
      if (!draggedSourcePath) {
        velocityY = 0;
        return;
      }
      // Resolve the scroller lazily — the parent layout can change
      // between the FileTree's mount and the first drag (e.g. tree
      // collapse / expand).
      if (!scroller || !scroller.isConnected) {
        scroller = findScroller();
        if (!scroller) return;
      }
      const rect = scroller.getBoundingClientRect();
      const fromTop = e.clientY - rect.top;
      const fromBottom = rect.bottom - e.clientY;
      let next = 0;
      if (fromTop >= 0 && fromTop < EDGE) {
        next = -Math.round(((EDGE - fromTop) / EDGE) * MAX_SPEED);
      } else if (fromBottom >= 0 && fromBottom < EDGE) {
        next = Math.round(((EDGE - fromBottom) / EDGE) * MAX_SPEED);
      }
      velocityY = next;
      if (velocityY !== 0 && raf === null) {
        raf = requestAnimationFrame(tick);
      }
    };

    const stop = () => {
      velocityY = 0;
      if (raf !== null) {
        cancelAnimationFrame(raf);
        raf = null;
      }
    };

    // CAPTURE PHASE is critical here. The folder/file row handlers
    // call `stopPropagation()` on dragover (so the root drop zone
    // doesn't double-highlight), which kills bubble-phase listeners.
    // Capture-phase listeners run BEFORE any element handler can
    // intervene, so we always see the cursor regardless of how the
    // tree's own row handlers route the event.
    window.addEventListener("dragover", onMove, { capture: true });
    window.addEventListener("dragend", stop, { capture: true });
    window.addEventListener("drop", stop, { capture: true });
    return () => {
      window.removeEventListener("dragover", onMove, { capture: true });
      window.removeEventListener("dragend", stop, { capture: true });
      window.removeEventListener("drop", stop, { capture: true });
      if (raf !== null) cancelAnimationFrame(raf);
    };
  }, [wrapperRef]);
}

/**
 * Wraps the root `<ul>` in a drop zone so dropping anywhere in the
 * blank space (or directly between/below rows) lands the entry at the
 * project root. Folders inside the tree have their own drop targets
 * that take precedence (we `stopPropagation` on row drops).
 */
function RootDropZone({ children }: { children: React.ReactNode }) {
  const ctx = useTreeCtx();
  const [dragOver, setDragOver] = React.useState(false);
  const dragDepth = React.useRef(0);

  const onDragEnter = (e: React.DragEvent<HTMLDivElement>) => {
    if (!draggedSourcePath || !canDropInto(draggedSourcePath, "")) return;
    e.preventDefault();
    dragDepth.current += 1;
    setDragOver(true);
  };
  const onDragOver = (e: React.DragEvent<HTMLDivElement>) => {
    if (!draggedSourcePath || !canDropInto(draggedSourcePath, "")) return;
    e.preventDefault();
    e.dataTransfer.dropEffect = "move";
  };
  const onDragLeave = () => {
    dragDepth.current = Math.max(0, dragDepth.current - 1);
    if (dragDepth.current === 0) setDragOver(false);
  };
  const onDrop = async (e: React.DragEvent<HTMLDivElement>) => {
    e.preventDefault();
    dragDepth.current = 0;
    setDragOver(false);
    const src =
      draggedSourcePath ?? e.dataTransfer.getData(TREE_DRAG_MIME) ?? "";
    if (!src || !canDropInto(src, "")) return;
    // Movement gate: a click + release without dragging produces a
    // dragstart → dragend pair where the cursor never moved. Without
    // this guard the drop bubbles up to the root and the file gets
    // silently relocated to the project root. Require a few pixels
    // of movement before the root accepts the drop. Folder rows
    // don't need this guard — they only fire `drop` after the user
    // actively crossed onto them, so we can trust that signal.
    if (dragStartCoords) {
      const dx = e.clientX - dragStartCoords.x;
      const dy = e.clientY - dragStartCoords.y;
      if (dx * dx + dy * dy < ROOT_DROP_MIN_MOVE_PX_SQ) return;
    }
    try {
      const newPath = await moveProjectPath(ctx.projectPath, src, "");
      ctx.invalidateDir("");
      ctx.invalidateDir(parentSubPath(src));
      ctx.onPathRenamed?.(src, newPath);
    } catch (err) {
      console.error("[file-tree] move to root failed", err);
    }
  };

  return (
    <div
      onDragEnter={onDragEnter}
      onDragOver={onDragOver}
      onDragLeave={onDragLeave}
      onDrop={onDrop}
      className={cn(
        "min-h-full",
        dragOver && "bg-primary/10 outline-1 -outline-offset-2 outline-primary/40",
      )}
    >
      {children}
    </div>
  );
}

interface DirectoryChildrenProps {
  /** Forward-slash project-relative path of the parent directory;
   *  empty string means the project root. */
  parentPath: string;
  depth: number;
}

// Fetches ONE directory's immediate children and renders each as
// either a collapsible folder row or a file row. Ignored entries get
// a dimmer text color so users can tell at a glance what's covered
// by a gitignore rule without having to open it.
function DirectoryChildren({ parentPath, depth }: DirectoryChildrenProps) {
  const ctx = useTreeCtx();
  // `enabled` is always true for depth 0 (the root fetches once on
  // mount) and for any expanded folder (the query fires when the
  // user opens it). Closed folders never fire the query, so heavy
  // dirs like node_modules/ stay completely un-walked.
  const enabled = depth === 0 || ctx.expanded.has(parentPath);
  const { data, isLoading, error } = useQuery({
    ...directoryQueryOptions(ctx.projectPath, parentPath),
    enabled,
  });

  if (!enabled) return null;

  if (error) {
    return (
      <li
        role="treeitem"
        className="py-0.5 pr-2 text-[11px] text-destructive"
        style={{ paddingLeft: 6 + depth * 12 }}
      >
        {String(error)}
      </li>
    );
  }

  if (isLoading && !data) {
    return (
      <li
        role="treeitem"
        className="py-0.5 pr-2 text-[11px] text-muted-foreground/60"
        style={{ paddingLeft: 6 + depth * 12 }}
      >
        …
      </li>
    );
  }

  const entries = data ?? [];
  // Show "no files" placeholder only when there's no in-flight
  // create-row at this level either; otherwise the placeholder is
  // already rendered alongside the (empty) entry list.
  const hasCreateChild =
    ctx.editing?.kind === "create" && ctx.editing.parentDir === parentPath;
  if (entries.length === 0 && !hasCreateChild) {
    if (depth === 0) {
      return (
        <li
          role="treeitem"
          className="px-3 py-4 text-center text-[11px] text-muted-foreground"
        >
          No files in project.
        </li>
      );
    }
    return null;
  }

  return (
    <>
      {entries.map((entry) => (
        <TreeRow
          key={entry.name}
          entry={entry}
          parentPath={parentPath}
          depth={depth}
        />
      ))}
    </>
  );
}

interface TreeRowProps {
  entry: DirEntry;
  parentPath: string;
  depth: number;
}

const TreeRow = React.memo(function TreeRow({
  entry,
  parentPath,
  depth,
}: TreeRowProps) {
  const ctx = useTreeCtx();
  const fullPath = parentPath ? `${parentPath}/${entry.name}` : entry.name;
  const isOpen = entry.isDir && ctx.expanded.has(fullPath);
  const isSelected = !entry.isDir && fullPath === ctx.selectedPath;
  const queryClient = useQueryClient();

  const isRenaming =
    ctx.editing?.kind === "rename" && ctx.editing.path === fullPath;
  const showCreateChild =
    entry.isDir &&
    ctx.editing?.kind === "create" &&
    ctx.editing.parentDir === fullPath;

  // Tailwind's pl-{n} doesn't go fine-grained enough for arbitrary
  // depths, and inline padding-left is the cleanest cross-browser
  // way to do indentation that scales linearly with depth.
  const paddingLeft = 6 + depth * 12;

  // Hover-prefetch a folder's children so the expand click feels
  // instant even on a cold cache. Same React-Query staleTime infinite
  // contract as the click path — if the query has resolved before,
  // this is a no-op.
  const handleFolderHover = React.useCallback(() => {
    if (!entry.isDir) return;
    void queryClient.prefetchQuery(
      directoryQueryOptions(ctx.projectPath, fullPath),
    );
  }, [entry.isDir, fullPath, ctx.projectPath, queryClient]);

  // ─── Drag source ──────────────────────────────────────────────
  // Every row drags as itself. We stash both a typed payload (so a
  // drop target *outside* this widget can read the path on drop) and
  // a module-scoped path mirror (so `dragover` handlers can read it
  // without waiting for the security-gated `getData` and without
  // depending on `dataTransfer.types` exposing custom MIMEs — WebKit
  // historically omits them).
  const onDragStart = (e: React.DragEvent) => {
    if (isRenaming) return;
    e.stopPropagation();
    draggedSourcePath = fullPath;
    // Record the pointer's start coords so the root drop zone can
    // tell "user dragged across the tree" from "user clicked and
    // released without moving" (the latter must be a no-op).
    dragStartCoords = { x: e.clientX, y: e.clientY };
    e.dataTransfer.effectAllowed = "move";
    try {
      e.dataTransfer.setData(TREE_DRAG_MIME, fullPath);
    } catch {
      // Some webviews refuse custom MIMEs; the module-scope mirror
      // is the source of truth anyway.
    }
    // text/plain fallback so dropping outside the app inserts a
    // sensible string rather than nothing.
    e.dataTransfer.setData("text/plain", fullPath);
  };
  const onDragEnd = () => {
    draggedSourcePath = null;
    dragStartCoords = null;
  };

  // ─── Drop target (folders only) ───────────────────────────────
  // Counter pattern avoids flicker as the cursor crosses children.
  // We don't gate on `dataTransfer.types.includes(TREE_DRAG_MIME)`
  // because WebKit may not surface custom MIMEs in `types` during
  // dragover — `draggedSourcePath` non-null already tells us this is
  // one of *our* drags (Finder drops never set it).
  const [dragOver, setDragOver] = React.useState(false);
  const dragDepth = React.useRef(0);
  const targetDir = entry.isDir ? fullPath : null;

  const onDragEnter = (e: React.DragEvent) => {
    // File rows: accept the dragover so the drop event fires HERE
    // instead of bubbling up to RootDropZone (which would otherwise
    // treat any drop near the source as "move to root"). The drop
    // handler below treats file-row drops as no-ops + stopPropagation.
    if (!targetDir) {
      if (!draggedSourcePath) return;
      e.preventDefault();
      e.stopPropagation();
      return;
    }
    if (!draggedSourcePath || !canDropInto(draggedSourcePath, targetDir)) {
      return;
    }
    e.preventDefault();
    // Stop the enter event from bubbling to RootDropZone — otherwise
    // both this folder row AND the root highlight light up at once,
    // making it ambiguous where the drop will actually land.
    e.stopPropagation();
    dragDepth.current += 1;
    setDragOver(true);
  };
  const onDragOver = (e: React.DragEvent) => {
    if (!targetDir) {
      if (!draggedSourcePath) return;
      // Same swallow contract as onDragEnter: keep the drop here so
      // it doesn't bubble to root.
      e.preventDefault();
      e.stopPropagation();
      e.dataTransfer.dropEffect = "none";
      return;
    }
    if (!draggedSourcePath || !canDropInto(draggedSourcePath, targetDir)) {
      return;
    }
    e.preventDefault();
    e.stopPropagation();
    e.dataTransfer.dropEffect = "move";
  };
  const onDragLeave = (e: React.DragEvent) => {
    if (!targetDir) return;
    e.stopPropagation();
    dragDepth.current = Math.max(0, dragDepth.current - 1);
    if (dragDepth.current === 0) setDragOver(false);
  };
  const onDrop = async (e: React.DragEvent) => {
    // Always swallow drops landing on a row — even non-folder rows —
    // so RootDropZone never sees a drop it shouldn't act on. Without
    // this swallow, dropping back onto the source row bubbles up and
    // (for nested files) silently moves the file to the root.
    e.preventDefault();
    e.stopPropagation();
    if (!targetDir) {
      // File row: nothing to do.
      return;
    }
    dragDepth.current = 0;
    setDragOver(false);
    const src =
      draggedSourcePath ?? e.dataTransfer.getData(TREE_DRAG_MIME) ?? "";
    if (!src || !canDropInto(src, targetDir)) return;
    // Auto-expand the destination so the user sees the moved entry
    // appear in its new home.
    ctx.ensureExpanded(targetDir);
    try {
      const newPath = await moveProjectPath(ctx.projectPath, src, targetDir);
      ctx.invalidateDir(targetDir);
      ctx.invalidateDir(parentSubPath(src));
      ctx.onPathRenamed?.(src, newPath);
    } catch (err) {
      console.error("[file-tree] move failed", err);
    }
  };

  const onClick = () => {
    if (isRenaming) return;
    if (entry.isDir) {
      ctx.toggle(fullPath);
    } else {
      ctx.onSelect(fullPath);
    }
  };

  const onKeyDown = (e: React.KeyboardEvent<HTMLDivElement>) => {
    if (isRenaming) return;
    if (e.key === "Enter" || e.key === " ") {
      e.preventDefault();
      onClick();
    }
  };

  return (
    <li role="treeitem" aria-expanded={entry.isDir ? isOpen : undefined}>
      <ContextMenu>
        <ContextMenuTrigger asChild>
          <div
            // The row is a `<div role="button">` rather than a
            // `<button>` — WebKit (Tauri's WKWebView on macOS) does
            // not reliably initiate HTML5 drag-and-drop from a native
            // `<button>`. Switching to a div with `role="button"` plus
            // an Enter/Space keydown gives the same a11y story while
            // restoring drag.
            role="button"
            tabIndex={isRenaming ? -1 : 0}
            onClick={onClick}
            onKeyDown={onKeyDown}
            onMouseEnter={entry.isDir ? handleFolderHover : undefined}
            onFocus={entry.isDir ? handleFolderHover : undefined}
            draggable={!isRenaming}
            onDragStart={onDragStart}
            onDragEnd={onDragEnd}
            onDragEnter={onDragEnter}
            onDragOver={onDragOver}
            onDragLeave={onDragLeave}
            onDrop={onDrop}
            className={cn(
              "flex w-full items-center gap-1 py-0.5 pr-2 text-left text-[11px] cursor-default",
              isSelected
                ? "bg-muted text-foreground"
                : entry.isIgnored
                  ? "text-muted-foreground/50 hover:bg-muted/40 hover:text-muted-foreground"
                  : "text-muted-foreground hover:bg-muted/40 hover:text-foreground",
              dragOver &&
                "bg-primary/15 outline outline-1 -outline-offset-1 outline-primary/50",
            )}
            style={{ paddingLeft }}
            title={entry.isIgnored ? `${fullPath} (gitignored)` : fullPath}
          >
            {entry.isDir ? (
              isOpen ? (
                <ChevronDown className="h-3 w-3 shrink-0" />
              ) : (
                <ChevronRight className="h-3 w-3 shrink-0" />
              )
            ) : (
              // Width-matched spacer so file rows align with folder
              // rows below their chevron.
              <span className="inline-block h-3 w-3 shrink-0" />
            )}
            {entry.isDir ? (
              <Folder className="h-3 w-3 shrink-0" />
            ) : (
              <FileText className="h-3 w-3 shrink-0" />
            )}
            {isRenaming ? (
              <RenameInput
                seed={ctx.editing!.kind === "rename" ? ctx.editing!.seed : ""}
                subPath={fullPath}
              />
            ) : (
              <span className="truncate font-mono">{entry.name}</span>
            )}
          </div>
        </ContextMenuTrigger>
        <RowContextMenu entry={entry} fullPath={fullPath} />
      </ContextMenu>
      {entry.isDir && isOpen && (
        <ul role="group">
          {showCreateChild ? (
            <CreatePlaceholder parentDir={fullPath} indentDepth={depth + 1} />
          ) : null}
          <DirectoryChildren parentPath={fullPath} depth={depth + 1} />
        </ul>
      )}
    </li>
  );
});

/**
 * Per-row context menu. Branches on `entry.isDir` so we don't show
 * "New file" on a leaf or "Open" on a directory.
 */
function RowContextMenu({
  entry,
  fullPath,
}: {
  entry: DirEntry;
  fullPath: string;
}) {
  const ctx = useTreeCtx();
  const isDir = entry.isDir;

  const onRename = () =>
    ctx.setEditing({ kind: "rename", path: fullPath, seed: entry.name });

  const onDelete = async () => {
    try {
      await trashProjectPath(ctx.projectPath, fullPath);
      ctx.invalidateDir(parentSubPath(fullPath));
      ctx.onPathRemoved?.(fullPath);
    } catch (err) {
      console.error("[file-tree] trash failed", err);
    }
  };

  // Absolute path for "Copy path" — the project root joined with the
  // sub-path. We don't know which OS separator was used in the
  // project root string the caller passed in; just join with `/`
  // since that's what every macOS / Linux path uses, and Windows'
  // file dialogs accept forward slashes too.
  const absolutePath = (): string => {
    const root = ctx.projectPath.replace(/\/$/, "");
    return `${root}/${fullPath}`;
  };

  return (
    <ContextMenuContent
      // Radix restores focus to the trigger element by default when
      // the menu closes; that fires *after* our inline input mounts
      // and steals focus away. Cancel the restore so the input keeps
      // focus.
      onCloseAutoFocus={(e) => e.preventDefault()}
    >
      {isDir ? (
        <>
          <ContextMenuItem
            onSelect={() => {
              ctx.ensureExpanded(fullPath);
              ctx.setEditing({
                kind: "create",
                parentDir: fullPath,
                childKind: "file",
              });
            }}
          >
            <FilePlus />
            <span>New file</span>
          </ContextMenuItem>
          <ContextMenuItem
            onSelect={() => {
              ctx.ensureExpanded(fullPath);
              ctx.setEditing({
                kind: "create",
                parentDir: fullPath,
                childKind: "folder",
              });
            }}
          >
            <FolderPlus />
            <span>New folder</span>
          </ContextMenuItem>
          <ContextMenuSeparator />
        </>
      ) : null}
      <ContextMenuItem onSelect={onRename}>
        <Pencil />
        <span>Rename…</span>
      </ContextMenuItem>
      <ContextMenuSeparator />
      <ContextMenuItem onSelect={() => void copyToClipboard(absolutePath())}>
        <Copy />
        <span>Copy path</span>
      </ContextMenuItem>
      <ContextMenuItem onSelect={() => void copyToClipboard(fullPath)}>
        <Copy />
        <span>Copy relative path</span>
      </ContextMenuItem>
      <ContextMenuSeparator />
      <ContextMenuItem variant="destructive" onSelect={onDelete}>
        <Trash2 />
        <span>Move to trash</span>
      </ContextMenuItem>
    </ContextMenuContent>
  );
}

/**
 * Inline rename input. Mounts focused with the seed pre-selected so
 * the user can either type a fresh name or tweak the existing one.
 */
function RenameInput({ seed, subPath }: { seed: string; subPath: string }) {
  const ctx = useTreeCtx();
  const ref = React.useRef<HTMLInputElement | null>(null);
  const [value, setValue] = React.useState(seed);
  const committedRef = React.useRef(false);

  React.useEffect(() => {
    // Schedule both an immediate attempt AND a deferred one — the
    // immediate call usually wins, and the `setTimeout(0)` covers
    // the case where Radix's portal teardown happens after our mount
    // and would otherwise steal focus back.
    ref.current?.focus();
    ref.current?.select();
    const id = window.setTimeout(() => {
      ref.current?.focus();
      ref.current?.select();
    }, 0);
    return () => window.clearTimeout(id);
  }, []);

  const commit = async () => {
    if (committedRef.current) return;
    committedRef.current = true;
    const trimmed = value.trim();
    if (!trimmed || trimmed === seed) {
      ctx.setEditing(null);
      return;
    }
    try {
      const newPath = await renameProjectPath(
        ctx.projectPath,
        subPath,
        trimmed,
      );
      ctx.invalidateDir(parentSubPath(subPath));
      ctx.onPathRenamed?.(subPath, newPath);
    } catch (err) {
      console.error("[file-tree] rename failed", err);
    }
    ctx.setEditing(null);
  };

  const cancel = () => {
    if (committedRef.current) return;
    committedRef.current = true;
    ctx.setEditing(null);
  };

  return (
    <input
      ref={ref}
      type="text"
      value={value}
      onChange={(e) => setValue(e.target.value)}
      onKeyDown={(e) => {
        if (e.key === "Enter") {
          e.preventDefault();
          void commit();
        } else if (e.key === "Escape") {
          e.preventDefault();
          cancel();
        }
      }}
      onBlur={() => void commit()}
      onClick={(e) => e.stopPropagation()}
      className="flex-1 truncate rounded-sm border border-primary/40 bg-background px-1 font-mono text-[11px] outline-none focus:border-primary"
    />
  );
}

/**
 * Placeholder row for "New file" / "New folder". Renders an input
 * inline at the requested indent level. Same Enter/Esc UX as
 * `RenameInput`; on commit invokes `createProjectFile` /
 * `createProjectDir` and (for files) opens the new file in the editor.
 */
function CreatePlaceholder({
  parentDir,
  indentDepth,
}: {
  parentDir: string;
  indentDepth: number;
}) {
  const ctx = useTreeCtx();
  const childKind =
    ctx.editing?.kind === "create" ? ctx.editing.childKind : "file";
  const indent = 6 + indentDepth * 12;
  const ref = React.useRef<HTMLInputElement | null>(null);
  const [value, setValue] = React.useState("");
  const committedRef = React.useRef(false);

  React.useEffect(() => {
    // Same belt-and-suspenders as `RenameInput`: focus immediately
    // *and* via a 0ms timeout so we win the race against Radix's
    // close-time focus restoration.
    ref.current?.focus();
    const id = window.setTimeout(() => ref.current?.focus(), 0);
    return () => window.clearTimeout(id);
  }, []);

  const commit = async () => {
    if (committedRef.current) return;
    committedRef.current = true;
    const trimmed = value.trim();
    if (!trimmed) {
      ctx.setEditing(null);
      return;
    }
    let createdPath: string | null = null;
    try {
      if (childKind === "file") {
        createdPath = await createProjectFile(
          ctx.projectPath,
          parentDir,
          trimmed,
        );
      } else {
        createdPath = await createProjectDir(
          ctx.projectPath,
          parentDir,
          trimmed,
        );
      }
      ctx.invalidateDir(parentDir);
    } catch (err) {
      console.error("[file-tree] create failed", err);
    }
    ctx.setEditing(null);
    // Newly created files: open them straight away. Folders: leave
    // the cursor wherever it was.
    if (createdPath && childKind === "file") {
      ctx.onSelect(createdPath);
    }
  };

  const cancel = () => {
    if (committedRef.current) return;
    committedRef.current = true;
    ctx.setEditing(null);
  };

  return (
    <li
      style={{ paddingLeft: indent }}
      className="flex items-center gap-1 py-0.5 pr-2 text-[11px]"
    >
      <span className="inline-block h-3 w-3 shrink-0" />
      {childKind === "file" ? (
        <FilePlus className="h-3 w-3 shrink-0 text-muted-foreground" />
      ) : (
        <FolderPlus className="h-3 w-3 shrink-0 text-muted-foreground" />
      )}
      <input
        ref={ref}
        type="text"
        value={value}
        placeholder={childKind === "file" ? "Untitled.txt" : "new-folder"}
        onChange={(e) => setValue(e.target.value)}
        onKeyDown={(e) => {
          if (e.key === "Enter") {
            e.preventDefault();
            void commit();
          } else if (e.key === "Escape") {
            e.preventDefault();
            cancel();
          }
        }}
        onBlur={() => void commit()}
        onClick={(e) => e.stopPropagation()}
        className="flex-1 truncate rounded-sm border border-primary/40 bg-background px-1 font-mono text-[11px] outline-none focus:border-primary"
      />
    </li>
  );
}

/**
 * Best-effort write to the system clipboard. Used by the "Copy path"
 * / "Copy relative path" context-menu items. Failures are logged and
 * swallowed — the operation is purely user-affordance.
 */
async function copyToClipboard(text: string): Promise<void> {
  try {
    await navigator.clipboard.writeText(text);
  } catch (err) {
    console.warn("[file-tree] clipboard write failed", err);
  }
}
