import * as React from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { ChevronDown, ChevronRight, FileText, Folder } from "lucide-react";
import { directoryQueryOptions } from "@/lib/queries";
import type { DirEntry } from "@/lib/api";

// Lazy, per-directory file tree. Each folder fetches its immediate
// children on first expansion via the `list_directory` Tauri command,
// which INCLUDES gitignored entries (flagged as dimmed). This means
// heavy directories like `node_modules/` and `dist/` show up at the
// top level but never get walked until the user explicitly opens
// them.
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
// reopening a folder is instant and no extra round-trip fires.

interface FileTreeProps {
  /** Project root. Required — the tree renders nothing without it. */
  projectPath: string | null;
  /** Currently-focused file's project-relative path, for highlighting
   *  and auto-expanding parents when the selection changes. */
  selectedPath: string | null;
  /** Fired when the user clicks a file row. Parent owns open-in-tab
   *  + url routing behavior; this component is purely presentational. */
  onSelect: (path: string) => void;
}

export function FileTree({
  projectPath,
  selectedPath,
  onSelect,
}: FileTreeProps) {
  // Which folder paths are currently expanded. Keyed by forward-slash
  // full project-relative path. Survives tree re-renders.
  const [expanded, setExpanded] = React.useState<Set<string>>(
    () => new Set<string>(),
  );

  const toggle = React.useCallback((path: string) => {
    setExpanded((prev) => {
      const next = new Set(prev);
      if (next.has(path)) next.delete(path);
      else next.add(path);
      return next;
    });
  }, []);

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

  return (
    <ul role="tree" className="py-1">
      <DirectoryChildren
        projectPath={projectPath}
        parentPath=""
        depth={0}
        expanded={expanded}
        onToggle={toggle}
        selectedPath={selectedPath}
        onSelect={onSelect}
      />
    </ul>
  );
}

interface DirectoryChildrenProps {
  projectPath: string;
  /** Forward-slash project-relative path of the parent directory;
   *  empty string means the project root. */
  parentPath: string;
  depth: number;
  expanded: Set<string>;
  onToggle: (path: string) => void;
  selectedPath: string | null;
  onSelect: (path: string) => void;
}

// Fetches ONE directory's immediate children and renders each as
// either a collapsible folder row or a file row. Ignored entries get
// a dimmer text color so users can tell at a glance what's covered
// by a gitignore rule without having to open it.
function DirectoryChildren({
  projectPath,
  parentPath,
  depth,
  expanded,
  onToggle,
  selectedPath,
  onSelect,
}: DirectoryChildrenProps) {
  // `enabled` is always true for depth 0 (the root fetches once on
  // mount) and for any expanded folder (the query fires when the
  // user opens it). Closed folders never fire the query, so heavy
  // dirs like node_modules/ stay completely un-walked.
  const enabled = depth === 0 || expanded.has(parentPath);
  const { data, isLoading, error } = useQuery({
    ...directoryQueryOptions(projectPath, parentPath),
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
  if (entries.length === 0) {
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
          projectPath={projectPath}
          parentPath={parentPath}
          depth={depth}
          expanded={expanded}
          onToggle={onToggle}
          selectedPath={selectedPath}
          onSelect={onSelect}
        />
      ))}
    </>
  );
}

interface TreeRowProps {
  entry: DirEntry;
  projectPath: string;
  parentPath: string;
  depth: number;
  expanded: Set<string>;
  onToggle: (path: string) => void;
  selectedPath: string | null;
  onSelect: (path: string) => void;
}

const TreeRow = React.memo(function TreeRow({
  entry,
  projectPath,
  parentPath,
  depth,
  expanded,
  onToggle,
  selectedPath,
  onSelect,
}: TreeRowProps) {
  const fullPath = parentPath ? `${parentPath}/${entry.name}` : entry.name;
  const isOpen = entry.isDir && expanded.has(fullPath);
  const isSelected = !entry.isDir && fullPath === selectedPath;
  const queryClient = useQueryClient();

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
      directoryQueryOptions(projectPath, fullPath),
    );
  }, [entry.isDir, fullPath, projectPath, queryClient]);

  return (
    <li role="treeitem" aria-expanded={entry.isDir ? isOpen : undefined}>
      <button
        type="button"
        onClick={() =>
          entry.isDir ? onToggle(fullPath) : onSelect(fullPath)
        }
        onMouseEnter={entry.isDir ? handleFolderHover : undefined}
        onFocus={entry.isDir ? handleFolderHover : undefined}
        className={
          "flex w-full items-center gap-1 py-0.5 pr-2 text-left text-[11px] " +
          (isSelected
            ? "bg-muted text-foreground"
            : entry.isIgnored
              ? "text-muted-foreground/50 hover:bg-muted/40 hover:text-muted-foreground"
              : "text-muted-foreground hover:bg-muted/40 hover:text-foreground")
        }
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
        <span className="truncate font-mono">{entry.name}</span>
      </button>
      {entry.isDir && isOpen && (
        <ul role="group">
          <DirectoryChildren
            projectPath={projectPath}
            parentPath={fullPath}
            depth={depth + 1}
            expanded={expanded}
            onToggle={onToggle}
            selectedPath={selectedPath}
            onSelect={onSelect}
          />
        </ul>
      )}
    </li>
  );
});
