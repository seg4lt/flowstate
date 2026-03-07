import * as React from "react";
import { ChevronDown, ChevronRight, FileText, Folder } from "lucide-react";

// Build a directory tree from the flat forward-slash relative path
// list returned by the `list_project_files` Tauri command, then
// render it as a collapsible nested list. Click a file to open it,
// click a folder header (or its chevron) to expand/collapse.
//
// Folder children are sorted folders-first then alphabetically,
// matching what users expect from VS Code / Finder. Nodes are
// React.memo'd so chat-side re-renders or unrelated state changes
// don't recompute every row.

interface TreeNode {
  /** Forward-slash full project-relative path (e.g. "src/foo.ts"). */
  path: string;
  /** Display name (basename for files, segment for folders). */
  name: string;
  /** Folder children (only set on directory nodes). */
  children?: TreeNode[];
}

interface FileTreeProps {
  files: string[];
  selectedPath: string | null;
  onSelect: (path: string) => void;
}

export function FileTree({ files, selectedPath, onSelect }: FileTreeProps) {
  // Rebuild the tree only when the file list changes — not on every
  // selection or chat tick. The construction is O(N) over paths,
  // cheap enough that we don't need persistent caching.
  const root = React.useMemo(() => buildTree(files), [files]);

  // Track which directories are expanded. Default: top-level folders
  // expanded so the user sees something on first open. Drilling
  // deeper requires a click. Keyed by full directory path so state
  // survives tree rebuilds (e.g. after a file refresh).
  const [expanded, setExpanded] = React.useState<Set<string>>(() => {
    const initial = new Set<string>();
    for (const child of root.children ?? []) {
      if (child.children) initial.add(child.path);
    }
    return initial;
  });

  const toggle = React.useCallback((path: string) => {
    setExpanded((prev) => {
      const next = new Set(prev);
      if (next.has(path)) next.delete(path);
      else next.add(path);
      return next;
    });
  }, []);

  // When the user picks a file from search (not the tree), expand
  // every parent directory so the row is visible if they then look
  // at the tree. Cheap: walks the path components once.
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

  if (!root.children || root.children.length === 0) {
    return (
      <div className="px-3 py-4 text-center text-[11px] text-muted-foreground">
        No files in project.
      </div>
    );
  }

  return (
    <ul role="tree" className="py-1">
      {root.children.map((child) => (
        <TreeRow
          key={child.path}
          node={child}
          depth={0}
          expanded={expanded}
          onToggle={toggle}
          selectedPath={selectedPath}
          onSelect={onSelect}
        />
      ))}
    </ul>
  );
}

interface TreeRowProps {
  node: TreeNode;
  depth: number;
  expanded: Set<string>;
  onToggle: (path: string) => void;
  selectedPath: string | null;
  onSelect: (path: string) => void;
}

const TreeRow = React.memo(function TreeRow({
  node,
  depth,
  expanded,
  onToggle,
  selectedPath,
  onSelect,
}: TreeRowProps) {
  const isFolder = node.children !== undefined;
  const isOpen = isFolder && expanded.has(node.path);
  const isSelected = !isFolder && node.path === selectedPath;

  // Tailwind's pl-{n} doesn't go fine-grained enough for arbitrary
  // depths, and inline padding-left is the cleanest cross-browser
  // way to do indentation that scales linearly with depth.
  const paddingLeft = 6 + depth * 12;

  return (
    <li role="treeitem" aria-expanded={isFolder ? isOpen : undefined}>
      <button
        type="button"
        onClick={() => (isFolder ? onToggle(node.path) : onSelect(node.path))}
        className={
          "flex w-full items-center gap-1 py-0.5 pr-2 text-left text-[11px] " +
          (isSelected
            ? "bg-muted text-foreground"
            : "text-muted-foreground hover:bg-muted/40 hover:text-foreground")
        }
        style={{ paddingLeft }}
        title={node.path}
      >
        {isFolder ? (
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
        {isFolder ? (
          <Folder className="h-3 w-3 shrink-0" />
        ) : (
          <FileText className="h-3 w-3 shrink-0" />
        )}
        <span className="truncate font-mono">{node.name}</span>
      </button>
      {isFolder && isOpen && node.children && (
        <ul role="group">
          {node.children.map((child) => (
            <TreeRow
              key={child.path}
              node={child}
              depth={depth + 1}
              expanded={expanded}
              onToggle={onToggle}
              selectedPath={selectedPath}
              onSelect={onSelect}
            />
          ))}
        </ul>
      )}
    </li>
  );
});

// Walk a flat sorted list of forward-slash paths and produce a
// nested tree. Paths are inserted in order; intermediate folder
// nodes are created on first encounter and reused on subsequent
// inserts. After insertion, every folder's children are sorted
// folders-first then alphabetically.
function buildTree(files: string[]): TreeNode {
  const root: TreeNode = { path: "", name: "", children: [] };

  for (const file of files) {
    const segments = file.split("/");
    let cursor = root;
    let prefix = "";
    for (let i = 0; i < segments.length; i++) {
      const segment = segments[i];
      prefix = prefix ? `${prefix}/${segment}` : segment;
      const isLast = i === segments.length - 1;
      if (!cursor.children) cursor.children = [];
      // Linear scan is fine — directories rarely have more than a
      // few hundred direct children in practice and we visit each
      // file once. If this ever becomes a bottleneck, swap for a
      // Map<segment, TreeNode> per cursor.
      let child = cursor.children.find((c) => c.name === segment);
      if (!child) {
        child = isLast
          ? { path: prefix, name: segment }
          : { path: prefix, name: segment, children: [] };
        cursor.children.push(child);
      }
      if (!isLast) cursor = child;
    }
  }

  sortTree(root);
  return root;
}

function sortTree(node: TreeNode) {
  if (!node.children) return;
  node.children.sort((a, b) => {
    const aIsDir = a.children !== undefined;
    const bIsDir = b.children !== undefined;
    if (aIsDir !== bIsDir) return aIsDir ? -1 : 1;
    return a.name.localeCompare(b.name);
  });
  for (const child of node.children) sortTree(child);
}
