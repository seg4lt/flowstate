import * as React from "react";
import { useNavigate } from "@tanstack/react-router";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import {
  ArrowLeft,
  CaseSensitive,
  FileText,
  Maximize2,
  Minimize2,
  PanelRight,
  PanelRightClose,
  Regex,
  RefreshCw,
  Search,
  SlidersHorizontal,
  Sparkles,
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
  type ContentBlock,
  type ContentSearchOptions,
} from "@/lib/api";
import { projectFilesQueryOptions } from "@/lib/queries";
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
   *   * Use `h-full` so the host's flex container governs height
   *     (the standalone route uses `h-svh`).
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
  const [highlightedIndex, setHighlightedIndex] = React.useState(0);
  const [contentBlocks, setContentBlocks] = React.useState<ContentBlock[]>([]);
  const [contentSearching, setContentSearching] = React.useState(false);
  const [contentSearchError, setContentSearchError] = React.useState<
    string | null
  >(null);
  const [contentOptions, setContentOptions] =
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
  const [useFuzzyFiles, setUseFuzzyFiles] = React.useState<boolean>(() => {
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
  const [multibufferOverride, setMultibufferOverride] = React.useState(false);

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

  // Re-sync search mode when the route's `mode` search param changes
  // while this view is already mounted. Pressing ⌘⇧F from /code/$id
  // (already on the route) pushes a new search-param value but
  // doesn't remount this component, so without this effect the second
  // press would be a silent no-op. Focus + select the input on every
  // change so the keypress feels like the in-view shortcut path
  // (Cmd+P / Cmd+Shift+F handlers below).
  React.useEffect(() => {
    if (!props.initialSearchMode) return;
    setSearchMode(props.initialSearchMode);
    queueMicrotask(() => {
      inputRef.current?.focus();
      inputRef.current?.select();
    });
  }, [props.initialSearchMode]);

  // Embedded-mode counterpart: re-sync mode + focus when the parent
  // dispatches a fresh `searchRequest` object. Same semantics as
  // the URL-driven effect above — Cmd+P and Cmd+Shift+F from the
  // chat view route through here so each press lands the cursor in
  // the input ready to type.
  React.useEffect(() => {
    if (!props.searchRequest) return;
    setSearchMode(props.searchRequest.mode);
    queueMicrotask(() => {
      inputRef.current?.focus();
      inputRef.current?.select();
    });
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
  const filteredFiles = pickerMatch.rows;
  const pickerTotalMatches = pickerMatch.total;

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

  // Aggregate match count across all blocks for the header badge.
  const contentMatchCount = React.useMemo(() => {
    let n = 0;
    for (const b of contentBlocks) {
      for (const l of b.lines) if (l.isMatch) n++;
    }
    return n;
  }, [contentBlocks]);

  // Show the multibuffer (in place of the single-pane viewer) when
  // a content search is actively producing results. Split layouts
  // always show file viewers — the multibuffer only takes over the
  // single-pane case so we never pre-empt a deliberately opened
  // tab in the non-focused pane.
  const focusedPane: PaneState =
    layout.panes[layout.focusedPaneIndex] ?? layout.panes[0]!;
  const noActiveTabInFocusedPane = focusedPane.activePath === null;
  const isSplit = layout.panes.length === 2 && layout.split !== null;
  const showMultibuffer =
    !isSplit &&
    searchMode === "content" &&
    query.trim().length > 0 &&
    (noActiveTabInFocusedPane || multibufferOverride);

  // Whether to surface a "back to N matches" link in the viewer
  // header (only when the user has a file open from the multibuffer
  // and content matches still exist).
  const canReturnToMultibuffer =
    !isSplit &&
    !multibufferOverride &&
    searchMode === "content" &&
    query.trim().length > 0 &&
    !noActiveTabInFocusedPane &&
    contentBlocks.length > 0;

  // Result count depending on mode — used for keyboard nav bounds.
  // Multibuffer mode doesn't need keyboard nav over matches (the
  // user clicks Open in a chunk header), so content mode reports 0.
  const resultCount = searchMode === "files" ? filteredFiles.length : 0;

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
    async (path: string, pane: PaneIndex, contents: string): Promise<void> => {
      if (!projectPath) {
        throw new Error("no project");
      }
      try {
        await writeProjectFile(projectPath, path, contents);
      } catch (err) {
        toast({
          title: "Save failed",
          description: String(err),
          duration: 5000,
        });
        throw err;
      }
      // Re-baseline the file cache so reopening this tab uses the
      // new content (no re-fetch, no flash of stale text). Bump the
      // cacheKey via hashContent so any future @pierre/diffs LRU
      // (still used for diffs) re-keys cleanly.
      fileCacheRef.current.set(path, {
        path,
        contents,
        cacheKey: `${path}::${hashContent(contents)}`,
      });
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
      return (
        tag === "INPUT" ||
        tag === "TEXTAREA" ||
        target.isContentEditable === true
      );
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
  const focusActiveEditor = React.useCallback(() => {
    const path = focusedPane.activePath;
    if (!path) return;
    const wrapper = document.querySelector(
      `[data-code-path="${CSS.escape(path)}"]`,
    );
    const content = wrapper?.querySelector(".cm-content") as HTMLElement | null;
    content?.focus();
  }, [focusedPane.activePath]);

  function openFromPickerIndex(index: number) {
    // Files mode only — content mode uses the multibuffer, where
    // Enter on the input is a no-op (user clicks Open per chunk).
    if (searchMode !== "files") return;
    const pick = filteredFiles[index] ?? filteredFiles[0];
    if (pick) {
      tabs.openFile(pick);
      setMultibufferOverride(false);
      setQuery("");
      inputRef.current?.blur();
      // Hand focus to the just-opened file. queueMicrotask defers
      // until after React commits the new tab, so the editor's
      // `.cm-content` exists by the time we focus it.
      queueMicrotask(focusActiveEditor);
    }
  }

  function handleInputKeyDown(e: React.KeyboardEvent<HTMLInputElement>) {
    if (resultCount === 0) {
      if (e.key === "Escape") {
        setQuery("");
        inputRef.current?.blur();
        focusActiveEditor();
      }
      return;
    }
    if (e.key === "ArrowDown") {
      e.preventDefault();
      setHighlightedIndex((i) => Math.min(resultCount - 1, i + 1));
    } else if (e.key === "ArrowUp") {
      e.preventDefault();
      setHighlightedIndex((i) => Math.max(0, i - 1));
    } else if (e.key === "Enter") {
      e.preventDefault();
      openFromPickerIndex(highlightedIndex);
    } else if (e.key === "Escape") {
      e.preventDefault();
      // Two-step Esc: first clears the query (stay in input so the
      // user can refine), second blurs + returns to editor. Mirrors
      // VS Code's command palette / quick-open behaviour.
      if (query) {
        setQuery("");
      } else {
        inputRef.current?.blur();
        focusActiveEditor();
      }
    }
  }

  const projectLabel = React.useMemo(() => {
    if (!projectPath) return null;
    const segments = projectPath.split("/").filter(Boolean);
    return segments[segments.length - 1] ?? projectPath;
  }, [projectPath]);

  return (
    <div
      className={cn(
        "flex min-w-0 flex-col overflow-hidden",
        embedded ? "h-full" : "h-svh",
      )}
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
          <div className="flex shrink-0 items-center gap-2 border-b border-border px-2 py-1.5">
            <Search className="h-3 w-3 shrink-0 text-muted-foreground" />
            <input
              ref={inputRef}
              type="text"
              value={query}
              onChange={(e) => setQuery(e.target.value)}
              onKeyDown={handleInputKeyDown}
              placeholder={
                !projectPath
                  ? "No project for this session"
                  : searchMode === "files"
                    ? "Search files…  e.g. tabs.ts  ·  src tabs.ts"
                    : "Search file contents…  (Cmd/Ctrl+Shift+F)"
              }
              disabled={!projectPath}
              className="min-w-0 flex-1 bg-transparent text-[12px] outline-none placeholder:text-muted-foreground"
            />
            <SearchModeToggle mode={searchMode} onChange={setSearchMode} />
            {searchMode === "files" && (
              // Files-mode fuzzy toggle. Subsequence + word-boundary
              // ranking via `lib/fuzzy.ts`, including IntelliJ /
              // Zed-style acronym matching ("usc" →
              // "UserServiceController.ts"). Useful when you remember
              // a few characters but not the exact substring (e.g.
              // typing `tbsv` to find `tabs-view.tsx`). On by default
              // — fuzzy still ranks contiguous substring matches
              // highest, so plain "I know the file name" jumps still
              // work. Persisted to localStorage.
              <Button
                variant="ghost"
                size="icon-xs"
                aria-pressed={useFuzzyFiles}
                onClick={() => setUseFuzzyFiles((v) => !v)}
                title="Fuzzy file match (subsequence + acronym, e.g. usc → UserServiceController)"
                aria-label="Toggle fuzzy file matching"
                className={
                  useFuzzyFiles ? "bg-muted text-foreground" : undefined
                }
              >
                <Sparkles className="h-3 w-3" />
              </Button>
            )}
            {searchMode === "content" && (
              <Button
                variant="ghost"
                size="icon-xs"
                aria-pressed={contentOptions.advancedOpen}
                onClick={() =>
                  setContentOptions((prev) => ({
                    ...prev,
                    advancedOpen: !prev.advancedOpen,
                  }))
                }
                title="Advanced search options"
                aria-label="Toggle advanced search options"
                className={
                  contentOptions.advancedOpen
                    ? "bg-muted text-foreground"
                    : undefined
                }
              >
                <SlidersHorizontal className="h-3 w-3" />
              </Button>
            )}
            <SearchStatusBadge
              mode={searchMode}
              filesLoading={filesLoading}
              filesTotal={files.length}
              filteredCount={filteredFiles.length}
              totalMatches={pickerTotalMatches}
              indexing={indexing}
              contentSearching={contentSearching}
              contentMatchCount={contentMatchCount}
            />
          </div>

          {searchMode === "content" && contentOptions.advancedOpen && (
            <ContentSearchAdvancedRow
              options={contentOptions}
              onChange={setContentOptions}
            />
          )}

          {/* Files-mode picker dropdown stays as-is — it's the
              quick Cmd+P jumper. Content-mode results are shown
              in the multibuffer below instead. */}
          {searchMode === "files" && query && (
            <div className="max-h-72 shrink-0 overflow-auto border-b border-border bg-background/95">
              <FilePickerResults
                results={filteredFiles}
                highlightedIndex={highlightedIndex}
                onHover={setHighlightedIndex}
                onPick={(p) => {
                  tabs.openFile(p);
                  setMultibufferOverride(false);
                  setQuery("");
                  inputRef.current?.blur();
                }}
              />
            </div>
          )}

          {canReturnToMultibuffer && (
            <button
              type="button"
              onClick={() => setMultibufferOverride(true)}
              className="flex shrink-0 items-center gap-1 border-b border-border bg-muted/30 px-3 py-1 text-left text-[10px] text-muted-foreground hover:bg-muted/60 hover:text-foreground"
            >
              <ArrowLeft className="h-3 w-3" />
              Back to {contentMatchCount}{" "}
              {contentMatchCount === 1 ? "match" : "matches"} for "{query}"
            </button>
          )}

          <div className="min-h-0 flex-1 overflow-hidden">
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

interface SearchModeToggleProps {
  mode: SearchMode;
  onChange: (m: SearchMode) => void;
}

interface ContentSearchAdvancedRowProps {
  options: ContentSearchUiOptions;
  onChange: React.Dispatch<React.SetStateAction<ContentSearchUiOptions>>;
}

// Second row that appears below the search bar when the user
// clicks the SlidersHorizontal toggle in content-search mode.
// Contains the include + exclude glob inputs and the regex /
// case-sensitivity toggles. Same comma-separated glob syntax
// as the file picker (passed through splitGlobList on its way
// to the rust side).
function ContentSearchAdvancedRow({
  options,
  onChange,
}: ContentSearchAdvancedRowProps) {
  return (
    <div className="flex shrink-0 items-center gap-2 border-b border-border bg-background/60 px-2 py-1.5">
      <input
        type="text"
        value={options.include}
        onChange={(e) =>
          onChange((prev) => ({ ...prev, include: e.target.value }))
        }
        placeholder="include: src/**/*.ts, !*.test.ts"
        className="min-w-0 flex-1 rounded border border-border bg-background px-2 py-0.5 text-[11px] outline-none placeholder:text-muted-foreground/70 focus:border-foreground/30"
      />
      <input
        type="text"
        value={options.exclude}
        onChange={(e) =>
          onChange((prev) => ({ ...prev, exclude: e.target.value }))
        }
        placeholder="exclude: node_modules/**, *.lock"
        className="min-w-0 flex-1 rounded border border-border bg-background px-2 py-0.5 text-[11px] outline-none placeholder:text-muted-foreground/70 focus:border-foreground/30"
      />
      <Button
        variant="ghost"
        size="icon-xs"
        aria-pressed={options.useRegex}
        // Fuzzy mode wins over regex on the Rust side (see the
        // `build_query_plan` precedence table in file_index.rs), so
        // disable the regex toggle while fuzzy is on to make the
        // mutual-exclusivity visible. Otherwise users see the regex
        // button "pressed" but get fuzzy results — confusing.
        disabled={options.useFuzzy}
        onClick={() =>
          onChange((prev) => ({
            ...prev,
            useRegex: !prev.useRegex,
          }))
        }
        title={
          options.useFuzzy ? "Disabled while fuzzy is on" : "Use regex (.*)"
        }
        aria-label="Toggle regex matching"
        className={options.useRegex ? "bg-muted text-foreground" : undefined}
      >
        <Regex className="h-3 w-3" />
      </Button>
      <Button
        variant="ghost"
        size="icon-xs"
        aria-pressed={options.useFuzzy}
        onClick={() =>
          onChange((prev) => ({
            ...prev,
            useFuzzy: !prev.useFuzzy,
            // Flipping fuzzy ON forces regex OFF — the Rust precedence
            // table makes regex+fuzzy resolve to fuzzy anyway, but we
            // sync the UI flags so the user can see what's actually
            // running.
            useRegex: !prev.useFuzzy ? false : prev.useRegex,
          }))
        }
        title="Fuzzy match (typo-tolerant, case-insensitive)"
        aria-label="Toggle fuzzy matching"
        className={options.useFuzzy ? "bg-muted text-foreground" : undefined}
      >
        <Sparkles className="h-3 w-3" />
      </Button>
      <Button
        variant="ghost"
        size="icon-xs"
        aria-pressed={options.caseSensitive}
        // Fuzzy mode is inherently case-insensitive on the Rust side,
        // so this toggle is a no-op while fuzzy is on. Disable to make
        // that obvious rather than letting the press do nothing.
        disabled={options.useFuzzy}
        onClick={() =>
          onChange((prev) => ({
            ...prev,
            caseSensitive: !prev.caseSensitive,
          }))
        }
        title={
          options.useFuzzy
            ? "Fuzzy is always case-insensitive"
            : "Case sensitive (aA)"
        }
        aria-label="Toggle case sensitivity"
        className={
          options.caseSensitive ? "bg-muted text-foreground" : undefined
        }
      >
        <CaseSensitive className="h-3 w-3" />
      </Button>
    </div>
  );
}

function SearchModeToggle({ mode, onChange }: SearchModeToggleProps) {
  return (
    <div
      role="tablist"
      aria-label="Search mode"
      className="flex shrink-0 items-center rounded-md border border-border p-0.5"
    >
      <button
        type="button"
        role="tab"
        aria-selected={mode === "files"}
        onClick={() => onChange("files")}
        className={
          "rounded px-2 py-0.5 text-[10px] font-medium transition-colors " +
          (mode === "files"
            ? "bg-muted text-foreground"
            : "text-muted-foreground hover:text-foreground")
        }
      >
        Files
      </button>
      <button
        type="button"
        role="tab"
        aria-selected={mode === "content"}
        onClick={() => onChange("content")}
        className={
          "rounded px-2 py-0.5 text-[10px] font-medium transition-colors " +
          (mode === "content"
            ? "bg-muted text-foreground"
            : "text-muted-foreground hover:text-foreground")
        }
      >
        Content
      </button>
    </div>
  );
}

interface SearchStatusBadgeProps {
  mode: SearchMode;
  filesLoading: boolean;
  filesTotal: number;
  filteredCount: number;
  /** Total ranked matches before the `PICKER_RESULT_LIMIT` slice.
   *  Drives the numeric "+N more" overflow hint so users know when
   *  the cap is biting and they should refine the query. */
  totalMatches: number;
  /** True while fff-search's background scanner is still walking the
   *  worktree on a cold open. Surfaces an "indexing N files…" badge
   *  alongside the live count so users know the picker is filling
   *  in. Goes away the moment the scan settles. */
  indexing: boolean;
  contentSearching: boolean;
  contentMatchCount: number;
}

function SearchStatusBadge({
  mode,
  filesLoading,
  filesTotal,
  filteredCount,
  totalMatches,
  indexing,
  contentSearching,
  contentMatchCount,
}: SearchStatusBadgeProps) {
  if (mode === "files") {
    if (filesLoading)
      return (
        <span className="shrink-0 text-[10px] text-muted-foreground">
          indexing…
        </span>
      );
    if (filesTotal === 0) return null;
    // While the cold scan is still running show "indexing N files…"
    // alongside the regular count so the live-fill is visible. We
    // do NOT replace the count — partial results are real results
    // and the user can already act on them.
    const overflow =
      filteredCount < totalMatches ? totalMatches - filteredCount : 0;
    return (
      <span className="shrink-0 tabular-nums text-[10px] text-muted-foreground">
        {filteredCount}
        {overflow > 0 ? ` +${overflow}` : ""} / {filesTotal}
        {indexing ? " · indexing…" : ""}
      </span>
    );
  }
  if (contentSearching)
    return (
      <span className="shrink-0 text-[10px] text-muted-foreground">
        searching…
      </span>
    );
  if (contentMatchCount === 0) return null;
  return (
    <span className="shrink-0 tabular-nums text-[10px] text-muted-foreground">
      {contentMatchCount} hit{contentMatchCount === 1 ? "" : "s"}
    </span>
  );
}

interface FilePickerResultsProps {
  results: string[];
  highlightedIndex: number;
  onHover: (i: number) => void;
  onPick: (path: string) => void;
}

const FilePickerResults = React.memo(function FilePickerResults({
  results,
  highlightedIndex,
  onHover,
  onPick,
}: FilePickerResultsProps) {
  if (results.length === 0) {
    return (
      <div className="px-3 py-3 text-center text-[11px] text-muted-foreground">
        No files match.
      </div>
    );
  }
  return (
    <>
      {results.map((path, i) => {
        const isHighlighted = i === highlightedIndex;
        const basename = path.includes("/")
          ? path.slice(path.lastIndexOf("/") + 1)
          : path;
        const dirname = path.includes("/")
          ? path.slice(0, path.lastIndexOf("/"))
          : "";
        return (
          <button
            key={path}
            type="button"
            // mousedown rather than click so the input doesn't lose
            // focus before the click registers (which would close
            // the dropdown via blur).
            onMouseDown={(e) => {
              e.preventDefault();
              onPick(path);
            }}
            onMouseEnter={() => onHover(i)}
            className={
              // `min-w-0 overflow-hidden` is load-bearing: without
              // them the row's intrinsic width is `icon + basename +
              // dirname` at full text width, which on narrow popout
              // windows pushes the dropdown — and the popout window
              // itself — wider than the viewport. With min-w-0 the
              // row collapses to the parent's width and the truncate
              // spans below clip cleanly via ellipsis.
              "flex w-full min-w-0 items-baseline gap-2 overflow-hidden px-3 py-1 text-left text-[11px] " +
              (isHighlighted
                ? "bg-muted text-foreground"
                : "text-muted-foreground hover:bg-muted/50")
            }
            title={path}
          >
            <FileText className="h-3 w-3 shrink-0" />
            <span className="truncate font-mono">{basename}</span>
            {dirname && (
              // `shrink-0` removed: keeping it forced the dirname to
              // its intrinsic width and made the row impossible to
              // compress on narrow popouts. `min-w-0` + `truncate`
              // lets it ellipsify instead.
              <span className="ml-auto min-w-0 truncate font-mono text-[10px] text-muted-foreground/70">
                {dirname}
              </span>
            )}
          </button>
        );
      })}
    </>
  );
});

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
   *  the file cache + tab dirty bit. */
  onSaveFile: (path: string, pane: PaneIndex, contents: string) => Promise<void>;
  /** Dirty-bit handler — bubbles up to `tabs.setTabDirty`. */
  onDirtyChangeFile: (path: string, pane: PaneIndex, dirty: boolean) => void;
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
    (contents: string) => onSaveFile(activePath ?? "", paneIndex, contents),
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
        className="relative z-0 min-h-0 flex-1 overflow-hidden"
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
          onSave={handleSave}
          onDirtyChange={handleDirty}
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
  onSave: (contents: string) => Promise<void>;
  onDirtyChange: (dirty: boolean) => void;
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
  onSave,
  onDirtyChange,
}: CodeViewBodyProps) {
  const { resolvedTheme } = useTheme();
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
  // of CM6. The 4 MiB Rust read cap is the binding constraint today,
  // but this protects against future code paths that might surface
  // larger buffers.
  if (loadedFile.contents.length > MAX_EDITABLE_BYTES) {
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
  return (
    <div className="h-full" data-code-path={loadedFile.path}>
      <DiffCommentOverlay
        sessionId={sessionId}
        surface="code"
        pathAttr="data-code-path"
        className="h-full"
      >
        <React.Suspense
          fallback={
            <div className="flex h-full items-center justify-center text-[11px] text-muted-foreground">
              Loading editor…
            </div>
          }
        >
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
