import * as React from "react";
import { ChevronDown, ChevronRight, Folder } from "lucide-react";
import { useStreamedGitDiffSummary } from "@/lib/git-diff-stream";
import type { GitFileStatus, GitFileSummary } from "@/lib/api";

// Tree of files changed in the working tree (vs HEAD). Used by
// CodeView when "Git mode" is on as a replacement for the project
// tree. Reuses `useStreamedGitDiffSummary` (the same hook the diff
// panel uses) — Phase 1 (`git status`) lands near-instantly with the
// file list, Phase 2 (`numstat`) hydrates per-file +N/-M counts
// progressively. Cleanup kills the git subprocess via the hook's
// internal `sub.stop()`.
//
// Tree shape:
//   * Files are grouped by their directory components. Folder rows
//     act as collapse toggles.
//   * VS Code-style "compact folders": when a directory contains a
//     SINGLE child directory and no files, the two are merged into
//     one row (`a/b/c` instead of three nested entries). This keeps
//     deep paths like `apps/flowstate/src/components/code/foo.ts`
//     readable instead of forcing five nested rows for one file.
//
// Performance: the tree is rebuilt on every diff event, but the
// stream batches numstat updates via rAF in the hook itself, so the
// tree rebuild happens at most once per frame. For O(N) changed
// files the rebuild is linear in N.

const STATUS_LABEL: Record<GitFileStatus, string> = {
  modified: "M",
  added: "A",
  deleted: "D",
  renamed: "R",
  copied: "C",
};

// Tailwind colour per status. Kept here rather than inline so the
// row JSX stays readable. Picks readable contrasts in both themes.
const STATUS_COLOR: Record<GitFileStatus, string> = {
  modified: "text-amber-500",
  added: "text-emerald-500",
  deleted: "text-rose-500",
  renamed: "text-sky-500",
  copied: "text-sky-500",
};

interface ChangedFilesListProps {
  projectPath: string | null;
  selectedPath: string | null;
  onSelect: (path: string) => void;
  /** Bumping this restarts the underlying git diff subscription so
   *  newly-staged / newly-changed files appear without toggling git
   *  mode off and back on. Wired to the file-tree header's refresh
   *  button. */
  refreshTick?: number;
}

// ── tree model ──────────────────────────────────────────────────

type TreeNode = DirNode | FileNode;

interface DirNode {
  kind: "dir";
  /** Display name. After compaction, may contain `/` (e.g. `src/lib`). */
  name: string;
  /** Forward-slash project-relative path of this directory. Used as
   *  the React key + the expanded-set key. */
  fullPath: string;
  children: TreeNode[];
}

interface FileNode {
  kind: "file";
  /** Display name (the basename only — directory components were
   *  consumed by ancestor DirNodes). */
  name: string;
  /** Forward-slash project-relative full path. */
  fullPath: string;
  entry: GitFileSummary;
}

/**
 * Build a (compacted) tree of changed files from a flat diff list.
 *
 * Algorithm:
 *  1. Insert each file into a trie keyed on path segments.
 *  2. Compact: walk the tree depth-first; whenever a dir has exactly
 *     one child *and that child is also a dir*, merge them by
 *     joining names with `/` and replacing the children. Repeat
 *     until stable. Files block compaction (a dir holding only a
 *     single file stays as a folder row containing that file —
 *     rolling the file up would lose the click target hierarchy).
 *  3. Sort: dirs before files at each level, alphabetically within
 *     each group. Matches FileTree's behaviour.
 */
function buildCompactedTree(diffs: GitFileSummary[]): TreeNode[] {
  // Mutable build form keeps children indexed by name for O(1)
  // insertion. We convert to the final array shape after compaction.
  interface BuildDir {
    kind: "dir";
    name: string;
    fullPath: string;
    childMap: Map<string, BuildDir | FileNode>;
  }
  const root: BuildDir = {
    kind: "dir",
    name: "",
    fullPath: "",
    childMap: new Map(),
  };

  for (const entry of diffs) {
    const segments = entry.path.split("/").filter((s) => s.length > 0);
    if (segments.length === 0) continue;
    let cursor: BuildDir = root;
    for (let i = 0; i < segments.length - 1; i++) {
      const seg = segments[i]!;
      const existing = cursor.childMap.get(seg);
      if (existing && existing.kind === "dir") {
        cursor = existing;
        continue;
      }
      // Either no entry yet, or a file with the same name as a
      // future directory — which can't happen in a real filesystem
      // but guard against it by overwriting (the tree wins).
      const next: BuildDir = {
        kind: "dir",
        name: seg,
        fullPath: cursor.fullPath ? `${cursor.fullPath}/${seg}` : seg,
        childMap: new Map(),
      };
      cursor.childMap.set(seg, next);
      cursor = next;
    }
    const fileName = segments[segments.length - 1]!;
    const fileNode: FileNode = {
      kind: "file",
      name: fileName,
      fullPath: entry.path,
      entry,
    };
    cursor.childMap.set(fileName, fileNode);
  }

  // Convert + compact in one recursive pass.
  function finalize(d: BuildDir): TreeNode[] {
    const out: TreeNode[] = [];
    for (const child of d.childMap.values()) {
      if (child.kind === "file") {
        out.push(child);
        continue;
      }
      // Recurse first so the child's own compaction is done before
      // we decide whether to fold it into us.
      const childChildren = finalize(child);
      let merged: DirNode = {
        kind: "dir",
        name: child.name,
        fullPath: child.fullPath,
        children: childChildren,
      };
      // Compact while the chain holds: exactly one child, and it's
      // a dir. Files block the merge.
      while (
        merged.children.length === 1 &&
        merged.children[0]!.kind === "dir"
      ) {
        const only = merged.children[0] as DirNode;
        merged = {
          kind: "dir",
          name: `${merged.name}/${only.name}`,
          fullPath: only.fullPath,
          children: only.children,
        };
      }
      out.push(merged);
    }
    // Sort: dirs first, then files; alphabetical within each group.
    out.sort((a, b) => {
      if (a.kind !== b.kind) return a.kind === "dir" ? -1 : 1;
      return a.name.localeCompare(b.name);
    });
    return out;
  }

  return finalize(root);
}

// Walk the tree and collect every directory path so we can default-
// expand them on first render. Keeps the experience close to "flat
// list" by default (everything visible) while still giving the user
// a way to collapse noisy subtrees.
function collectAllDirPaths(nodes: TreeNode[], acc: Set<string>): void {
  for (const n of nodes) {
    if (n.kind !== "dir") continue;
    acc.add(n.fullPath);
    collectAllDirPaths(n.children, acc);
  }
}

export function ChangedFilesList({
  projectPath,
  selectedPath,
  onSelect,
  refreshTick = 0,
}: ChangedFilesListProps) {
  // refreshTick is forwarded into the streaming hook: bumping it
  // tears down the existing git subscription and starts a new one,
  // picking up files added on disk since the panel was first opened.
  // The hook keeps the previous list visible until phase 1 of the
  // restart lands so the tree doesn't flash empty.
  const { diffs, status, error } = useStreamedGitDiffSummary(
    projectPath,
    refreshTick,
    /* enabled */ projectPath !== null,
  );

  // Rebuild the tree from `diffs`. Cheap (linear in N) and the hook
  // already rAF-batches numstat updates upstream, so this fires at
  // most once per frame.
  const tree = React.useMemo(() => buildCompactedTree(diffs), [diffs]);

  // Default-expanded set. Recomputed when the *set of directories*
  // in the tree changes, NOT when numstat counts hydrate — comparing
  // the full path-set on every render would defeat the React.useMemo
  // we just added. Identity comparison on `tree` is the right
  // boundary: a counts-only update keeps `tree` ref-stable because
  // useMemo returns the same array unless `diffs` changed.
  const [collapsed, setCollapsed] = React.useState<Set<string>>(
    () => new Set(),
  );
  // When new directories appear (first stream of a project, or new
  // files added), drop any stale entries in `collapsed` that no
  // longer exist. This keeps `collapsed` from growing unboundedly
  // across many edit sessions.
  React.useEffect(() => {
    const live = new Set<string>();
    collectAllDirPaths(tree, live);
    setCollapsed((prev) => {
      let changed = false;
      const next = new Set<string>();
      for (const p of prev) {
        if (live.has(p)) {
          next.add(p);
        } else {
          changed = true;
        }
      }
      return changed ? next : prev;
    });
  }, [tree]);

  const toggleDir = React.useCallback((fullPath: string) => {
    setCollapsed((prev) => {
      const next = new Set(prev);
      if (next.has(fullPath)) next.delete(fullPath);
      else next.add(fullPath);
      return next;
    });
  }, []);

  if (!projectPath) {
    return (
      <div className="px-3 py-4 text-center text-[11px] text-muted-foreground">
        No project for this session.
      </div>
    );
  }

  if (status === "error" && diffs.length === 0) {
    return (
      <div className="px-3 py-4 text-[11px] text-destructive">
        {error ?? "Failed to read git diff."}
      </div>
    );
  }

  if (status === "streaming" && diffs.length === 0) {
    return (
      <div className="px-3 py-4 text-center text-[11px] text-muted-foreground">
        Scanning…
      </div>
    );
  }

  if (diffs.length === 0) {
    return (
      <div className="px-3 py-4 text-center text-[11px] text-muted-foreground">
        No changes vs HEAD.
      </div>
    );
  }

  return (
    <ul role="tree" className="py-1">
      {tree.map((node) => (
        <TreeRow
          key={node.fullPath}
          node={node}
          depth={0}
          collapsed={collapsed}
          onToggle={toggleDir}
          selectedPath={selectedPath}
          onSelect={onSelect}
        />
      ))}
    </ul>
  );
}

// ── row component ───────────────────────────────────────────────

interface TreeRowProps {
  node: TreeNode;
  depth: number;
  collapsed: Set<string>;
  onToggle: (fullPath: string) => void;
  selectedPath: string | null;
  onSelect: (path: string) => void;
}

function TreeRow({
  node,
  depth,
  collapsed,
  onToggle,
  selectedPath,
  onSelect,
}: TreeRowProps) {
  // Same indentation math as FileTree so the two views feel
  // identical when toggling git mode off and back on.
  const paddingLeft = 6 + depth * 12;

  if (node.kind === "file") {
    const d = node.entry;
    const isSelected = node.fullPath === selectedPath;
    return (
      <li role="treeitem">
        <button
          type="button"
          onClick={() => onSelect(node.fullPath)}
          className={
            "flex w-full items-center gap-2 py-0.5 pr-2 text-left text-[11px] " +
            (isSelected
              ? "bg-muted text-foreground"
              : "text-muted-foreground hover:bg-muted/40 hover:text-foreground")
          }
          style={{ paddingLeft }}
          title={`${node.fullPath} (${d.status}, +${d.additions} -${d.deletions})`}
        >
          {/* Width-matched spacer so file rows align under the dir
              chevron above them, identical to FileTree. */}
          <span className="inline-block h-3 w-3 shrink-0" />
          <span
            className={
              "inline-block w-3 shrink-0 text-center font-mono text-[10px] font-bold " +
              STATUS_COLOR[d.status]
            }
            aria-label={d.status}
          >
            {STATUS_LABEL[d.status]}
          </span>
          <span className="min-w-0 flex-1 truncate font-mono">{node.name}</span>
          {d.additions + d.deletions > 0 && (
            <span className="shrink-0 font-mono text-[10px] tabular-nums">
              <span className="text-emerald-500">+{d.additions}</span>{" "}
              <span className="text-rose-500">-{d.deletions}</span>
            </span>
          )}
        </button>
      </li>
    );
  }

  // Directory row.
  const isOpen = !collapsed.has(node.fullPath);
  return (
    <li role="treeitem" aria-expanded={isOpen}>
      <button
        type="button"
        onClick={() => onToggle(node.fullPath)}
        className="flex w-full items-center gap-1 py-0.5 pr-2 text-left text-[11px] text-muted-foreground hover:bg-muted/40 hover:text-foreground"
        style={{ paddingLeft }}
        title={node.fullPath}
      >
        {isOpen ? (
          <ChevronDown className="h-3 w-3 shrink-0" />
        ) : (
          <ChevronRight className="h-3 w-3 shrink-0" />
        )}
        <Folder className="h-3 w-3 shrink-0" />
        <span className="truncate font-mono">{node.name}</span>
      </button>
      {isOpen && (
        <ul role="group">
          {node.children.map((child) => (
            <TreeRow
              key={child.fullPath}
              node={child}
              depth={depth + 1}
              collapsed={collapsed}
              onToggle={onToggle}
              selectedPath={selectedPath}
              onSelect={onSelect}
            />
          ))}
        </ul>
      )}
    </li>
  );
}
