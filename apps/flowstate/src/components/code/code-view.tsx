import * as React from "react";
import { useNavigate } from "@tanstack/react-router";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import {
  ArrowLeft,
  Maximize2,
  Minimize2,
  PanelRight,
  PanelRightClose,
  RefreshCw,
  X,
} from "lucide-react";
import { SidebarTrigger, useSidebar } from "@/components/ui/sidebar";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { useApp } from "@/stores/app-store";
import { cn } from "@/lib/utils";
import { isMacOS } from "@/lib/popout";
import {
  defaultContentSearchOptions,
  nextContentSearchToken,
  readProjectFile,
  searchFileContents,
  stopContentSearch,
  writeProjectFile,
  writeProjectFileBytes,
  type ContentBlock,
  type ContentSearchOptions,
} from "@/lib/api";
import {
  directoryQueryOptions,
  projectFilesQueryOptions,
} from "@/lib/queries";
import { getEditorKind } from "@/lib/language-from-path";
import { dirname, normalizePath } from "@/lib/paths";
import { openUrl as openExternal } from "@tauri-apps/plugin-opener";
import {
  matchesPickerQuery,
  parsePickerQuery,
  splitGlobList,
} from "@/lib/glob";
import { rankFileMatches } from "@/lib/mention-utils";
import { useTheme } from "@/hooks/use-theme";
import { useEditorPrefs } from "@/hooks/use-editor-prefs";
import { toast } from "@/hooks/use-toast";
import { hashContent } from "@/lib/content-hash";
import { FileTree } from "./file-tree";
import { ChangedFilesList } from "./changed-files-list";
import { Multibuffer } from "./multibuffer";
import { SearchPalette } from "./search-palette";
import { TabBar } from "./tab-bar";
import { EditorPanes } from "./editor-panes";
import { DiffCommentOverlay } from "@/components/chat/diff-comment-overlay";
import {
  useEditorTabs,
  type PaneIndex,
  type Pane as PaneState,
} from "./use-editor-tabs";
import { useEnsurePierrePoolActive } from "@/lib/pierre-pool-controller";

// CM6 editor module is dynamic-imported so the editor chunk
// (CodeMirror + vim + Shiki integration, ~180 KB gz) only lands
// in the bundle on first file open. The `Suspense` fallback in
// `CodeViewBody` covers the brief load gap.
const LazyCodeEditor = React.lazy(() => import("./code-editor"));
// Sibling rich-edit chunks for `.md*` and `.excalidraw.*` files. Each
// is loaded lazily on first encounter — markdown pulls in mermaid
// (~few MB) the first time the user opens a `.md`, and the
// excalidraw editor pulls in `@excalidraw/excalidraw` (~3 MB) on the
// first drawing open. Files that never trigger these chunks pay zero
// bundle cost.
const LazyMarkdownEditor = React.lazy(
  () => import("../markdown/markdown-editor"),
);
const LazyExcalidrawEditor = React.lazy(
  () => import("../markdown/excalidraw-editor"),
);
// Thin wrapper over `LazyCodeEditor` adding a code/preview toggle for
// `.html` / `.htm` files. Lazy because it pulls in CodeEditor anyway
// and we don't want HTML-specific bytes in the bundle until first
// HTML file is opened.
const LazyHtmlEditor = React.lazy(() => import("../html/html-editor"));

// Frontend-side max size for inline editing. The Rust read API
// caps at 4 MiB (CODE_VIEW_MAX_FILE_BYTES) so files above that
// never reach us, but if a future loader bypasses the Rust cap
// we still don't want to mount CM6 on a multi-megabyte buffer
// (the rope can take it; the keystroke path stays smooth) but
// the initial paint cost on huge files is wasteful — better to
// surface a banner and let the user pick another tool.
const MAX_EDITABLE_BYTES = 10 * 1024 * 1024;

// Read-only editor view: file tree + Cmd+P / content search picker
// + single-file viewer. Layout:
//
//   ┌────────────────────────────────────────────────┐
//   │ header                                          │
//   ├──────────────┬─────────────────────────────────┤
//   │ tree         │ search bar [Files|Content]      │
//   │              ├─────────────────────────────────┤
//   │ src/         │ results (when query is non-empty)│
//   │  foo.ts      ├─────────────────────────────────┤
//   │  bar.ts      │                                  │
//   │              │ <Virtualizer><PierreFile />     │
//   │ [drag]       │                                  │
//   └──────────────┴─────────────────────────────────┘
//
// Heavy work is offloaded:
//  * File listing: rust `list_project_files` (ripgrep's `ignore`
//    crate, parallel gitignore walker)
//  * Content search: rust `search_file_contents` (ripgrep's
//    grep-searcher + grep-regex)
//  * Syntax highlighting + virtualization: @pierre/diffs <File>
//    inside <Virtualizer>, sharing the worker pool from main.tsx

// Hard cap on rendered picker rows. We rank with `rankFileMatches`
// first (basename-exact > basename-prefix > path-segment > basename-
// contains > path-contains), then take the top `PICKER_RESULT_LIMIT`.
// 200 is high enough that a real "what was that file called" search
// finds it without scrolling, low enough that the popup stays
// scannable. Overflow is surfaced as a numeric "+N more — refine"
// hint in the header so users know the cap is biting.
const PICKER_RESULT_LIMIT = 200;
// Trailing-edge debounce for the content-search call. 600ms is
// deliberately on the patient side — long enough that even slow
// typists don't fire a ripgrep walk per keystroke, and any
// in-flight search has time to settle before the next one
// kicks off. The effect's cleanup still cancels stale promises
// so only the latest query's results ever land.
const CONTENT_SEARCH_DEBOUNCE_MS = 600;

const TREE_WIDTH_KEY = "flowstate:code-tree-width";
const TREE_COLLAPSED_KEY = "flowstate:code-tree-collapsed";
// Persists the file-picker fuzzy/acronym mode toggle across reloads.
// See the `useFuzzyFiles` initializer for default + rationale.
const FUZZY_FILES_STORAGE_KEY = "flowstate:fuzzy-files";
const TREE_DEFAULT_WIDTH = 260;
const TREE_MIN_WIDTH = 160;
const TREE_MAX_WIDTH = 520;
// Width of the collapsed-tree strip — just wide enough for a
// single icon button, narrow enough to cede most of the
// horizontal space back to the right pane.
const TREE_COLLAPSED_WIDTH = 28;

// Stable empty-array sentinel for the filesQuery fallback. Pulled
// out of render so a re-render doesn't allocate a new [] every time
// and break downstream `useMemo([files])` reference equality.
const EMPTY_FILES: readonly string[] = Object.freeze([]);

// Atomic "file that finished loading" record. Bundling path+contents
// keeps PierreFile from ever seeing a mismatched pair during the
// render between a click and the fetch effect.
interface LoadedFile {
  path: string;
  contents: string;
  cacheKey: string;
}

interface ContentSearchUiOptions {
  advancedOpen: boolean;
  include: string;
  exclude: string;
  useRegex: boolean;
  /** Fuzzy-match each line against the query using fff-search's
   *  Smith-Waterman scorer. Typo-tolerant and inherently
   *  case-insensitive — wins over `useRegex` when both are set
   *  (the Rust side enforces the same precedence). */
  useFuzzy: boolean;
  caseSensitive: boolean;
}

function defaultContentSearchUiOptions(): ContentSearchUiOptions {
  return {
    advancedOpen: false,
    include: "",
    exclude: "",
    useRegex: false,
    useFuzzy: false,
    caseSensitive: true,
  };
}

export type SearchMode = "files" | "content";

interface CodeViewProps {
  sessionId?: string;
  projectPath?: string;
  /** Optional initial picker mode, sourced from the `mode` search
   *  param on /code/$sessionId. The global ⌘P / ⌘⇧F shortcuts set
   *  this so the user lands directly in the right tab; defaults to
   *  "files" when absent. Subsequent changes to the search param
   *  (e.g. ⌘⇧F while already on /code) re-seed the mode via the
   *  effect below — without re-syncing, a second press of ⌘⇧F from
   *  inside CodeView would be a no-op because the route was already
   *  active. */
  initialSearchMode?: SearchMode;
  /** When true, the view is mounted as a side panel inside ChatView
   *  rather than as a full-screen route. In this mode:
   *   * Use `h-full` so the host's flex container governs height.
   *   * Hide the SidebarTrigger + back-to-chat button — both are
   *     redundant inside the chat view.
   *   * Skip the plain-Esc → navigate-to-chat handler — the parent
   *     ChatView owns the Esc key when the panel is embedded.
   *   * Skip the Shift+Esc → internal-pane fullscreen handler —
   *     ChatView's Shift+Esc takes precedence and fullscreens the
   *     whole code panel relative to the chat column instead. */
  embedded?: boolean;
  /** Embedded-only: render a "close panel" button in the header.
   *  Wired by ChatView to drop the panel from its layout. */
  onClose?: () => void;
  /** Embedded-only: current fullscreen state for the panel-level
   *  fullscreen toggle. When true, the panel's max button shows the
   *  "exit fullscreen" glyph. */
  isFullscreen?: boolean;
  /** Embedded-only: handler for the panel-level fullscreen toggle.
   *  Same affordance as the diff/context panels — header button +
   *  Shift+Esc both flip this. */
  onToggleFullscreen?: () => void;
  /** Embedded-only: a fresh-reference object request to switch the
   *  search mode and focus the input. ChatView passes a new object
   *  on every Cmd+P / Cmd+Shift+F press; the reference change is
   *  what makes the effect re-fire even when the mode hasn't
   *  changed. Mirrors the standalone route's `initialSearchMode`
   *  re-sync, but driven by an explicit user-action signal instead
   *  of URL search params. */
  searchRequest?: { mode: SearchMode } | null;
}

export function CodeView(props: CodeViewProps) {
  const { state } = useApp();
  const navigate = useNavigate();
  const embedded = props.embedded === true;
  const { onClose, isFullscreen, onToggleFullscreen } = props;

  // Top-level CodeView is the window's titlebar — show the macOS
  // traffic-light spacer only when the sidebar is actually collapsed.
  // Embedded mode is a split-pane header; never needs the spacer.
  const { state: sidebarState } = useSidebar();
  const showMacTrafficSpacer =
    !embedded && isMacOS() && sidebarState === "collapsed";

  // Participate in the Pierre worker-pool lifecycle: wake the pool
  // if it was killed during long idle, and keep it alive while this
  // view is mounted. See pierre-pool-controller.tsx.
  useEnsurePierrePoolActive();

  // Derive projectPath from the session when not provided directly.
  const session = props.sessionId
    ? state.sessions.get(props.sessionId)
    : undefined;
  const derivedPath = React.useMemo(() => {
    if (props.projectPath) return props.projectPath;
    if (!session?.projectId) return null;
    return (
      state.projects.find((p) => p.projectId === session.projectId)?.path ??
      null
    );
  }, [props.projectPath, session?.projectId, state.projects]);
  const projectPath = derivedPath;
  const sessionId = props.sessionId;

  // ─── tree resize / collapse state ────────────────────────────
  const splitContainerRef = React.useRef<HTMLDivElement | null>(null);
  const [treeWidth, setTreeWidth] = React.useState<number>(() => {
    try {
      const saved = window.localStorage.getItem(TREE_WIDTH_KEY);
      if (saved) {
        const parsed = Number.parseInt(saved, 10);
        if (Number.isFinite(parsed) && parsed >= TREE_MIN_WIDTH) {
          return Math.min(parsed, TREE_MAX_WIDTH);
        }
      }
    } catch {
      /* storage may be unavailable */
    }
    return TREE_DEFAULT_WIDTH;
  });
  const [treeCollapsed, setTreeCollapsed] = React.useState<boolean>(() => {
    try {
      return window.localStorage.getItem(TREE_COLLAPSED_KEY) === "1";
    } catch {
      return false;
    }
  });
  const toggleTreeCollapsed = React.useCallback(() => {
    setTreeCollapsed((prev) => {
      const next = !prev;
      try {
        window.localStorage.setItem(TREE_COLLAPSED_KEY, next ? "1" : "0");
      } catch {
        /* storage may be unavailable */
      }
      return next;
    });
  }, []);

  // ─── file list / picker state ────────────────────────────────
  // Backed by fff-search's per-worktree mmap-mounted index. The query
  // returns a `ProjectFileListing` snapshot — `files` is everything
  // walked so far (no cap), `indexing` flips to false once fff's
  // background scanner finishes. While indexing is true, React Query
  // re-polls every 750 ms (see `projectFilesQueryOptions`) so the
  // picker visibly fills in on huge repos. The chat session's
  // `turn_completed` event explicitly invalidates this query so
  // agent-created files appear immediately.
  const filesQuery = useQuery(projectFilesQueryOptions(projectPath));
  // structuralSharing keeps the same array reference when a refresh
  // returns identical data, so the FileTree useMemo dependency stays
  // stable on no-op refreshes. EMPTY_FILES is a frozen module-level
  // sentinel for the same reason.
  const files = (filesQuery.data?.files ?? EMPTY_FILES) as string[];
  // True while fff-search's cold scan is still walking the worktree.
  // Surfaced in the picker header alongside the live file count so
  // the user can see "Indexing 47 312 files…" instead of an empty
  // popup on a 100k-file repo. React Query re-polls every 750 ms
  // until this flips to false (see `projectFilesQueryOptions`).
  const indexing = filesQuery.data?.indexing ?? false;
  // Only show the "loading…" placeholder on a true cold fetch (no
  // cached data yet). A populated cache means the picker is already
  // usable and we should not flash a loading state on remount; the
  // separate `indexing` badge handles the "still walking" state.
  const filesLoading = filesQuery.isPending && !!projectPath;
  const filesError = filesQuery.error ? String(filesQuery.error) : null;

  // Manual file-tree refresh state. The query client and tick are
  // declared here next to filesQuery; the actual `refreshFileTree`
  // callback is defined further down (after `gitModeEnabled` is in
  // scope) so its mode-aware branch can read that flag without
  // hitting a temporal-dead-zone error.
  const queryClient = useQueryClient();
  const [treeRefreshTick, setTreeRefreshTick] = React.useState(0);

  // ─── search state ────────────────────────────────────────────
  // Seed from the route's `mode` search param so deep links / the
  // global ⌘P, ⌘⇧F shortcuts land in the right tab. Falls back to
  // "files" — the same default the view had before route-driven mode
  // existed. The re-sync effect that handles in-place search-param
  // changes lives below `inputRef` so it can focus the input.
  const [searchMode, setSearchMode] = React.useState<SearchMode>(
    props.initialSearchMode ?? "files",
  );
  const [query, setQuery] = React.useState("");
  // Legacy highlight cursor for the inline picker. The SearchPalette
  // tracks its own selection internally; this state stays so the
  // reset effect below still runs (cheap, decoupled from the new
  // palette).
  const [, setHighlightedIndex] = React.useState(0);
  const [contentBlocks, setContentBlocks] = React.useState<ContentBlock[]>([]);
  const [contentSearching, setContentSearching] = React.useState(false);
  const [contentSearchError, setContentSearchError] = React.useState<
    string | null
  >(null);
  // Legacy content-search options state. The new SearchPalette owns
  // its own option toggles; this slot survives only to keep the
  // (unreachable) Multibuffer branch below well-typed. Setter is
  // dropped because nothing writes the slot anymore.
  const [contentOptions] =
    React.useState<ContentSearchUiOptions>(defaultContentSearchUiOptions);
  // Fuzzy-mode toggle for the FILE picker (Cmd+P). Independent from
  // the content-search fuzzy flag because the matchers are different:
  // file fuzzy is the JS-side subsequence scorer in `lib/fuzzy.ts`
  // (instant, no IPC), content fuzzy goes through fff-search's
  // Smith-Waterman grep mode on the Rust side.
  //
  // Default = ON. Fuzzy mode is a strict superset: it still scores
  // contiguous substring matches highest, plus enables IntelliJ /
  // Zed-style acronym matching ("usc" → "UserServiceController.ts")
  // and typo-tolerant subsequence matches. Defaulting it OFF made
  // the acronym feature silently inert because the substring
  // pre-filter dropped acronym-only candidates before the scorer
  // ever saw them. Persisted to localStorage so a user who
  // explicitly turns it off keeps that preference across reloads.
  // Legacy fuzzy-mode toggle for the FILE picker. The new
  // SearchPalette has its own per-mode fuzzy toggle, so this state
  // is no longer mutated — but the localStorage read survives so
  // older preferences aren't lost; future cleanup can delete it.
  const [useFuzzyFiles] = React.useState<boolean>(() => {
    if (typeof window === "undefined") return true;
    const raw = window.localStorage.getItem(FUZZY_FILES_STORAGE_KEY);
    if (raw === "true") return true;
    if (raw === "false") return false;
    return true;
  });
  React.useEffect(() => {
    if (typeof window === "undefined") return;
    window.localStorage.setItem(
      FUZZY_FILES_STORAGE_KEY,
      String(useFuzzyFiles),
    );
  }, [useFuzzyFiles]);

  // ─── tabs + panes layout ─────────────────────────────────────
  // `useEditorTabs` owns the per-project tab/pane layout and
  // persists it to localStorage keyed by projectPath. File CONTENTS
  // live in the separate cache below so switching between already-
  // opened tabs is instant (no re-fetch flash).
  const tabs = useEditorTabs(projectPath, files);
  const layout = tabs.layout;

  // Per-path content cache. Kept in refs so that async fetches can
  // mutate without stomping the render cycle, with a bumpable
  // `cacheVersion` to signal React when something visible changed.
  // `loadedFile` still bundles {path, contents, cacheKey} atomically
  // — splitting across two useState slots lets React commit one
  // intermediate render after a click (where activePath is the new
  // file but contents still hold the previous file) — PierreFile
  // then mounts with the new name but stale contents, the "opens
  // the wrong file" bug. The cacheKey drives @pierre/diffs'
  // worker-pool LRU; see the sibling explanation in `diff-panel.tsx`.
  const fileCacheRef = React.useRef<Map<string, LoadedFile>>(new Map());
  const loadingPathsRef = React.useRef<Set<string>>(new Set());
  // Bumping this state is the one-liner we use to re-render after
  // mutating a ref-owned cache. The value itself is never read —
  // React re-renders on any state update, which re-executes
  // TabPaneView and re-reads `fileCacheRef.current`.
  const [, setCacheVersion] = React.useState(0);
  const [fileErrors, setFileErrors] = React.useState<Map<string, string>>(
    () => new Map(),
  );

  // When the user clicks "Back to N matches" while a tab is active,
  // we temporarily show the multibuffer in the focused pane even
  // though a tab is active. Any subsequent tab activation or file
  // open clears the override.
  // Legacy multibuffer-override toggle. The Multibuffer branch in
  // the render below is unreachable now (the new SearchPalette is
  // the canonical content-search surface), but TabPaneView callbacks
  // still set this to keep the old branch ready for a quick rollback
  // — the value itself is never read.
  const [, setMultibufferOverride] = React.useState(false);

  // Transient fullscreen state for split layouts. When non-null, the
  // indicated pane takes the whole viewer area; the other pane stays
  // mounted (display:none) so its CodeMirror state survives the
  // toggle. Toggled via Shift+Esc; reset automatically when the
  // layout collapses to a single pane.
  const [fullscreenedPane, setFullscreenedPane] =
    React.useState<PaneIndex | null>(null);

  // Editor preferences. vim is a global preference (backed by
  // localStorage and shared across panes via a module-singleton
  // store) so toggling it flips both panes at once. gitMode is
  // per-session — keyed by `sessionId` in the transient store, lost
  // on reload, mirroring how diff-panel-open is per-thread. When
  // `sessionId` is undefined (the standalone /code/$id route with
  // no chat parent), the toggle reads as `false` and writes are
  // no-ops. Soft-wrap is no longer a knob — long lines were
  // breaking the editor viewport, so it's hardcoded on inside
  // CodeEditor.
  const {
    vimEnabled,
    gitModeEnabled,
    setGitModeEnabled,
  } = useEditorPrefs(sessionId);

  // Manual file-tree refresh, wired to the FILES/Changed header
  // button. We hold the documented "no automatic refetches" contract
  // on `projectFilesQueryOptions` and `directoryQueryOptions` — a
  // click here is the explicit user signal those queries are gated
  // on. Branches by mode:
  //
  //   * Files mode: invalidate the flat project-files list AND every
  //     cached `directory` listing for this project so new files
  //     created on disk show up in the tree, whether the user is
  //     viewing the root or has subfolders expanded.
  //   * Git mode: bump `treeRefreshTick`, forwarded into
  //     ChangedFilesList → useStreamedGitDiffSummary's `refreshTick`
  //     knob. Bumping it tears down the git subprocess and restarts
  //     the status/numstat stream while keeping the previous list
  //     visible until phase 1 lands (no flash to empty).
  const refreshFileTree = React.useCallback(() => {
    if (gitModeEnabled) {
      setTreeRefreshTick((t) => t + 1);
      return;
    }
    if (!projectPath) return;
    void queryClient.invalidateQueries({
      queryKey: ["code", "project-files", projectPath],
    });
    void queryClient.invalidateQueries({
      // Predicate form so every cached subdirectory listing for this
      // project (any subPath) gets invalidated in one call. Cached
      // shape is `["code", "directory", projectPath, subPath]` —
      // matching on the first three positions catches all of them.
      predicate: (q) => {
        const k = q.queryKey;
        return (
          Array.isArray(k) &&
          k[0] === "code" &&
          k[1] === "directory" &&
          k[2] === projectPath
        );
      },
    });
  }, [gitModeEnabled, projectPath, queryClient]);

  // ─── file-tree mutation hooks ────────────────────────────────
  // The /code file tree creates / renames / moves / trashes files
  // through Tauri commands; the tree component handles its own
  // listing-cache invalidation per-directory. The two callbacks
  // below let it tell us about open editor tabs that need to be
  // closed (path was removed) or re-targeted (path moved/renamed)
  // so the tab bar stays in sync with disk.
  //
  // `closeMatchingTabs` walks every pane and closes any tab whose
  // path equals or sits under the removed sub-path (a single-file
  // delete and a folder-trash both flow through this helper). We
  // also drop file-content + file-error cache entries for the
  // affected paths so a subsequent re-create doesn't hand back a
  // stale buffer.
  const closeMatchingTabs = React.useCallback(
    (subPath: string) => {
      if (!subPath) return;
      const prefix = `${subPath}/`;
      const matches = (p: string) => p === subPath || p.startsWith(prefix);
      // Snapshot panes outside the dispatch loop so we don't iterate
      // a value that mutates underneath us.
      const panes = layout.panes;
      panes.forEach((pane, idx) => {
        const paneIdx = idx as PaneIndex;
        for (const tab of pane.tabs) {
          if (matches(tab.path)) {
            tabs.closeTab(tab.path, paneIdx);
          }
        }
      });
      // Drop cached buffers + errors for any matching path.
      const cache = fileCacheRef.current;
      for (const key of Array.from(cache.keys())) {
        if (matches(key)) cache.delete(key);
      }
      setFileErrors((prev) => {
        let mutated = false;
        const next = new Map(prev);
        for (const key of Array.from(next.keys())) {
          if (matches(key)) {
            next.delete(key);
            mutated = true;
          }
        }
        return mutated ? next : prev;
      });
      setCacheVersion((v) => v + 1);
    },
    [layout.panes, tabs],
  );

  // `retargetMatchingTabs` covers rename + move. Any tab whose
  // path starts with the old sub-path is re-opened at the new
  // path (preserving the active pane / focus context) and the old
  // tab is closed. Cache entries are similarly migrated so the new
  // tab opens against a warm buffer rather than re-fetching.
  const retargetMatchingTabs = React.useCallback(
    (oldSubPath: string, newSubPath: string) => {
      if (!oldSubPath || oldSubPath === newSubPath) return;
      const oldPrefix = `${oldSubPath}/`;
      const rewrite = (p: string): string | null => {
        if (p === oldSubPath) return newSubPath;
        if (p.startsWith(oldPrefix)) {
          return `${newSubPath}/${p.slice(oldPrefix.length)}`;
        }
        return null;
      };
      // Migrate cache entries first so the re-opened tab finds its
      // buffer immediately (the editor reads from `fileCacheRef`).
      const cache = fileCacheRef.current;
      for (const key of Array.from(cache.keys())) {
        const next = rewrite(key);
        if (next !== null) {
          const value = cache.get(key)!;
          cache.delete(key);
          cache.set(next, { ...value, path: next });
        }
      }
      // Then re-open / close on each pane. The pane's active path is
      // tracked via `tabs.openFile`, so re-opening the new path on
      // the same pane that owned the old one preserves focus.
      const panes = layout.panes;
      panes.forEach((pane, idx) => {
        const paneIdx = idx as PaneIndex;
        const wasActive = pane.activePath;
        for (const tab of pane.tabs) {
          const next = rewrite(tab.path);
          if (next !== null) {
            tabs.openFile(next, paneIdx);
            tabs.closeTab(tab.path, paneIdx);
            if (wasActive === tab.path) {
              tabs.activateTab(next, paneIdx);
            }
          }
        }
      });
      setCacheVersion((v) => v + 1);
    },
    [layout.panes, tabs],
  );

  // Confirm-close-with-unsaved-changes dialog state. This only
  // appears in the rare case where auto-save-on-blur has FAILED
  // (e.g., file became read-only on disk), leaving the tab dirty.
  // In the happy path, focus-out auto-saves, the dirty bit clears
  // before the close click lands, and this dialog never shows.
  const [confirmClose, setConfirmClose] = React.useState<{
    path: string;
    pane: PaneIndex;
  } | null>(null);

  const inputRef = React.useRef<HTMLInputElement>(null);

  // ── search palette open state ────────────────────────────────
  // Ported from zen-tools — a single popup hosts both Files (⌘P) and
  // Content (⌘⇧F) modes, with Tab to swap between them. Replaces
  // the old top-input picker. Open / mode are driven off the same
  // signals the old input used:
  //   * `initialSearchMode` — URL-search-param-driven, standalone
  //     /code route. The two effects below trigger on every fresh
  //     value, mirroring the old input's focus + select behaviour.
  //   * `searchRequest` — fresh-object signal from the chat-embedded
  //     host. ChatView mints a new object on every shortcut press so
  //     the reference change re-fires the effect even when mode is
  //     the same.
  const [paletteOpen, setPaletteOpen] = React.useState(false);

  // Re-sync search mode when the route's `mode` search param changes
  // while this view is already mounted. Pressing ⌘⇧F from /code/$id
  // (already on the route) pushes a new search-param value but
  // doesn't remount this component, so without this effect the second
  // press would be a silent no-op.
  //
  // Skip the FIRST run after mount, even when `initialSearchMode` is
  // set: arriving at /code/$id by URL alone (no explicit shortcut
  // press in this session) shouldn't auto-pop the palette over the
  // editor. The palette only opens when a *fresh* mode value lands
  // post-mount — i.e. after a ⌘P / ⌘⇧F press routes through.
  const initialModeFirstRun = React.useRef(true);
  React.useEffect(() => {
    if (initialModeFirstRun.current) {
      initialModeFirstRun.current = false;
      if (props.initialSearchMode) setSearchMode(props.initialSearchMode);
      return;
    }
    if (!props.initialSearchMode) return;
    setSearchMode(props.initialSearchMode);
    setPaletteOpen(true);
  }, [props.initialSearchMode]);

  // Embedded-mode counterpart: re-sync mode + open the palette when
  // the parent dispatches a fresh `searchRequest` object. Same
  // semantics as the URL-driven effect above — including the
  // skip-first-mount guard, so a panel re-mount with a leftover
  // request object never auto-pops the palette. ChatView clears the
  // request on toggle (see chat-view.tsx `toggleCodeView`); this
  // ref-guard is the belt-and-braces second line of defence.
  const searchRequestFirstRun = React.useRef(true);
  React.useEffect(() => {
    if (searchRequestFirstRun.current) {
      searchRequestFirstRun.current = false;
      if (props.searchRequest) setSearchMode(props.searchRequest.mode);
      return;
    }
    if (!props.searchRequest) return;
    setSearchMode(props.searchRequest.mode);
    setPaletteOpen(true);
  }, [props.searchRequest]);

  // Esc → leave the code view. If the user came from a chat thread,
  // route back to it; otherwise fall through to browser history. Skip
  // when focus is in the search input — handleInputKeyDown owns Esc
  // there (clear query first, blur, etc.) and we don't want this
  // global handler to swallow that. Same isInTextInput rule as the
  // tab-bar shortcuts below so typing Esc into any other text field
  // (mention popup, future inline rename, etc.) keeps working.
  React.useEffect(() => {
    function isInTextInputEl(target: EventTarget | null): boolean {
      if (!(target instanceof HTMLElement)) return false;
      const tag = target.tagName;
      return (
        tag === "INPUT" ||
        tag === "TEXTAREA" ||
        target.isContentEditable === true
      );
    }
    function onEsc(e: KeyboardEvent) {
      if (e.key !== "Escape") return;

      // Shift+Esc — toggle fullscreen on the focused split pane.
      // Fires regardless of focus (including from inside the editor
      // contenteditable) since the fullscreen-toggle action is
      // "global" within the code view. Only meaningful when there's
      // a split open; outside that case the keystroke is ignored
      // (lets the browser default through, which is nothing for
      // Shift+Esc).
      //
      // SKIPPED in embedded mode: ChatView's outer Shift+Esc
      // handler owns this keystroke when the code view is mounted
      // as a panel — there it fullscreens the panel itself
      // (relative to the chat column), which is the more useful
      // affordance at that scope. Internal-split fullscreen would
      // be a niche second-order feature and the two handlers can't
      // both consume the same keystroke.
      if (e.shiftKey) {
        if (embedded) return;
        if (layout.panes.length !== 2 || layout.split === null) return;
        e.preventDefault();
        setFullscreenedPane((cur) =>
          cur === null ? layout.focusedPaneIndex : null,
        );
        return;
      }

      if (isInTextInputEl(e.target)) return;
      // Plain Esc inside fullscreen first exits fullscreen — gives
      // the user a one-tap escape hatch back to the split before the
      // second tap navigates back to chat. Mirrors VS Code's
      // "exit zen mode" Esc behaviour.
      if (fullscreenedPane !== null) {
        e.preventDefault();
        setFullscreenedPane(null);
        return;
      }
      // In embedded mode, plain Esc is owned by ChatView (it closes
      // the panel or pops a layer). Don't compete here.
      if (embedded) return;
      e.preventDefault();
      if (sessionId) {
        navigate({
          to: "/chat/$sessionId",
          params: { sessionId },
        });
      } else {
        // No session context (e.g. /browse?path=…) — fall back to
        // browser history. Matches the header "Back" button's
        // behavior (see Back button onClick above).
        window.history.back();
      }
    }
    window.addEventListener("keydown", onEsc);
    return () => window.removeEventListener("keydown", onEsc);
  }, [
    navigate,
    sessionId,
    layout.panes.length,
    layout.split,
    layout.focusedPaneIndex,
    fullscreenedPane,
    embedded,
  ]);

  // Reset fullscreen when the layout collapses to a single pane —
  // e.g. the user closed the last tab in the non-fullscreened pane,
  // or programmatically un-split. Without this the fullscreen flag
  // would be a no-op held against a non-existent pane that any
  // future re-split would awkwardly inherit.
  React.useEffect(() => {
    if (layout.panes.length !== 2 && fullscreenedPane !== null) {
      setFullscreenedPane(null);
    }
  }, [layout.panes.length, fullscreenedPane]);

  // Reset search + file-content caches when the project changes.
  // The tab/pane layout is owned by `useEditorTabs` and re-hydrates
  // from localStorage for the new project on its own. The file list
  // itself is owned by `filesQuery` above; useQuery swaps in the
  // new project's cached entry (or kicks off a cold fetch if none)
  // on its own, so there's no fetch logic here — only stale
  // per-project UI state to clear.
  React.useEffect(() => {
    fileCacheRef.current = new Map();
    loadingPathsRef.current = new Set();
    setFileErrors(new Map());
    setCacheVersion((v) => v + 1);
    setMultibufferOverride(false);
    setQuery("");
    setHighlightedIndex(0);
    setContentBlocks([]);
    setContentSearchError(null);
  }, [projectPath]);

  // ─── filename filter (client-side, instant) ─────────────────
  // Glob + comma-list aware. Plain queries fall back to substring
  // matching so users don't have to remember `**/foo*` for the
  // common "type half a name" case. A SPACE in any comma-chunk
  // splits it into a folder filter and a filename filter — Zed/
  // IntelliJ-style scoped search. Examples:
  //   "tabs"              substring match anywhere in the path
  //   "src tabs.ts"       basename "tabs.ts" inside a path with "src"
  //   "lib/api git.ts"    basename "git.ts" inside paths with "lib/api"
  //   "**/code *.tsx"     basename matching "*.tsx" inside any "code" dir
  // See lib/glob.ts for the parser.
  // Two-stage match: glob/scoped query first (handles `src tabs.ts`,
  // `**/code *.tsx`, comma alternatives — see `lib/glob.ts`), then
  // ranking on the survivors via `rankFileMatches`. Two ranking
  // backends:
  //   * `useFuzzyFiles=false` (default) — substring scorer:
  //     basename-exact > basename-prefix > path-segment-prefix >
  //     basename-contains > path-contains. Fast, no typo tolerance.
  //   * `useFuzzyFiles=true` — subsequence scorer in `lib/fuzzy.ts`:
  //     each query char must appear in path order, ranked by
  //     basename hits + word boundaries + consecutive runs. Tolerates
  //     typos / out-of-order chars / dropped chars.
  //
  // Glob queries (`*.ts`, `**/lib *.ts`) bypass the ranker's typo
  // tolerance regardless of mode — the glob predicate is exact and
  // the ranker just orders the survivors.
  //
  // We expose both the trimmed top-N (`rows`) and the full match
  // count (`total`) so the header can render a numeric "+N more —
  // refine query" hint when the cap is biting.
  const pickerMatch = React.useMemo<{
    rows: string[];
    total: number;
  }>(() => {
    if (searchMode !== "files") return { rows: [], total: 0 };
    const trimmed = query.trim();
    if (!trimmed) {
      return {
        rows: files.slice(0, PICKER_RESULT_LIMIT),
        total: files.length,
      };
    }

    // Classify the query so we can route around the substring
    // pre-filter when fuzzy is on. The pre-filter uses
    // `matchesPickerQuery` (substring `.includes()` under the hood),
    // which silently drops anything not literally containing the
    // query — fatal for fuzzy (typing `tbsv` returns zero before the
    // fuzzy ranker ever sees the list).
    const hasComma = trimmed.includes(",");
    const hasGlob = /[*?]/.test(trimmed);
    const spaceIdx = trimmed.indexOf(" ");
    const isScopedQuery = !hasComma && !hasGlob && spaceIdx > 0;
    const isPlainQuery = !hasComma && !hasGlob && spaceIdx < 0;

    // ── Three branches when fuzzy is on ──
    //
    //   1. Plain query (`tbsv`)            → no pre-filter; fuzzy
    //                                        ranks the full list.
    //   2. Scoped query (`src tbsv`)       → substring-filter by the
    //                                        FOLDER portion only;
    //                                        fuzzy ranks survivors
    //                                        on basename. The user
    //                                        opted into folder
    //                                        scoping, so we honor
    //                                        it — but the basename
    //                                        substring check has to
    //                                        be skipped or fuzzy
    //                                        gets an empty list
    //                                        again.
    //   3. Glob / comma query              → keep the existing
    //                                        substring pre-filter
    //                                        (explicit user intent).
    //                                        Fuzzy then orders.
    //
    // Substring mode (default) always falls through to the existing
    // pre-filter pipeline — no behaviour change.
    let survivors: string[];
    if (useFuzzyFiles && isPlainQuery) {
      survivors = files as string[];
    } else if (useFuzzyFiles && isScopedQuery) {
      const folderPart = trimmed.slice(0, spaceIdx).trim().toLowerCase();
      survivors = (files as string[]).filter((p) => {
        const slash = p.lastIndexOf("/");
        const dir = slash >= 0 ? p.slice(0, slash).toLowerCase() : "";
        return dir.includes(folderPart);
      });
    } else {
      const parsed = parsePickerQuery(trimmed);
      survivors =
        parsed.alternatives.length === 0
          ? (files as string[])
          : files.filter((f) => matchesPickerQuery(f, parsed));
    }

    // Strip the optional folder/glob qualifier off for the ranker:
    // it scores on the full path so passing the original query is
    // fine for plain substring/fuzzy queries; for scoped queries
    // (`src tabs.ts`) we hand the file part to the ranker so the
    // basename-priority kicks in.
    const rankerQuery = trimmed.includes(" ")
      ? trimmed.split(" ").pop()!
      : trimmed;
    const ranked = rankFileMatches(
      survivors,
      rankerQuery,
      Infinity,
      useFuzzyFiles ? "fuzzy" : "substring",
    );
    return {
      rows: ranked.slice(0, PICKER_RESULT_LIMIT),
      total: ranked.length,
    };
  }, [files, query, searchMode, useFuzzyFiles]);
  // `pickerMatch` is no longer consumed — the SearchPalette owns
  // both the filter and the display. Reference it so the surrounding
  // useMemo isn't flagged as dead; a follow-up sweep can delete the
  // memo itself together with `query`, `searchMode`, `useFuzzyFiles`
  // once we're sure no rollback path needs them.
  void pickerMatch;

  // ─── content search (debounced, server-side via ripgrep libs) ─
  React.useEffect(() => {
    if (searchMode !== "content") {
      setContentBlocks([]);
      setContentSearchError(null);
      setContentSearching(false);
      return;
    }
    const q = query.trim();
    if (!projectPath || !q) {
      setContentBlocks([]);
      setContentSearchError(null);
      setContentSearching(false);
      return;
    }
    // Build the rust-side options snapshot. Comma-split the
    // include / exclude inputs into clean string lists so the
    // OverrideBuilder on the rust side gets one glob per entry.
    const apiOptions: ContentSearchOptions = {
      ...defaultContentSearchOptions(),
      useRegex: contentOptions.useRegex,
      useFuzzy: contentOptions.useFuzzy,
      caseSensitive: contentOptions.caseSensitive,
      includes: splitGlobList(contentOptions.include),
      excludes: splitGlobList(contentOptions.exclude),
    };
    // Mint a fresh cancellation token per query and pass it to the
    // Rust side. When this effect tears down (next keystroke,
    // search-mode flip, unmount) the cleanup calls
    // `stopContentSearch(token)` which flips an AtomicBool inside
    // the running grep — the in-flight search bails on its next
    // cooperative check instead of running to completion. Without
    // this, three rapid keystrokes ("a" → "ab" → "abc") would start
    // three full-tree greps and only the last one's results would
    // be used; the first two burn CPU until their 30 s budgets
    // expire.
    const token = nextContentSearchToken();
    let cancelled = false;
    setContentSearching(true);
    setContentSearchError(null);
    const handle = window.setTimeout(() => {
      searchFileContents(projectPath, q, apiOptions, token)
        .then((blocks) => {
          if (cancelled) return;
          setContentBlocks(blocks);
        })
        .catch((err) => {
          if (cancelled) return;
          setContentSearchError(String(err));
          setContentBlocks([]);
        })
        .finally(() => {
          if (cancelled) return;
          setContentSearching(false);
        });
    }, CONTENT_SEARCH_DEBOUNCE_MS);
    return () => {
      cancelled = true;
      window.clearTimeout(handle);
      // Idempotent — the Rust registry silently no-ops on unknown
      // tokens, so racing with the search's own unregister-on-
      // completion is safe.
      void stopContentSearch(token).catch(() => {});
    };
  }, [
    searchMode,
    query,
    projectPath,
    contentOptions.useRegex,
    contentOptions.useFuzzy,
    contentOptions.caseSensitive,
    contentOptions.include,
    contentOptions.exclude,
  ]);

  // Multibuffer is gone — the new SearchPalette dialog replaces it.
  // The legacy state (`query`, `contentBlocks`, `multibufferOverride`,
  // …) is left in place to avoid a sweeping refactor of the wider
  // file; with the search input removed, none of it ever flips on
  // and the renderer below always takes the EditorPanes branch.
  // A follow-up cleanup pass can delete the orphaned state, the
  // search-bar helper components further down (SearchModeToggle,
  // ContentSearchAdvancedRow, FilePickerResults, SearchStatusBadge),
  // and the Multibuffer import.
  const focusedPane: PaneState =
    layout.panes[layout.focusedPaneIndex] ?? layout.panes[0]!;
  const showMultibuffer = false;

  // Reset highlight when query / mode changes.
  React.useEffect(() => {
    setHighlightedIndex(0);
  }, [query, searchMode]);

  // ─── lazy file-content fetch for each pane's active tab ─────
  // Kicks off fetches for any active path not already in the
  // cache. Results land in `fileCacheRef` and a `cacheVersion`
  // bump re-renders the viewer. Tracks in-flight paths in a ref
  // set so we never double-fetch the same file while an earlier
  // request is still running.
  const activePathsKey = layout.panes
    .map((p) => p.activePath ?? "")
    .join("|");
  React.useEffect(() => {
    if (!projectPath) return;
    const toFetch: string[] = [];
    for (const pane of layout.panes) {
      const p = pane.activePath;
      if (!p) continue;
      if (fileCacheRef.current.has(p)) continue;
      if (loadingPathsRef.current.has(p)) continue;
      toFetch.push(p);
    }
    if (toFetch.length === 0) return;
    for (const p of toFetch) {
      loadingPathsRef.current.add(p);
    }
    // Signal loading state to the viewer.
    setCacheVersion((v) => v + 1);

    let cancelled = false;
    for (const fetchPath of toFetch) {
      readProjectFile(projectPath, fetchPath)
        .then((contents) => {
          if (cancelled) return;
          fileCacheRef.current.set(fetchPath, {
            path: fetchPath,
            contents,
            // Content-hashed so switching back to an already-loaded
            // tab re-uses the @pierre/diffs LRU entry. The prior
            // `Date.now()` suffix guaranteed a cache miss on every
            // remount (because the key was time-based, not
            // content-based), which is why reopening the same file
            // felt slow even though bytes hadn't changed. djb2 over
            // the contents is ~20 ms for a 1 MB file, amortized
            // once per fetch, and a cache hit skips the Shiki
            // tokenize + the whole worker roundtrip.
            cacheKey: `${fetchPath}::${hashContent(contents)}`,
          });
          loadingPathsRef.current.delete(fetchPath);
          setFileErrors((prev) => {
            if (!prev.has(fetchPath)) return prev;
            const next = new Map(prev);
            next.delete(fetchPath);
            return next;
          });
          setCacheVersion((v) => v + 1);
        })
        .catch((err) => {
          if (cancelled) return;
          loadingPathsRef.current.delete(fetchPath);
          setFileErrors((prev) => {
            const next = new Map(prev);
            next.set(fetchPath, String(err));
            return next;
          });
          setCacheVersion((v) => v + 1);
        });
    }
    return () => {
      cancelled = true;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [projectPath, activePathsKey]);

  // ─── editor save / dirty wiring ──────────────────────────────────
  //
  // `handleSaveFile` is invoked by CodeEditor on Cmd+S, Vim `:w`,
  // and the focus-out auto-save path. Throws on failure so the
  // editor's own error wrapper surfaces a toast and leaves the
  // dirty bit set — the Close-with-unsaved dialog can then offer
  // the user the choice to discard.
  //
  // `handleDirtyChange` flips the tab's dirty flag in the layout
  // reducer. The reducer dedupes redundant transitions, so this
  // is a cheap dispatch on every keystroke that crosses the
  // saved/unsaved boundary.
  const handleSaveFile = React.useCallback(
    async (
      path: string,
      pane: PaneIndex,
      contents: string | Uint8Array,
    ): Promise<void> => {
      if (!projectPath) {
        throw new Error("no project");
      }
      try {
        if (typeof contents === "string") {
          await writeProjectFile(projectPath, path, contents);
        } else {
          await writeProjectFileBytes(projectPath, path, contents);
        }
      } catch (err) {
        toast({
          title: "Save failed",
          description: String(err),
          duration: 5000,
        });
        throw err;
      }
      // Re-baseline the file cache so reopening this tab uses the
      // new content (no re-fetch, no flash of stale text). For
      // binary saves we don't keep the bytes in the text cache —
      // the next open re-reads via the asset protocol.
      if (typeof contents === "string") {
        fileCacheRef.current.set(path, {
          path,
          contents,
          cacheKey: `${path}::${hashContent(contents)}`,
        });
      } else {
        // Binary: invalidate any stale text cache entry so the next
        // open re-fetches via the excalidraw asset-protocol path.
        fileCacheRef.current.delete(path);
      }
      tabs.setTabDirty(path, pane, false);
      setCacheVersion((v) => v + 1);
    },
    [projectPath, tabs],
  );

  const handleDirtyChange = React.useCallback(
    (path: string, pane: PaneIndex, dirty: boolean) => {
      tabs.setTabDirty(path, pane, dirty);
    },
    [tabs],
  );

  // ── markdown editor: link follow + image-paste cache invalidation ──
  //
  // Cmd+click on a `[label](./README.md)` calls `onLinkOpen(url)`,
  // which routes here. We resolve the URL into either:
  //   - an external open via `tauri-plugin-opener` (http/https), or
  //   - a project-relative path that we re-open as a tab.
  //
  // Wikilinks (`[[Note]]`) are intentionally NOT routed here — per the
  // locked decision, they're a visual-only decoration. Cmd+click on
  // a wikilink is a no-op.
  const handleOpenLink = React.useCallback(
    (url: string) => {
      const trimmed = url.trim();
      if (!trimmed) return;
      if (/^(https?|mailto):/i.test(trimmed)) {
        void openExternal(trimmed).catch((err: unknown) => {
          console.warn("[markdown] open external failed", err);
        });
        return;
      }
      if (!projectPath) return;
      // Compute the project-relative path. Anchor against the
      // currently-focused tab's directory so `./foo.md` resolves
      // sensibly.
      const focused = layout.panes[layout.focusedPaneIndex] ?? layout.panes[0];
      const activePath = focused?.activePath ?? null;
      const docDir = activePath ? dirname(activePath) : "";
      let relPath: string;
      if (trimmed.startsWith("/")) {
        // Absolute — strip projectPath if it's a prefix.
        if (trimmed.startsWith(projectPath)) {
          relPath = trimmed.slice(projectPath.length).replace(/^\/+/, "");
        } else {
          // Not in this project — bail.
          return;
        }
      } else {
        relPath = normalizePath(docDir ? `${docDir}/${trimmed}` : trimmed);
      }
      tabs.openFile(relPath);
    },
    [projectPath, layout, tabs],
  );

  // After the markdown editor saves a pasted image, invalidate the
  // queries that drive the file tree and the link autocomplete so
  // the new file shows up immediately. We invalidate three keys:
  //   1. The doc's parent directory (so a freshly-created `pasted/`
  //      subfolder appears).
  //   2. The `pasted/` listing under the doc dir (so the new file
  //      shows up if the user already has it expanded).
  //   3. The whole project file index (autocomplete + Cmd+P).
  const handleImageSaved = React.useCallback(
    (relPath: string) => {
      if (!projectPath) return;
      const focused = layout.panes[layout.focusedPaneIndex] ?? layout.panes[0];
      const activePath = focused?.activePath ?? null;
      const docDir = activePath ? dirname(activePath) : "";
      const pastedDir = relPath.includes("/")
        ? relPath.slice(0, relPath.lastIndexOf("/"))
        : "";
      const pastedSubpath = docDir
        ? pastedDir
          ? `${docDir}/${pastedDir}`
          : docDir
        : pastedDir;
      void queryClient.invalidateQueries(
        directoryQueryOptions(projectPath, docDir),
      );
      void queryClient.invalidateQueries(
        directoryQueryOptions(projectPath, pastedSubpath),
      );
      void queryClient.invalidateQueries(
        projectFilesQueryOptions(projectPath),
      );
    },
    [projectPath, layout, queryClient],
  );

  // Wrap close-tab to gate on dirty state. In the steady state this
  // dialog never appears — auto-save-on-blur clears `dirty` before
  // the close click lands. It's only here for the save-failure
  // case where the dirty bit is stuck.
  const handleCloseTab = React.useCallback(
    (path: string, pane: PaneIndex) => {
      const target = layout.panes[pane]?.tabs.find((t) => t.path === path);
      if (target?.dirty) {
        setConfirmClose({ path, pane });
        return;
      }
      tabs.closeTab(path, pane);
    },
    [layout, tabs],
  );

  // Cmd/Ctrl+P focuses the picker in `files` mode; Cmd/Ctrl+Shift+F
  // focuses it in `content` mode; Cmd/Ctrl+Shift+B toggles the file
  // tree collapse. Bare Cmd/Ctrl+B is left to the app sidebar (chat
  // list show/hide) so it keeps working while the code view has
  // focus — Shift is the disambiguator.
  React.useEffect(() => {
    function isInTextInput(target: EventTarget | null): boolean {
      if (!(target instanceof HTMLElement)) return false;
      const tag = target.tagName;
      if (tag === "INPUT" || tag === "TEXTAREA") return true;
      // CodeMirror 6's editor surface (.cm-content) is contenteditable but
      // is NOT a "real" text input for tab-bar shortcut purposes — we want
      // cmd+W, cmd+tab, etc. to fire even when the editor (including vim
      // normal mode) has focus. Any other contenteditable node (e.g. a
      // rich-text widget) is still treated as a text input.
      if (
        target.isContentEditable &&
        !target.classList.contains("cm-content")
      ) {
        return true;
      }
      return false;
    }
    function onKeyDown(e: KeyboardEvent) {
      const mod = e.metaKey || e.ctrlKey;
      if (!mod) return;
      const key = e.key.toLowerCase();
      if (e.shiftKey && key === "f") {
        e.preventDefault();
        setSearchMode("content");
        inputRef.current?.focus();
        inputRef.current?.select();
        return;
      }
      if (!e.shiftKey && key === "p") {
        e.preventDefault();
        setSearchMode("files");
        inputRef.current?.focus();
        inputRef.current?.select();
        return;
      }
      if (e.shiftKey && key === "b") {
        // Cmd/Ctrl+Shift+B — toggle the code view's file tree.
        // Bare Cmd/Ctrl+B is intentionally NOT bound here: that
        // shortcut belongs to the app sidebar (chat list show/hide)
        // and the user wants it to keep working while focused on the
        // code view. Shift is the disambiguator. Fires unconditionally
        // — including from inside the editor's contenteditable — since
        // the listener only exists while CodeView is mounted, so this
        // is a no-op everywhere else.
        e.preventDefault();
        toggleTreeCollapsed();
        return;
      }
      if (e.shiftKey && key === "g") {
        // Cmd/Ctrl+Shift+G — flip git mode (changed-files panel +
        // editor diff markers). Skip when typing in a real text
        // input so it doesn't clobber the user's keystroke. Note
        // CM6 also binds Cmd+G (no shift) to gotoLine — that's
        // not us; this branch is only the shift variant.
        if (isInTextInput(e.target)) return;
        e.preventDefault();
        setGitModeEnabled(!gitModeEnabled);
        return;
      }
      // Tab-bar shortcuts — all guarded by "not typing in an input"
      // so regular form editing keeps working.
      if (isInTextInput(e.target)) return;

      if (!e.shiftKey && key === "w") {
        e.preventDefault();
        tabs.closeActiveTab();
        setMultibufferOverride(false);
        return;
      }
      if (e.altKey && !e.shiftKey && e.code === "KeyT") {
        // Cmd/Ctrl+Opt+T — close all other tabs in the focused pane,
        // keeping only the currently active one.
        // Use e.code (physical key) instead of e.key because opt+T on
        // macOS produces "†" (dagger), not "t".
        e.preventDefault();
        tabs.closeOtherTabs();
        setMultibufferOverride(false);
        return;
      }
      if (key === "tab") {
        // Cmd+Tab on macOS is app-switch; most users won't actually
        // get this event. Ctrl+Tab on all platforms works though.
        e.preventDefault();
        tabs.cycleTab(e.shiftKey ? -1 : 1);
        setMultibufferOverride(false);
        return;
      }
      if (!e.shiftKey && key === "\\") {
        e.preventDefault();
        tabs.splitPane("horizontal");
        setMultibufferOverride(false);
        return;
      }
      if (e.shiftKey && key === "\\") {
        e.preventDefault();
        tabs.splitPane("vertical");
        setMultibufferOverride(false);
        return;
      }
      // Cmd/Ctrl+1..9 → jump to tab N (1-indexed) in focused pane.
      if (!e.shiftKey && key.length === 1 && key >= "1" && key <= "9") {
        const idx = Number.parseInt(key, 10) - 1;
        e.preventDefault();
        tabs.focusTabAtIndex(idx);
        setMultibufferOverride(false);
        return;
      }
    }
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [toggleTreeCollapsed, tabs, gitModeEnabled, setGitModeEnabled]);

  // Refocus the focused pane's CodeMirror editor — used after Esc
  // from the search input, or after closing other transient UIs
  // that stole focus. The editor is wrapped by `<div
  // data-code-path={path}>` (set by `CodeViewBody` so the comment
  // overlay can locate it), and the editable area is `.cm-content`
  // inside CM6's DOM. We resolve via DOM query rather than refs
  // because the editor sits inside a `React.lazy` boundary several
  // layers deep — threading a ref through every level would be
  // strictly worse for the value.
  // `focusActiveEditor`, `openFromPickerIndex`, and `handleInputKeyDown`
  // lived here when the search input was inline. The SearchPalette
  // now owns keyboard navigation and focus handoff (Radix's
  // `onCloseAutoFocus` returns focus to the trigger, which is the
  // editor host), so these helpers were removed along with the input.

  const projectLabel = React.useMemo(() => {
    if (!projectPath) return null;
    const segments = projectPath.split("/").filter(Boolean);
    return segments[segments.length - 1] ?? projectPath;
  }, [projectPath]);

  return (
    <div
      className="flex h-full min-w-0 flex-col overflow-hidden"
    >
      <header
        // Only the top-level code route is the window's titlebar — when
        // embedded inside another route's split pane it must not be
        // draggable (would steal clicks/drags from the host header) or
        // hold a spacer for traffic lights (those live in the host
        // header).
        data-tauri-drag-region={embedded ? undefined : ""}
        className={cn(
          "flex shrink-0 items-center gap-1 border-b border-border px-2 text-sm",
          // Top-level mode is the window's titlebar — match the
          // h-9 used by chat/project headers. Embedded mode is a
          // split-pane header below the chat titlebar; line it up
          // with the sibling diff/context panel headers (h-10).
          embedded ? "h-10" : "h-9",
        )}
      >
        {showMacTrafficSpacer && (
          <div className="w-16 shrink-0" data-tauri-drag-region />
        )}
        {!embedded && <SidebarTrigger />}
        {!embedded && (
          <Button
            variant="ghost"
            size="xs"
            onClick={() =>
              sessionId
                ? navigate({ to: "/chat/$sessionId", params: { sessionId } })
                : window.history.back()
            }
            title={sessionId ? "Back to chat" : "Back"}
          >
            <ArrowLeft className="h-3 w-3" />
            {sessionId ? "Chat" : "Back"}
          </Button>
        )}
        <div className="flex min-w-0 items-center gap-1 text-[11px] text-muted-foreground">
          {projectLabel && (
            <span className="truncate font-medium text-foreground">
              {projectLabel}
            </span>
          )}
          {focusedPane.activePath && (
            <>
              <span className="shrink-0">/</span>
              <span
                className="truncate font-mono"
                title={focusedPane.activePath}
              >
                {focusedPane.activePath}
              </span>
            </>
          )}
        </div>
        <div
          className="ml-auto flex shrink-0 items-center gap-1"
          data-tauri-drag-region={false}
        >
          <Button
            variant={gitModeEnabled ? "secondary" : "ghost"}
            size="xs"
            onClick={() => setGitModeEnabled(!gitModeEnabled)}
            title={
              gitModeEnabled
                ? "Git mode is ON — showing changed files only and diff markers (Cmd/Ctrl+Shift+G)"
                : "Git mode is OFF — click or press Cmd/Ctrl+Shift+G to enable"
            }
            aria-pressed={gitModeEnabled}
          >
            <span className="font-mono text-[10px] uppercase tracking-wide">
              git {gitModeEnabled ? "on" : "off"}
            </span>
          </Button>
          {/* Embedded-only: panel-level fullscreen + close. Mirrors
              the affordance the diff / context panels surface — the
              fullscreen button shares state with the chat-view
              Shift+Esc handler. */}
          {embedded && onToggleFullscreen && (
            <Button
              variant="ghost"
              size="icon-xs"
              onClick={onToggleFullscreen}
              title={
                isFullscreen
                  ? "Exit fullscreen (Shift+Esc)"
                  : "Fullscreen panel (Shift+Esc)"
              }
              aria-label={isFullscreen ? "Exit fullscreen" : "Enter fullscreen"}
              aria-pressed={isFullscreen}
            >
              {isFullscreen ? (
                <Minimize2 className="h-3 w-3" />
              ) : (
                <Maximize2 className="h-3 w-3" />
              )}
            </Button>
          )}
          {embedded && onClose && (
            <Button
              variant="ghost"
              size="icon-xs"
              onClick={onClose}
              title="Close code view panel (Cmd/Ctrl+Alt+E)"
              aria-label="Close code view panel"
            >
              <X className="h-3 w-3" />
            </Button>
          )}
        </div>
      </header>

      <div ref={splitContainerRef} className="flex min-h-0 min-w-0 flex-1">
        {/* ── LEFT: search + viewer column ──────────────────── */}
        <div className="flex min-w-0 flex-1 flex-col">
          {/* Search palette — ported from zen-tools. A modal popup
              with a Files / Content mode toggle, query input,
              ranked result list on the left, and a lazy-loaded
              preview pane on the right. ⌘P opens in Files mode,
              ⌘⇧F in Content mode (both signals route through
              `props.initialSearchMode` / `props.searchRequest`
              and the matching effects above). Replaces the
              primitive top-input search bar and the multibuffer
              that used to render content-search results. */}
          <SearchPalette
            open={paletteOpen}
            onOpenChange={setPaletteOpen}
            mode={searchMode}
            onModeChange={setSearchMode}
            projectPath={projectPath}
            files={files}
            indexing={indexing}
            onPickFile={(path) => {
              tabs.openFile(path);
              setMultibufferOverride(false);
              setQuery("");
            }}
          />

          {/* `min-w-0` is required here too: this is the flex-column
              child that hosts EditorPanes (and Multibuffer below).
              Without `min-w-0` the default `min-width: auto` resolves
              to the children's intrinsic width — for CodeMirror that's
              the longest source line — so the whole subtree (.cm-editor
              → .cm-scroller → .cm-content) measures wider than the
              panel and `EditorView.lineWrapping` wraps off-screen,
              leaving the right edge clipped by `overflow-hidden`. */}
          <div className="min-h-0 min-w-0 flex-1 overflow-hidden">
            {showMultibuffer ? (
              <Multibuffer
                query={query}
                blocks={contentBlocks}
                searching={contentSearching}
                error={contentSearchError}
                projectPath={projectPath}
                onOpenFile={(p) => {
                  tabs.openFile(p);
                  setMultibufferOverride(false);
                }}
                sessionId={sessionId ?? null}
              />
            ) : (
              <EditorPanes
                direction={layout.split}
                ratio={layout.splitRatio}
                onRatioChange={tabs.setSplitRatio}
                fullscreenedPaneIndex={fullscreenedPane}
                first={
                  <TabPaneView
                    paneIndex={0}
                    pane={layout.panes[0]!}
                    focused={layout.focusedPaneIndex === 0}
                    canSplit={layout.panes.length === 1}
                    fileCacheRef={fileCacheRef}
                    loadingPathsRef={loadingPathsRef}
                    fileErrors={fileErrors}
                    filesError={filesError}
                    hasProject={projectPath !== null}
                    projectPath={projectPath}
                    onActivate={(p) => {
                      tabs.activateTab(p, 0);
                      setMultibufferOverride(false);
                    }}
                    onClose={(p) => handleCloseTab(p, 0)}
                    onFocus={() => tabs.focusPane(0)}
                    onSplitHorizontal={() => tabs.splitPane("horizontal")}
                    onSplitVertical={() => tabs.splitPane("vertical")}
                    onDropTab={(fromPane, path) => {
                      if (fromPane !== 0) tabs.moveTab(fromPane, 0, path);
                    }}
                    sessionId={sessionId ?? null}
                    vimEnabled={vimEnabled}
                    gitModeEnabled={gitModeEnabled}
                    onSaveFile={handleSaveFile}
                    onDirtyChangeFile={handleDirtyChange}
                    projectFiles={files}
                    onOpenFile={handleOpenLink}
                    onImageSaved={handleImageSaved}
                  />
                }
                second={
                  layout.panes.length === 2 ? (
                    <TabPaneView
                      paneIndex={1}
                      pane={layout.panes[1]!}
                      focused={layout.focusedPaneIndex === 1}
                      canSplit={false}
                      fileCacheRef={fileCacheRef}
                      loadingPathsRef={loadingPathsRef}
                      fileErrors={fileErrors}
                      filesError={filesError}
                      hasProject={projectPath !== null}
                      projectPath={projectPath}
                      onActivate={(p) => {
                        tabs.activateTab(p, 1);
                        setMultibufferOverride(false);
                      }}
                      onClose={(p) => handleCloseTab(p, 1)}
                      onFocus={() => tabs.focusPane(1)}
                      onDropTab={(fromPane, path) => {
                        if (fromPane !== 1) tabs.moveTab(fromPane, 1, path);
                      }}
                      sessionId={sessionId ?? null}
                      vimEnabled={vimEnabled}
                      gitModeEnabled={gitModeEnabled}
                      onSaveFile={handleSaveFile}
                      onDirtyChangeFile={handleDirtyChange}
                      projectFiles={files}
                      onOpenFile={handleOpenLink}
                      onImageSaved={handleImageSaved}
                    />
                  ) : undefined
                }
              />
            )}
          </div>
        </div>

        {/* ── RIGHT: file tree column (collapsed or expanded) ── */}
        {treeCollapsed ? (
          <aside
            className="flex shrink-0 flex-col items-center border-l border-border bg-background py-1.5"
            style={{ width: TREE_COLLAPSED_WIDTH }}
            aria-label="File tree (collapsed)"
          >
            <Button
              variant="ghost"
              size="icon-xs"
              onClick={toggleTreeCollapsed}
              title="Show file tree (Cmd/Ctrl+Shift+B)"
              aria-label="Show file tree"
            >
              <PanelRight className="h-3 w-3" />
            </Button>
          </aside>
        ) : (
          <>
            {/* Drag handle is on the LEFT edge of the tree (between
                viewer and tree) so the user can grab the seam to
                resize, mirroring the chat-view diff/context panel
                handle position. */}
            <TreeDragHandle
              containerRef={splitContainerRef}
              width={treeWidth}
              onResize={setTreeWidth}
            />
            <aside
              className="flex shrink-0 flex-col border-l border-border bg-background"
              style={{ width: treeWidth }}
            >
              <div className="flex h-9 shrink-0 items-center gap-1 border-b border-border px-2 text-[10px] uppercase tracking-wide text-muted-foreground">
                <Button
                  variant="ghost"
                  size="icon-xs"
                  onClick={toggleTreeCollapsed}
                  title="Hide file tree (Cmd/Ctrl+Shift+B)"
                  aria-label="Hide file tree"
                >
                  <PanelRightClose className="h-3 w-3" />
                </Button>
                <span>{gitModeEnabled ? "Changed" : "Files"}</span>
                {!gitModeEnabled && filesLoading && <span>· indexing…</span>}
                <span className="ml-auto" />
                <Button
                  variant="ghost"
                  size="icon-xs"
                  onClick={refreshFileTree}
                  disabled={!projectPath || filesLoading}
                  title={
                    gitModeEnabled
                      ? "Refresh changed-files list"
                      : "Refresh file tree"
                  }
                  aria-label="Refresh file tree"
                >
                  <RefreshCw
                    className={cn(
                      "h-3 w-3",
                      // Spin while the flat-list cold fetch is in
                      // flight. Git-mode restarts complete on the
                      // order of frames so a one-off spin would
                      // flicker; we leave it static there.
                      !gitModeEnabled && filesLoading && "animate-spin",
                    )}
                  />
                </Button>
              </div>
              <div className="min-h-0 flex-1 overflow-auto">
                {filesError && !gitModeEnabled ? (
                  <div className="px-3 py-3 text-[11px] text-destructive">
                    {filesError}
                  </div>
                ) : !projectPath ? (
                  <div className="px-3 py-3 text-[11px] text-muted-foreground">
                    No project for this session.
                  </div>
                ) : gitModeEnabled ? (
                  <ChangedFilesList
                    projectPath={projectPath ?? null}
                    selectedPath={focusedPane.activePath}
                    refreshTick={treeRefreshTick}
                    onSelect={(p) => {
                      tabs.openFile(p);
                      setMultibufferOverride(false);
                      setQuery("");
                    }}
                  />
                ) : (
                  <FileTree
                    projectPath={projectPath ?? null}
                    selectedPath={focusedPane.activePath}
                    onSelect={(p) => {
                      tabs.openFile(p);
                      setMultibufferOverride(false);
                      setQuery("");
                    }}
                    onPathRemoved={closeMatchingTabs}
                    onPathRenamed={retargetMatchingTabs}
                  />
                )}
              </div>
            </aside>
          </>
        )}
      </div>
      <Dialog
        open={confirmClose !== null}
        onOpenChange={(open) => {
          if (!open) setConfirmClose(null);
        }}
      >
        <DialogContent>
          <DialogHeader>
            <DialogTitle>Unsaved changes</DialogTitle>
            <DialogDescription>
              {confirmClose
                ? `${confirmClose.path} has unsaved changes that failed to auto-save. Close without saving?`
                : ""}
            </DialogDescription>
          </DialogHeader>
          <DialogFooter>
            <Button variant="outline" onClick={() => setConfirmClose(null)}>
              Cancel
            </Button>
            <Button
              variant="destructive"
              onClick={() => {
                if (confirmClose) {
                  tabs.closeTab(confirmClose.path, confirmClose.pane);
                }
                setConfirmClose(null);
              }}
            >
              Close without saving
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </div>
  );
}

// ──────────────────────────────────────────────────────────────
// Subcomponents
// ──────────────────────────────────────────────────────────────
//
// `SearchModeToggle`, `ContentSearchAdvancedRow`, `SearchStatusBadge`,
// and `FilePickerResults` used to live here. They were the building
// blocks of the inline search bar that the new SearchPalette
// replaced. Removed wholesale — see git history for the old
// implementation. The dead Multibuffer branch in the render is
// kept around so a rollback to the old search UX is a small revert
// rather than a re-import + re-wire.


// One pane = tab strip + viewer. Owns no state itself; all inputs
// come from the parent `CodeView`. The viewer reads the shared
// fileCacheRef (mutable, ref-stable) — the parent bumps its own
// `cacheVersion` state whenever the cache changes, which re-renders
// CodeView and (since this is not memoized) this pane alongside,
// so the fresh read below picks up fetched contents.
interface TabPaneViewProps {
  paneIndex: PaneIndex;
  pane: PaneState;
  focused: boolean;
  canSplit: boolean;
  fileCacheRef: React.RefObject<Map<string, LoadedFile>>;
  loadingPathsRef: React.RefObject<Set<string>>;
  fileErrors: Map<string, string>;
  filesError: string | null;
  hasProject: boolean;
  /** Project root, threaded through so the editor can call
   *  `getGitDiffFile(projectPath, path)` when git mode is on. */
  projectPath: string | null;
  onActivate: (path: string) => void;
  onClose: (path: string) => void;
  onFocus: () => void;
  onSplitHorizontal?: () => void;
  onSplitVertical?: () => void;
  onDropTab: (fromPane: PaneIndex, path: string) => void;
  /** Forwarded to DiffCommentOverlay so hover "+" works on the open
   *  file viewer. Null disables the overlay (passthrough). */
  sessionId: string | null;
  /** Editor preferences forwarded into the CodeMirror instance.
   *  Soft-wrap is hardcoded on inside CodeEditor — no prop needed. */
  vimEnabled: boolean;
  gitModeEnabled: boolean;
  /** Save handler — bubbles all the way up to CodeView's
   *  `handleSaveFile` which writes the file via Tauri and updates
   *  the file cache + tab dirty bit. Accepts string for the regular
   *  editors and `Uint8Array` for the excalidraw binary path. */
  onSaveFile: (
    path: string,
    pane: PaneIndex,
    contents: string | Uint8Array,
  ) => Promise<void>;
  /** Dirty-bit handler — bubbles up to `tabs.setTabDirty`. */
  onDirtyChangeFile: (path: string, pane: PaneIndex, dirty: boolean) => void;
  /** Project file index, surfaced into the markdown editor's link
   *  autocomplete. */
  projectFiles: readonly string[];
  /** Open a project-relative file in a new tab in this pane. Used by
   *  Cmd+click on `[label](./README.md)` inside the markdown editor. */
  onOpenFile: (relPath: string) => void;
  /** Fired after an image was just pasted + saved next to the open
   *  document. The host invalidates the file-tree query so the new
   *  file shows up immediately in the sidebar. */
  onImageSaved?: (relPath: string) => void;
}

function TabPaneView({
  paneIndex,
  pane,
  focused,
  canSplit,
  fileCacheRef,
  loadingPathsRef,
  fileErrors,
  filesError,
  hasProject,
  projectPath,
  onActivate,
  onClose,
  onFocus,
  onSplitHorizontal,
  onSplitVertical,
  onDropTab,
  sessionId,
  vimEnabled,
  gitModeEnabled,
  onSaveFile,
  onDirtyChangeFile,
  projectFiles,
  onOpenFile,
  onImageSaved,
}: TabPaneViewProps) {
  const activePath = pane.activePath;
  const loadedFile =
    activePath !== null
      ? (fileCacheRef.current?.get(activePath) ?? null)
      : null;
  const loading =
    activePath !== null && loadingPathsRef.current?.has(activePath) === true;
  const error = activePath !== null ? (fileErrors.get(activePath) ?? null) : null;

  // Bind the save / dirty callbacks to this pane's index so the
  // editor can stay generic (it doesn't know which pane it lives in).
  const handleSave = React.useCallback(
    (contents: string | Uint8Array) =>
      onSaveFile(activePath ?? "", paneIndex, contents),
    [onSaveFile, activePath, paneIndex],
  );
  const handleDirty = React.useCallback(
    (dirty: boolean) => {
      if (activePath !== null) onDirtyChangeFile(activePath, paneIndex, dirty);
    },
    [onDirtyChangeFile, activePath, paneIndex],
  );

  return (
    <div className="flex min-h-0 min-w-0 flex-1 flex-col">
      <TabBar
        paneIndex={paneIndex}
        tabs={pane.tabs}
        activePath={pane.activePath}
        focused={focused}
        canSplit={canSplit}
        onActivate={onActivate}
        onClose={onClose}
        onSplitHorizontal={onSplitHorizontal}
        onSplitVertical={onSplitVertical}
        onFocus={onFocus}
        onDropTab={onDropTab}
      />
      {/*
        `relative` + `z-0` creates an isolated stacking context for the
        editor pane. CM6 renders its bottom panels (e.g. the vim status
        line) as `position: absolute` children of the editor root with
        no z-index of their own. Without isolation here, those panels
        escape to the nearest positioned ancestor (SidebarInset) and
        paint over the TerminalDock (z-20) sitting in that same
        stacking context. Pinning a stacking context at the pane
        boundary keeps CM panels stacked inside the editor, below the
        dock.
      */}
      <div
        className="relative z-0 flex min-h-0 min-w-0 flex-1 flex-col overflow-hidden"
        onMouseDown={onFocus}
      >
        <CodeViewBody
          path={activePath}
          loadedFile={loadedFile}
          loading={loading}
          error={error}
          filesError={filesError}
          hasProject={hasProject}
          projectPath={projectPath}
          sessionId={sessionId}
          vimEnabled={vimEnabled}
          gitModeEnabled={gitModeEnabled}
          projectFiles={projectFiles}
          onSave={handleSave}
          onDirtyChange={handleDirty}
          onOpenFile={onOpenFile}
          onImageSaved={onImageSaved}
        />
      </div>
    </div>
  );
}

interface CodeViewBodyProps {
  path: string | null;
  loadedFile: LoadedFile | null;
  loading: boolean;
  error: string | null;
  filesError: string | null;
  hasProject: boolean;
  /** Project root, threaded through so the editor can resolve git
   *  diff content via `getGitDiffFile(projectPath, path)`. */
  projectPath: string | null;
  /** Forwarded to DiffCommentOverlay — hover "+" only works when we
   *  have a chat session to attach comments to. */
  sessionId: string | null;
  vimEnabled: boolean;
  gitModeEnabled: boolean;
  /** Project file index — fed into the markdown editor's link-path
   *  autocomplete so suggestions come from the same source the
   *  Cmd+P picker uses. */
  projectFiles: readonly string[];
  /** Save handler. Accepts a string for the code/markdown editors
   *  and a `Uint8Array` for the excalidraw binary `.png` path. The
   *  host (`handleSaveFile`) dispatches to `writeProjectFile` or
   *  `writeProjectFileBytes` based on which arm comes through. */
  onSave: (contents: string | Uint8Array) => Promise<void>;
  onDirtyChange: (dirty: boolean) => void;
  /** Markdown-specific: open a project-relative file in a new tab.
   *  Used by Cmd+click on `[label](./README.md)` and similar. */
  onOpenFile: (relPath: string) => void;
  /** Markdown-specific: invalidate caches after a clipboard image
   *  was just saved next to the open document. */
  onImageSaved?: (relPath: string) => void;
}

const CodeViewBody = React.memo(function CodeViewBody({
  path,
  loadedFile,
  loading,
  error,
  filesError,
  hasProject,
  projectPath,
  sessionId,
  vimEnabled,
  gitModeEnabled,
  projectFiles,
  onSave,
  onDirtyChange,
  onOpenFile,
  onImageSaved,
}: CodeViewBodyProps) {
  const { resolvedTheme } = useTheme();
  // Editor-kind dispatch is keyed off the *current* path so a tab
  // switch from `notes.md` → `foo.ts` flips between the markdown and
  // code editors via the Suspense + key remount below.
  const editorKind = path ? getEditorKind(path) : "code";
  if (!hasProject) {
    return (
      <div className="flex h-full items-center justify-center px-4 text-center text-xs text-muted-foreground">
        This session has no project — open a session that's pinned to a
        directory to browse files.
      </div>
    );
  }
  if (filesError) {
    return (
      <div className="flex h-full items-center justify-center px-4 text-center text-xs text-destructive">
        Failed to list project files: {filesError}
      </div>
    );
  }
  if (!path) {
    return (
      <div className="flex h-full items-center justify-center px-4 text-center text-xs text-muted-foreground">
        Click a file in the tree, or press Cmd/Ctrl+P to search by name,
        Cmd/Ctrl+Shift+F to search contents.
      </div>
    );
  }
  // The `loadedFile.path !== path` guard catches the one-frame window
  // after a click where `selectedPath` has already flipped to the new
  // file but the fetch effect hasn't yet cleared `loadedFile`. Showing
  // the loading placeholder instead of mounting PierreFile with a
  // mismatched (name, contents) pair is what keeps the wrong file's
  // text from flashing into the viewer on every click.
  if (loading || !loadedFile || loadedFile.path !== path) {
    if (error) {
      return (
        <div className="flex h-full items-center justify-center px-4 text-center text-xs text-destructive">
          {error}
        </div>
      );
    }
    return (
      <div className="flex h-full items-center justify-center text-[11px] text-muted-foreground">
        Loading {path}…
      </div>
    );
  }
  // Files larger than the editable cap get a static banner instead
  // of CM6 — but excalidraw `.png` files routinely exceed the cap
  // (the embedded scene + raster bytes adds up fast) and we still
  // want them editable. Skip the cap for that arm.
  if (
    editorKind !== "excalidraw" &&
    loadedFile.contents.length > MAX_EDITABLE_BYTES
  ) {
    return (
      <div className="flex h-full items-center justify-center px-4 text-center text-xs text-muted-foreground">
        File is too large to edit inline (
        {Math.round(loadedFile.contents.length / 1024 / 1024)} MB).
        <br />
        Open it in an external editor.
      </div>
    );
  }

  // Wrap the editor with DiffCommentOverlay + `data-code-path` to
  // keep parity with the multibuffer / diff panels. NOTE for v1.1:
  // the overlay's "+" line-hover walks @pierre/diffs' shadow-DOM
  // `data-line` attributes — CM6 doesn't emit those, so the "+"
  // never appears over this editor. Documented regression; the
  // wrapper stays so the data-code-path attribute is in place for
  // a future CM6-aware overlay (`view.posAtCoords` + `lineBlockAt`).
  //
  // Both wrappers below MUST carry the full width chain
  // (`flex min-w-0 flex-1 flex-col`) so the CodeMirror container
  // inside `LazyCodeEditor` can measure a width that's bounded by
  // the panel's actual width. Without `min-w-0` and the flex chain,
  // CM6's `.cm-content` intrinsic width (long markdown lines, code
  // tokens) propagates back up through plain `h-full` blocks and
  // line-wrapping fails to wrap at the panel edge — text gets
  // clipped mid-word by the `overflow-hidden` ancestor instead of
  // wrapping inside it.
  return (
    <div
      className="flex h-full min-h-0 min-w-0 flex-1 flex-col"
      data-code-path={loadedFile.path}
    >
      <DiffCommentOverlay
        sessionId={sessionId}
        surface="code"
        pathAttr="data-code-path"
        className="flex h-full min-h-0 min-w-0 flex-1 flex-col"
      >
        <React.Suspense
          fallback={
            <div className="flex h-full items-center justify-center text-[11px] text-muted-foreground">
              Loading editor…
            </div>
          }
        >
          {editorKind === "markdown" ? (
            <LazyMarkdownEditor
              key={loadedFile.path}
              path={loadedFile.path}
              projectPath={projectPath}
              initialContent={loadedFile.contents}
              theme={resolvedTheme}
              vimEnabled={vimEnabled}
              projectFiles={projectFiles}
              onSave={onSave}
              onDirtyChange={onDirtyChange}
              onLinkOpen={onOpenFile}
              onImageSaved={onImageSaved}
            />
          ) : editorKind === "excalidraw" ? (
            <LazyExcalidrawEditor
              key={loadedFile.path}
              path={
                projectPath
                  ? `${projectPath}/${loadedFile.path}`
                  : loadedFile.path
              }
              theme={resolvedTheme}
              onSave={(data) => {
                // Both arms route through the unified `onSave`;
                // host-side `handleSaveFile` dispatches `string` to
                // `writeProjectFile` and `Uint8Array` to
                // `writeProjectFileBytes`.
                void onSave(data);
              }}
              onDirty={() => onDirtyChange(true)}
            />
          ) : editorKind === "html" ? (
            <LazyHtmlEditor
              key={loadedFile.path}
              path={loadedFile.path}
              initialContent={loadedFile.contents}
              theme={resolvedTheme}
              vimEnabled={vimEnabled}
              gitModeEnabled={gitModeEnabled}
              projectPath={projectPath}
              sessionId={sessionId}
              onSave={onSave}
              onDirtyChange={onDirtyChange}
            />
          ) : (
            <LazyCodeEditor
              key={loadedFile.path}
              path={loadedFile.path}
              initialContent={loadedFile.contents}
              theme={resolvedTheme}
              vimEnabled={vimEnabled}
              gitModeEnabled={gitModeEnabled}
              projectPath={projectPath}
              sessionId={sessionId}
              onSave={onSave}
              onDirtyChange={onDirtyChange}
            />
          )}
        </React.Suspense>
      </DiffCommentOverlay>
    </div>
  );
});

// ──────────────────────────────────────────────────────────────
// TreeDragHandle — mirrors PanelDragHandle in chat-view.tsx. The
// tree sits on the RIGHT side of the code view; this handle lives
// at its left edge (between the viewer and the tree). Width grows
// as the user drags LEFT — measured from the container's right
// edge. Persisted to localStorage on mouse-up.
// ──────────────────────────────────────────────────────────────

function TreeDragHandle({
  containerRef,
  width,
  onResize,
}: {
  containerRef: React.RefObject<HTMLDivElement | null>;
  width: number;
  onResize: (w: number) => void;
}) {
  const draggingRef = React.useRef(false);
  const latestWidthRef = React.useRef(width);

  React.useEffect(() => {
    latestWidthRef.current = width;
  }, [width]);

  React.useEffect(() => {
    function onMove(e: MouseEvent) {
      if (!draggingRef.current || !containerRef.current) return;
      const rect = containerRef.current.getBoundingClientRect();
      // Width = container's right edge minus mouse x. Dragging
      // leftward grows the tree (mouse moves further from right
      // edge → larger delta → wider tree). Clamped to the same
      // [MIN, MAX] window the keyboard collapse honors.
      const next = Math.max(
        TREE_MIN_WIDTH,
        Math.min(TREE_MAX_WIDTH, Math.round(rect.right - e.clientX)),
      );
      latestWidthRef.current = next;
      onResize(next);
    }
    function onUp() {
      if (!draggingRef.current) return;
      draggingRef.current = false;
      document.body.style.cursor = "";
      document.body.style.userSelect = "";
      try {
        window.localStorage.setItem(
          TREE_WIDTH_KEY,
          String(latestWidthRef.current),
        );
      } catch {
        /* storage may be unavailable */
      }
    }
    window.addEventListener("mousemove", onMove);
    window.addEventListener("mouseup", onUp);
    return () => {
      window.removeEventListener("mousemove", onMove);
      window.removeEventListener("mouseup", onUp);
    };
  }, [containerRef, onResize]);

  return (
    <div
      role="separator"
      aria-label="Resize file tree"
      aria-orientation="vertical"
      className="w-1 shrink-0 cursor-col-resize bg-border/50 hover:bg-border"
      onMouseDown={(e) => {
        e.preventDefault();
        draggingRef.current = true;
        document.body.style.cursor = "col-resize";
        document.body.style.userSelect = "none";
      }}
    />
  );
}
