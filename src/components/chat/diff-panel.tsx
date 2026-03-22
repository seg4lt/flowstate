import * as React from "react";
import { MultiFileDiff } from "@pierre/diffs/react";
import { ChevronDown, ChevronRight, Maximize2, Minimize2, X } from "lucide-react";
import { Button } from "@/components/ui/button";
import { getGitDiffFile, type GitFileContents } from "@/lib/api";
import type { AggregatedFileDiff } from "@/lib/session-diff";

export type DiffStyle = "split" | "unified";

// Any file with more than this many changed lines starts collapsed
// so opening the panel on a huge session doesn't fire off a bunch
// of heavy renders. Users can click the header to expand a
// collapsed file; small files stay expanded by default.
const LARGE_FILE_LINE_THRESHOLD = 500;

interface DiffPanelProps {
  projectPath: string | null;
  diffs: AggregatedFileDiff[];
  // Bumped by ChatView whenever the on-disk state may have changed
  // (turn_completed, panel toggle, etc.). Per-file cache entries
  // tagged with an older refreshKey are considered stale and
  // refetched when the section is visible.
  refreshKey: number;
  style: DiffStyle;
  onStyleChange: (s: DiffStyle) => void;
  onClose: () => void;
  isFullscreen: boolean;
  onToggleFullscreen: () => void;
}

type CacheEntry =
  | { kind: "loading"; refreshKey: number }
  | { kind: "ready"; contents: GitFileContents; refreshKey: number }
  | { kind: "error"; message: string; refreshKey: number };

// All-files-expanded layout with lazy mounting:
//   * Every changed file shows up immediately as a sticky header
//     with path + line stats (cheap — no content fetch, no Shiki).
//   * Each file's <MultiFileDiff> body mounts only when its section
//     scrolls into the viewport, via IntersectionObserver. Once
//     mounted, it stays mounted (cached).
//   * First few files are visible on initial render, so IO fires
//     immediately and they start loading right away. Files further
//     down only mount when the user actually scrolls to them.
//   * Content fetches are cached per-path and tagged with the
//     parent's `refreshKey`; a refresh invalidates entries below
//     the new key so visible sections refetch automatically.
export function DiffPanel({
  projectPath,
  diffs,
  refreshKey,
  style,
  onStyleChange,
  onClose,
  isFullscreen,
  onToggleFullscreen,
}: DiffPanelProps) {
  // Cache lives in a ref so mutations don't force re-renders of the
  // whole tree. `cacheVersion` state triggers the re-render we DO
  // want when a particular entry changes status. Crucially,
  // cacheVersion is NOT a dep of `ensureLoaded` so bumping it
  // doesn't recreate the callback and cascade re-renders everywhere.
  const cacheRef = React.useRef<Map<string, CacheEntry>>(new Map());
  const [, setCacheVersion] = React.useState(0);
  const bumpCacheVersion = React.useCallback(
    () => setCacheVersion((v) => v + 1),
    [],
  );

  // Stable-ish fetch entry point. Identity changes only when
  // `projectPath` or `refreshKey` changes, which is exactly when we
  // want visible FileSections to re-run their effect and possibly
  // refetch. Entries whose refreshKey < current are treated as stale.
  const ensureLoaded = React.useCallback(
    (path: string) => {
      if (!projectPath) return;
      const existing = cacheRef.current.get(path);
      if (
        existing &&
        existing.refreshKey === refreshKey &&
        existing.kind !== "error"
      ) {
        return;
      }
      cacheRef.current.set(path, { kind: "loading", refreshKey });
      bumpCacheVersion();

      getGitDiffFile(projectPath, path)
        .then((contents) => {
          // If the refreshKey moved on while the fetch was in
          // flight, a newer ensureLoaded has already taken over —
          // drop the stale result rather than clobbering it.
          const current = cacheRef.current.get(path);
          if (current && current.refreshKey > refreshKey) return;
          cacheRef.current.set(path, {
            kind: "ready",
            contents,
            refreshKey,
          });
          bumpCacheVersion();
        })
        .catch((err) => {
          const current = cacheRef.current.get(path);
          if (current && current.refreshKey > refreshKey) return;
          cacheRef.current.set(path, {
            kind: "error",
            message: String(err),
            refreshKey,
          });
          bumpCacheVersion();
        });
    },
    [projectPath, refreshKey, bumpCacheVersion],
  );

  const fileCount = diffs.length;

  return (
    <div className="flex h-full flex-col">
      <header className="flex h-10 shrink-0 items-center gap-2 border-b border-border bg-background/80 px-2">
        <span className="truncate text-[11px] font-medium">
          {fileCount} {fileCount === 1 ? "file" : "files"} changed
        </span>

        <div className="ml-auto flex items-center gap-1">
          <div
            role="tablist"
            aria-label="Diff layout"
            className="flex items-center rounded-md border border-border p-0.5"
          >
            <button
              type="button"
              role="tab"
              aria-selected={style === "split"}
              onClick={() => onStyleChange("split")}
              className={
                "rounded px-2 py-0.5 text-[10px] font-medium transition-colors " +
                (style === "split"
                  ? "bg-muted text-foreground"
                  : "text-muted-foreground hover:text-foreground")
              }
            >
              Split
            </button>
            <button
              type="button"
              role="tab"
              aria-selected={style === "unified"}
              onClick={() => onStyleChange("unified")}
              className={
                "rounded px-2 py-0.5 text-[10px] font-medium transition-colors " +
                (style === "unified"
                  ? "bg-muted text-foreground"
                  : "text-muted-foreground hover:text-foreground")
              }
            >
              Unified
            </button>
          </div>
          <Button
            variant="ghost"
            size="icon-xs"
            onClick={onToggleFullscreen}
            aria-label={isFullscreen ? "Exit fullscreen" : "Enter fullscreen"}
            title={isFullscreen ? "Exit fullscreen" : "Enter fullscreen"}
          >
            {isFullscreen ? (
              <Minimize2 className="h-3 w-3" />
            ) : (
              <Maximize2 className="h-3 w-3" />
            )}
          </Button>
          <Button
            variant="ghost"
            size="icon-xs"
            onClick={onClose}
            aria-label="Close diff panel"
          >
            <X className="h-3 w-3" />
          </Button>
        </div>
      </header>

      <div className="min-h-0 flex-1 overflow-auto">
        {fileCount === 0 ? (
          <div className="flex h-full items-center justify-center px-4 text-center text-xs text-muted-foreground">
            No changes in this session yet.
          </div>
        ) : (
          diffs.map((d) => (
            <FileSection
              key={d.path}
              diff={d}
              state={cacheRef.current.get(d.path)}
              ensureLoaded={ensureLoaded}
              style={style}
            />
          ))
        )}
      </div>
    </div>
  );
}

interface FileSectionProps {
  diff: AggregatedFileDiff;
  state: CacheEntry | undefined;
  ensureLoaded: (path: string) => void;
  style: DiffStyle;
}

// Each file section watches its own visibility via IntersectionObserver.
// Once it's been seen, it stays "mounted" (hasBeenVisible flips and
// never goes back) so scrolling away doesn't throw away the rendered
// diff — users expect scroll-back to be instant.
//
// Wrapped in React.memo so chat-side re-renders that don't actually
// change this section's props (most of them) don't recompute or
// re-tokenize anything.
const FileSection = React.memo(function FileSection({
  diff,
  state,
  ensureLoaded,
  style,
}: FileSectionProps) {
  const sectionRef = React.useRef<HTMLElement>(null);
  const [hasBeenVisible, setHasBeenVisible] = React.useState(false);
  const changedLines = diff.additions + diff.deletions;

  // Per-file collapse toggle. Default is "expanded" for small files
  // and "collapsed" for anything past the threshold, so opening the
  // panel doesn't fire off big renders behind the user's back.
  // `useState(() => …)` initializes once per mount; afterwards the
  // user's click is the only thing that moves it.
  const [collapsed, setCollapsed] = React.useState<boolean>(
    () => changedLines > LARGE_FILE_LINE_THRESHOLD,
  );

  React.useEffect(() => {
    if (hasBeenVisible) return;
    const el = sectionRef.current;
    if (!el) return;

    // Use the window viewport as root — the diff panel's internal
    // scroll area shifts each section's window coordinates, so a
    // viewport-rooted observer still fires at the right time
    // without us having to plumb a scrollRoot ref through.
    const observer = new IntersectionObserver((entries) => {
      if (entries.some((e) => e.isIntersecting)) {
        setHasBeenVisible(true);
        observer.disconnect();
      }
    });
    observer.observe(el);
    return () => observer.disconnect();
  }, [hasBeenVisible]);

  // Kick off (or refresh) the fetch any time the section is both
  // visible AND expanded — no point paying the Tauri IPC cost for
  // a file the user has collapsed. `ensureLoaded` identity also
  // changes when the parent bumps refreshKey, so already-mounted
  // expanded files refetch on every `turn_completed`.
  React.useEffect(() => {
    if (!hasBeenVisible) return;
    if (collapsed) return;
    ensureLoaded(diff.path);
  }, [hasBeenVisible, collapsed, ensureLoaded, diff.path]);

  const toggleCollapsed = React.useCallback(() => {
    setCollapsed((c) => !c);
  }, []);

  return (
    <section ref={sectionRef} className="border-b border-border/50">
      <button
        type="button"
        onClick={toggleCollapsed}
        aria-expanded={!collapsed}
        className="sticky top-0 z-10 flex w-full items-center gap-2 border-b border-border/40 bg-background/95 px-2 py-1.5 text-left backdrop-blur hover:bg-muted/40"
        title={diff.path}
      >
        {collapsed ? (
          <ChevronRight className="h-3 w-3 shrink-0 text-muted-foreground" />
        ) : (
          <ChevronDown className="h-3 w-3 shrink-0 text-muted-foreground" />
        )}
        <span className="truncate font-mono text-[11px]">{diff.path}</span>
        <span className="ml-auto shrink-0 tabular-nums text-[10px]">
          <span className="text-green-600 dark:text-green-400">
            +{diff.additions}
          </span>
          {" "}
          <span className="text-red-600 dark:text-red-400">
            −{diff.deletions}
          </span>
        </span>
      </button>
      {!collapsed && (
        <DiffBody
          path={diff.path}
          state={state}
          style={style}
          hasBeenVisible={hasBeenVisible}
        />
      )}
    </section>
  );
});

interface DiffBodyProps {
  path: string;
  state: CacheEntry | undefined;
  style: DiffStyle;
  hasBeenVisible: boolean;
}

// Placeholder height when a file hasn't been seen yet. Gives the
// section enough vertical presence for IntersectionObserver to
// fire even though the real MultiFileDiff isn't mounted.
const UNMOUNTED_PLACEHOLDER_HEIGHT = 120;

// Memoized so an unrelated parent re-render that doesn't change
// path/state/style/visibility (which is most of them) doesn't blow
// away the mounted MultiFileDiff. This is what keeps the surrounding
// chat UI responsive while the diff pane is open.
const DiffBody = React.memo(function DiffBody({
  path,
  state,
  style,
  hasBeenVisible,
}: DiffBodyProps) {
  if (!hasBeenVisible) {
    return (
      <div
        aria-hidden
        style={{ minHeight: UNMOUNTED_PLACEHOLDER_HEIGHT }}
        className="border-t border-transparent"
      />
    );
  }
  if (!state || state.kind === "loading") {
    return (
      <div
        style={{ minHeight: UNMOUNTED_PLACEHOLDER_HEIGHT }}
        className="flex items-center justify-center text-[11px] text-muted-foreground"
      >
        Loading diff…
      </div>
    );
  }
  if (state.kind === "error") {
    return (
      <div
        style={{ minHeight: UNMOUNTED_PLACEHOLDER_HEIGHT }}
        className="flex items-center justify-center px-3 text-center text-[11px] text-destructive"
      >
        Failed to load diff: {state.message}
      </div>
    );
  }
  // cacheKey is what makes the @pierre/diffs LRU actually do its
  // job — without it, `getFileResultCache` returns undefined and
  // every mount re-tokenizes from scratch. Keying on
  // `path::refreshKey::side` means same-content within a refresh
  // tick shares the cache, so reopening the panel on an unchanged
  // diff hits the cache instead of re-tokenizing. Bumping
  // refreshKey (turn_completed, manual refresh, branch checkout)
  // produces a new key — old entries age out via the LRU.
  return (
    <MultiFileDiff
      key={`${path}::${style}`}
      oldFile={{
        name: path,
        contents: state.contents.before,
        cacheKey: `${path}::${state.refreshKey}::before`,
      }}
      newFile={{
        name: path,
        contents: state.contents.after,
        cacheKey: `${path}::${state.refreshKey}::after`,
      }}
      options={{
        diffStyle: style,
        theme: { dark: "pierre-dark", light: "pierre-light" },
        themeType: "system",
        diffIndicators: "classic",
        overflow: "scroll",
        // Skip character-level intra-line diffing on very long
        // lines (minified JS, lockfile rows, …) — the underlying
        // algorithm is O(N*M) in line length.
        maxLineDiffLength: 2_000,
        // Skip Shiki tokenisation on absurdly long lines; the
        // diff still renders, just as plain text for that line.
        tokenizeMaxLineLength: 5_000,
      }}
    />
  );
});
