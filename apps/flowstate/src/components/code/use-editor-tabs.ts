import * as React from "react";

// Editor tabs + split layout state, scoped to a single project path.
//
// Owns the "which files are open in which pane" state and persists
// it per-projectPath to localStorage. The actual file contents live
// in a sibling cache in CodeView — this module only tracks paths
// and UI layout.
//
// Cap per pane: MAX_TABS. Opening an (MAX_TABS + 1)th file evicts
// the least-recently-accessed tab in that pane. Split is at most
// one level deep ("horizontal" = side-by-side, "vertical" =
// top/bottom). When a pane's last tab closes and a split is active
// the split collapses back to a single pane.

export const MAX_TABS_PER_PANE = 10;
export const DEFAULT_SPLIT_RATIO = 0.5;

export type SplitDirection = "horizontal" | "vertical";
export type PaneIndex = 0 | 1;

export interface Tab {
  path: string;
  /** Monotonic counter from openedAt; higher = more recently touched.
   *  Used as the LRU key when we need to evict past MAX_TABS_PER_PANE. */
  lastAccessedAt: number;
  /** True while the editor's buffer differs from disk. Cleared on
   *  successful save (Cmd+S, Vim `:w`, or auto-save on focus-out).
   *  Drives the unsaved-dot in the tab bar and the close-confirm
   *  dialog. NOT persisted — always starts false on app reload. */
  dirty?: boolean;
}

export interface Pane {
  tabs: Tab[];
  activePath: string | null;
}

export interface EditorLayout {
  panes: [Pane] | [Pane, Pane];
  split: SplitDirection | null;
  focusedPaneIndex: PaneIndex;
  /** Fraction of the container given to pane 0 when split !== null.
   *  Clamped to [0.15, 0.85] by the drag handle. */
  splitRatio: number;
}

// ─── persistence ─────────────────────────────────────────────────

const STORAGE_PREFIX = "flowstate:code-tabs:";

function storageKey(projectPath: string): string {
  return STORAGE_PREFIX + projectPath;
}

interface StoredLayout {
  panes: Array<{ tabs: string[]; activePath: string | null }>;
  split: SplitDirection | null;
  focusedPaneIndex: number;
  splitRatio: number;
}

function serialize(layout: EditorLayout): StoredLayout {
  return {
    panes: layout.panes.map((p) => ({
      tabs: p.tabs.map((t) => t.path),
      activePath: p.activePath,
    })),
    split: layout.split,
    focusedPaneIndex: layout.focusedPaneIndex,
    splitRatio: layout.splitRatio,
  };
}

function deserialize(
  raw: StoredLayout,
  validPaths: Set<string>,
): EditorLayout {
  // A single monotonic stamp base is fine here — restored tabs don't
  // need to preserve the original access order across reloads, only
  // the in-memory relative order going forward.
  let stamp = 0;
  const panesRaw = Array.isArray(raw.panes) ? raw.panes.slice(0, 2) : [];
  const panes = panesRaw.map((p): Pane => {
    const paths = (Array.isArray(p.tabs) ? p.tabs : [])
      .filter((path) => typeof path === "string" && validPaths.has(path))
      .slice(0, MAX_TABS_PER_PANE);
    const tabs: Tab[] = paths.map((path) => ({
      path,
      lastAccessedAt: ++stamp,
    }));
    const activePath =
      typeof p.activePath === "string" && paths.includes(p.activePath)
        ? p.activePath
        : (paths[paths.length - 1] ?? null);
    return { tabs, activePath };
  });

  // Drop empty tail panes — a persisted split with no tabs on one
  // side should collapse rather than restore an empty pane.
  while (panes.length > 1 && panes[panes.length - 1]!.tabs.length === 0) {
    panes.pop();
  }
  if (panes.length === 0) {
    return emptyLayout();
  }

  const split: SplitDirection | null =
    panes.length === 2 && (raw.split === "horizontal" || raw.split === "vertical")
      ? raw.split
      : null;

  const focusedPaneIndex: PaneIndex =
    raw.focusedPaneIndex === 1 && panes.length === 2 ? 1 : 0;

  const splitRatio =
    typeof raw.splitRatio === "number" &&
    Number.isFinite(raw.splitRatio) &&
    raw.splitRatio >= 0.15 &&
    raw.splitRatio <= 0.85
      ? raw.splitRatio
      : DEFAULT_SPLIT_RATIO;

  const finalPanes =
    panes.length === 2
      ? ([panes[0]!, panes[1]!] as [Pane, Pane])
      : ([panes[0]!] as [Pane]);

  return {
    panes: finalPanes,
    split: finalPanes.length === 2 ? split : null,
    focusedPaneIndex,
    splitRatio,
  };
}

function emptyLayout(): EditorLayout {
  return {
    panes: [{ tabs: [], activePath: null }],
    split: null,
    focusedPaneIndex: 0,
    splitRatio: DEFAULT_SPLIT_RATIO,
  };
}

function loadLayout(
  projectPath: string | null,
  validPaths: Set<string>,
): EditorLayout {
  if (!projectPath) return emptyLayout();
  try {
    const raw = window.localStorage.getItem(storageKey(projectPath));
    if (!raw) return emptyLayout();
    const parsed = JSON.parse(raw) as StoredLayout;
    return deserialize(parsed, validPaths);
  } catch {
    return emptyLayout();
  }
}

// ─── reducer ─────────────────────────────────────────────────────

type Action =
  | { type: "openFile"; pane: PaneIndex; path: string }
  | { type: "closeTab"; pane: PaneIndex; path: string }
  | { type: "closeOtherTabs"; pane: PaneIndex; path: string }
  | { type: "activateTab"; pane: PaneIndex; path: string }
  | { type: "focusPane"; pane: PaneIndex }
  | { type: "splitPane"; direction: SplitDirection }
  | { type: "moveTab"; from: PaneIndex; to: PaneIndex; path: string }
  | { type: "cycleTab"; pane: PaneIndex; delta: 1 | -1 }
  | { type: "focusTabAtIndex"; pane: PaneIndex; index: number }
  | { type: "setSplitRatio"; ratio: number }
  | { type: "markTabDirty"; pane: PaneIndex; path: string; dirty: boolean }
  | { type: "reset"; next: EditorLayout };

let stampSeq = 0;
function nextStamp(): number {
  stampSeq += 1;
  return stampSeq;
}

function touchTab(pane: Pane, path: string): Pane {
  const stamp = nextStamp();
  return {
    ...pane,
    tabs: pane.tabs.map((t) =>
      t.path === path ? { ...t, lastAccessedAt: stamp } : t,
    ),
    activePath: path,
  };
}

function addTabWithLru(pane: Pane, path: string): Pane {
  const existing = pane.tabs.find((t) => t.path === path);
  if (existing) return touchTab(pane, path);

  const stamp = nextStamp();
  let tabs = [...pane.tabs, { path, lastAccessedAt: stamp }];
  if (tabs.length > MAX_TABS_PER_PANE) {
    // Find and evict the LRU tab that isn't the one we just added.
    let lruIdx = -1;
    let lruStamp = Infinity;
    for (let i = 0; i < tabs.length - 1; i++) {
      if (tabs[i]!.lastAccessedAt < lruStamp) {
        lruStamp = tabs[i]!.lastAccessedAt;
        lruIdx = i;
      }
    }
    if (lruIdx >= 0) tabs = tabs.filter((_, i) => i !== lruIdx);
  }
  return { ...pane, tabs, activePath: path };
}

function removeTab(pane: Pane, path: string): Pane {
  const idx = pane.tabs.findIndex((t) => t.path === path);
  if (idx === -1) return pane;
  const tabs = pane.tabs.filter((t) => t.path !== path);
  let activePath = pane.activePath;
  if (activePath === path) {
    // Fall back to the neighbor: prefer the tab to the right, else
    // the one to the left. Null if the pane is now empty.
    activePath = tabs[idx]?.path ?? tabs[idx - 1]?.path ?? null;
  }
  return { ...pane, tabs, activePath };
}

function setPane(
  layout: EditorLayout,
  index: PaneIndex,
  pane: Pane,
): EditorLayout {
  const panes = layout.panes.slice() as Pane[];
  panes[index] = pane;
  if (panes.length === 2) {
    return { ...layout, panes: [panes[0]!, panes[1]!] };
  }
  return { ...layout, panes: [panes[0]!] };
}

function collapseIfEmpty(layout: EditorLayout): EditorLayout {
  if (layout.panes.length !== 2) return layout;
  const [a, b] = layout.panes;
  if (a.tabs.length === 0 && b.tabs.length === 0) {
    // Both empty — drop to a single empty pane.
    return {
      panes: [{ tabs: [], activePath: null }],
      split: null,
      focusedPaneIndex: 0,
      splitRatio: layout.splitRatio,
    };
  }
  if (a.tabs.length === 0) {
    return {
      panes: [b],
      split: null,
      focusedPaneIndex: 0,
      splitRatio: layout.splitRatio,
    };
  }
  if (b.tabs.length === 0) {
    return {
      panes: [a],
      split: null,
      focusedPaneIndex: 0,
      splitRatio: layout.splitRatio,
    };
  }
  return layout;
}

function reducer(layout: EditorLayout, action: Action): EditorLayout {
  switch (action.type) {
    case "openFile": {
      const idx =
        action.pane < layout.panes.length ? action.pane : 0;
      const pane = layout.panes[idx]!;
      const nextPane = addTabWithLru(pane, action.path);
      return {
        ...setPane(layout, idx as PaneIndex, nextPane),
        focusedPaneIndex: idx as PaneIndex,
      };
    }
    case "closeTab": {
      const idx =
        action.pane < layout.panes.length ? action.pane : 0;
      const pane = layout.panes[idx]!;
      const nextPane = removeTab(pane, action.path);
      return collapseIfEmpty(setPane(layout, idx as PaneIndex, nextPane));
    }
    case "closeOtherTabs": {
      const idx =
        action.pane < layout.panes.length ? action.pane : 0;
      const pane = layout.panes[idx]!;
      const kept = pane.tabs.find((t) => t.path === action.path);
      const nextPane: Pane = {
        tabs: kept ? [kept] : [],
        activePath: kept ? action.path : null,
      };
      return collapseIfEmpty(setPane(layout, idx as PaneIndex, nextPane));
    }
    case "activateTab": {
      const idx =
        action.pane < layout.panes.length ? action.pane : 0;
      const pane = layout.panes[idx]!;
      if (!pane.tabs.find((t) => t.path === action.path)) return layout;
      return {
        ...setPane(layout, idx as PaneIndex, touchTab(pane, action.path)),
        focusedPaneIndex: idx as PaneIndex,
      };
    }
    case "focusPane": {
      if (action.pane >= layout.panes.length) return layout;
      if (layout.focusedPaneIndex === action.pane) return layout;
      return { ...layout, focusedPaneIndex: action.pane };
    }
    case "splitPane": {
      // If already split, just change the direction (cheap UX:
      // flip between horizontal & vertical without collapsing).
      if (layout.panes.length === 2) {
        return { ...layout, split: action.direction };
      }
      const source = layout.panes[0]!;
      const activePath = source.activePath;
      // Seed the new pane with the active tab duplicated so the
      // user sees something immediately. If nothing is active, the
      // new pane starts empty.
      const secondPane: Pane =
        activePath !== null
          ? {
              tabs: [{ path: activePath, lastAccessedAt: nextStamp() }],
              activePath,
            }
          : { tabs: [], activePath: null };
      return {
        panes: [source, secondPane],
        split: action.direction,
        focusedPaneIndex: 1,
        splitRatio: layout.splitRatio || DEFAULT_SPLIT_RATIO,
      };
    }
    case "moveTab": {
      if (action.from === action.to) return layout;
      if (
        action.from >= layout.panes.length ||
        action.to >= layout.panes.length
      ) {
        return layout;
      }
      const fromPane = layout.panes[action.from]!;
      const toPane = layout.panes[action.to]!;
      if (!fromPane.tabs.find((t) => t.path === action.path)) return layout;
      const nextFrom = removeTab(fromPane, action.path);
      const nextTo = addTabWithLru(toPane, action.path);
      const panes = layout.panes.slice() as Pane[];
      panes[action.from] = nextFrom;
      panes[action.to] = nextTo;
      const mid: EditorLayout = {
        ...layout,
        panes:
          panes.length === 2
            ? [panes[0]!, panes[1]!]
            : [panes[0]!],
        focusedPaneIndex: action.to,
      };
      return collapseIfEmpty(mid);
    }
    case "cycleTab": {
      const idx =
        action.pane < layout.panes.length ? action.pane : 0;
      const pane = layout.panes[idx]!;
      if (pane.tabs.length === 0) return layout;
      const currentIdx = pane.tabs.findIndex(
        (t) => t.path === pane.activePath,
      );
      const base = currentIdx === -1 ? 0 : currentIdx;
      const next =
        (base + action.delta + pane.tabs.length) % pane.tabs.length;
      const targetPath = pane.tabs[next]!.path;
      return {
        ...setPane(layout, idx as PaneIndex, touchTab(pane, targetPath)),
        focusedPaneIndex: idx as PaneIndex,
      };
    }
    case "focusTabAtIndex": {
      const idx =
        action.pane < layout.panes.length ? action.pane : 0;
      const pane = layout.panes[idx]!;
      if (action.index < 0 || action.index >= pane.tabs.length) return layout;
      const targetPath = pane.tabs[action.index]!.path;
      return {
        ...setPane(layout, idx as PaneIndex, touchTab(pane, targetPath)),
        focusedPaneIndex: idx as PaneIndex,
      };
    }
    case "setSplitRatio": {
      const clamped = Math.max(0.15, Math.min(0.85, action.ratio));
      return { ...layout, splitRatio: clamped };
    }
    case "markTabDirty": {
      const idx =
        action.pane < layout.panes.length ? action.pane : 0;
      const pane = layout.panes[idx]!;
      const tab = pane.tabs.find((t) => t.path === action.path);
      // Skip if no such tab or the dirty bit is already what we want
      // — keeps the layout reference stable and avoids a needless
      // localStorage persist tick.
      if (!tab) return layout;
      const current = tab.dirty === true;
      if (current === action.dirty) return layout;
      const nextPane: Pane = {
        ...pane,
        tabs: pane.tabs.map((t) =>
          t.path === action.path ? { ...t, dirty: action.dirty } : t,
        ),
      };
      return setPane(layout, idx as PaneIndex, nextPane);
    }
    case "reset": {
      return action.next;
    }
  }
}

// ─── hook ────────────────────────────────────────────────────────

export interface EditorTabsApi {
  layout: EditorLayout;
  openFile: (path: string, pane?: PaneIndex) => void;
  closeTab: (path: string, pane: PaneIndex) => void;
  activateTab: (path: string, pane: PaneIndex) => void;
  focusPane: (pane: PaneIndex) => void;
  splitPane: (direction: SplitDirection) => void;
  moveTab: (from: PaneIndex, to: PaneIndex, path: string) => void;
  cycleTab: (delta: 1 | -1) => void;
  focusTabAtIndex: (index: number) => void;
  closeActiveTab: () => void;
  setSplitRatio: (ratio: number) => void;
  /** Set the dirty bit on the matching tab. No-op if the tab doesn't
   *  exist or the bit is already in the requested state. */
  setTabDirty: (path: string, pane: PaneIndex, dirty: boolean) => void;
  /** Close every tab in the focused pane except the currently active one. */
  closeOtherTabs: () => void;
}

/**
 * Per-project editor layout state. When `projectPath` changes we
 * reload the layout for that project (validating paths against the
 * known `files` list) so switching sessions/worktrees swaps tabs
 * wholesale. The caller still owns file-content fetching; this hook
 * only tracks paths + which pane owns them.
 */
export function useEditorTabs(
  projectPath: string | null,
  files: readonly string[],
): EditorTabsApi {
  // Seed with an empty layout synchronously; the load effect below
  // swaps in the stored layout once `files` has resolved. Doing the
  // load in an effect (rather than useState init) lets us wait for
  // the first non-empty `files` list so stale paths are filtered
  // against the real file index.
  const [layout, dispatch] = React.useReducer(reducer, emptyLayout());

  const filesKey = files.length > 0 ? projectPath : null;
  const hasLoadedRef = React.useRef<string | null>(null);

  // Load layout when projectPath changes (or when files arrives for
  // the first time for a given projectPath).
  React.useEffect(() => {
    if (!projectPath) {
      if (hasLoadedRef.current !== null) {
        dispatch({ type: "reset", next: emptyLayout() });
        hasLoadedRef.current = null;
      }
      return;
    }
    // Re-load when the projectPath changes. We do NOT re-load on
    // subsequent `files` updates for the same project — that would
    // clobber in-memory tab state every time the file index
    // refreshes.
    if (hasLoadedRef.current === projectPath) return;
    // If files hasn't loaded yet, wait so validation isn't a no-op
    // that drops every tab as "missing".
    if (files.length === 0) {
      // Clear any stale layout from a previous project while we wait.
      if (hasLoadedRef.current !== null) {
        dispatch({ type: "reset", next: emptyLayout() });
      }
      return;
    }
    const validPaths = new Set(files);
    const loaded = loadLayout(projectPath, validPaths);
    dispatch({ type: "reset", next: loaded });
    hasLoadedRef.current = projectPath;
  }, [projectPath, files, filesKey]);

  // Persist layout changes, debounced. Skip persistence before the
  // initial load lands so we don't immediately overwrite stored
  // state with an empty layout on mount.
  React.useEffect(() => {
    if (!projectPath) return;
    if (hasLoadedRef.current !== projectPath) return;
    const handle = window.setTimeout(() => {
      try {
        window.localStorage.setItem(
          storageKey(projectPath),
          JSON.stringify(serialize(layout)),
        );
      } catch {
        /* storage may be unavailable */
      }
    }, 150);
    return () => window.clearTimeout(handle);
  }, [projectPath, layout]);

  // Stable action callbacks. Dispatch is stable; we memoize the
  // helpers so consumers can pass them into memo'd children without
  // churn.
  const api = React.useMemo<EditorTabsApi>(() => {
    return {
      layout,
      openFile: (path, pane) =>
        dispatch({
          type: "openFile",
          pane: pane ?? layout.focusedPaneIndex,
          path,
        }),
      closeTab: (path, pane) => dispatch({ type: "closeTab", pane, path }),
      activateTab: (path, pane) =>
        dispatch({ type: "activateTab", pane, path }),
      focusPane: (pane) => dispatch({ type: "focusPane", pane }),
      splitPane: (direction) => dispatch({ type: "splitPane", direction }),
      moveTab: (from, to, path) =>
        dispatch({ type: "moveTab", from, to, path }),
      cycleTab: (delta) =>
        dispatch({ type: "cycleTab", pane: layout.focusedPaneIndex, delta }),
      focusTabAtIndex: (index) =>
        dispatch({
          type: "focusTabAtIndex",
          pane: layout.focusedPaneIndex,
          index,
        }),
      closeActiveTab: () => {
        const pane = layout.panes[layout.focusedPaneIndex]!;
        if (pane.activePath) {
          dispatch({
            type: "closeTab",
            pane: layout.focusedPaneIndex,
            path: pane.activePath,
          });
        }
      },
      closeOtherTabs: () => {
        const pane = layout.panes[layout.focusedPaneIndex]!;
        if (pane.activePath) {
          dispatch({
            type: "closeOtherTabs",
            pane: layout.focusedPaneIndex,
            path: pane.activePath,
          });
        }
      },
      setSplitRatio: (ratio) => dispatch({ type: "setSplitRatio", ratio }),
      setTabDirty: (path, pane, dirty) =>
        dispatch({ type: "markTabDirty", pane, path, dirty }),
    };
  }, [layout]);

  return api;
}

// ─── helpers exported for tests / consumers ──────────────────────

export const __test__ = {
  reducer,
  emptyLayout,
  serialize,
  deserialize,
};
