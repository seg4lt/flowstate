import * as React from "react";
import { ExternalLink } from "lucide-react";
import { MultiFileDiff } from "@pierre/diffs/react";
import type { FileDiffMetadata } from "@pierre/diffs/react";
import { Button } from "@/components/ui/button";
import { readProjectFile, type ContentBlock } from "@/lib/api";
import { useTheme } from "@/hooks/use-theme";
import { DiffCommentOverlay } from "@/components/chat/diff-comment-overlay";

// Zed-style multibuffer for content-search results, rendered via
// `@pierre/diffs` MultiFileDiff. Trick: we build a synthetic "before"
// file = the real file with the matched lines removed, and pass the
// real file as "after". The diff library then naturally renders
// every match line as an "added" line (tinted) with ±N lines of
// surrounding context, collapses unchanged regions far from any
// match into expandable `…` separators, and tokenises the whole
// file at once — which gives correct syntax highlighting context
// for free (multi-line strings, block comments, etc.) plus shares
// the worker pool with the diff panel and file viewer.
//
// Custom header (`renderCustomHeader`) replaces the default
// `+N -0` stats with `[Open] | path | N matches` so the abuse of
// the diff API doesn't leak into the UI labels.

interface MultibufferProps {
  query: string;
  blocks: ContentBlock[];
  searching: boolean;
  error: string | null;
  projectPath: string | null;
  onOpenFile: (path: string) => void;
  /** Active session id used by the review-comment overlay. When null
   *  (e.g. no active chat session) the overlay is disabled and the
   *  multibuffer renders exactly as it did before. */
  sessionId: string | null;
}

interface FileGroup {
  path: string;
  /** 1-based line numbers that matched the query inside this file. */
  matchLines: Set<number>;
}

export function Multibuffer({
  query,
  blocks,
  searching,
  error,
  projectPath,
  onOpenFile,
  sessionId,
}: MultibufferProps) {
  // Suppress "unused" warning — query is kept on props for symmetry
  // with the rest of the picker plumbing in case we want to thread
  // it into per-line annotations later (e.g. yellow underline of
  // the matched substring inside a tinted line).
  void query;

  const groups = React.useMemo<FileGroup[]>(() => {
    if (blocks.length === 0) return [];
    const byPath = new Map<string, FileGroup>();
    const order: string[] = [];
    for (const block of blocks) {
      let group = byPath.get(block.path);
      if (!group) {
        group = { path: block.path, matchLines: new Set() };
        byPath.set(block.path, group);
        order.push(block.path);
      }
      for (const line of block.lines) {
        if (line.isMatch) group.matchLines.add(line.line);
      }
    }
    return order.map((p) => byPath.get(p)!);
  }, [blocks]);

  if (error) {
    return (
      <div className="flex h-full items-center justify-center px-4 text-center text-xs text-destructive">
        {error}
      </div>
    );
  }
  if (searching && blocks.length === 0) {
    return (
      <div className="flex h-full items-center justify-center text-[11px] text-muted-foreground">
        Searching…
      </div>
    );
  }
  if (blocks.length === 0) {
    return (
      <div className="flex h-full items-center justify-center px-4 text-center text-[11px] text-muted-foreground">
        No matches.
      </div>
    );
  }
  if (!projectPath) {
    return (
      <div className="flex h-full items-center justify-center px-4 text-center text-[11px] text-muted-foreground">
        No project for this session.
      </div>
    );
  }

  return (
    <div className="h-full overflow-auto">
      {groups.map((group) => (
        <FileMatchGroup
          key={group.path}
          path={group.path}
          matchLines={group.matchLines}
          projectPath={projectPath}
          onOpenFile={onOpenFile}
          sessionId={sessionId}
        />
      ))}
    </div>
  );
}

interface FileMatchGroupProps {
  path: string;
  matchLines: Set<number>;
  projectPath: string;
  onOpenFile: (path: string) => void;
  sessionId: string | null;
}

// Min height of an unmounted file group placeholder. Big enough
// that IntersectionObserver can fire on it (scroll containers
// with zero-height children sometimes never trigger an
// intersection event), small enough that 50 collapsed groups
// don't push the viewer beyond a screen.
const PLACEHOLDER_MIN_HEIGHT = 56;

// Per-file group: lazy-fetches the full file ONLY when the group's
// section scrolls into the viewport (IntersectionObserver), then
// builds the synthetic before/after pair and hands them to
// MultiFileDiff. Without the IO gate, opening a search with 50
// matched files would fire 50 parallel `readProjectFile` calls
// the moment results land — wasteful for files the user never
// scrolls to. React.memo so chat-side re-renders or other group
// fetches don't recompute this one.
const FileMatchGroup = React.memo(function FileMatchGroup({
  path,
  matchLines,
  projectPath,
  onOpenFile,
  sessionId,
}: FileMatchGroupProps) {
  const { resolvedTheme } = useTheme();
  const sectionRef = React.useRef<HTMLElement>(null);
  const [hasBeenVisible, setHasBeenVisible] = React.useState(false);
  const [contents, setContents] = React.useState<string | null>(null);
  const [loadError, setLoadError] = React.useState<string | null>(null);

  // Same viewport-rooted IO pattern as the diff panel's file
  // sections. `rootMargin: "200px 0px"` pre-warms the next group
  // a little ahead of the scroll so the user rarely sees a bare
  // placeholder when they reach it.
  React.useEffect(() => {
    if (hasBeenVisible) return;
    const el = sectionRef.current;
    if (!el) return;
    const observer = new IntersectionObserver(
      (entries) => {
        if (entries.some((e) => e.isIntersecting)) {
          setHasBeenVisible(true);
          observer.disconnect();
        }
      },
      { rootMargin: "200px 0px" },
    );
    observer.observe(el);
    return () => observer.disconnect();
  }, [hasBeenVisible]);

  React.useEffect(() => {
    if (!hasBeenVisible) return;
    let cancelled = false;
    setContents(null);
    setLoadError(null);
    readProjectFile(projectPath, path)
      .then((c) => {
        if (cancelled) return;
        setContents(c);
      })
      .catch((err) => {
        if (cancelled) return;
        setLoadError(String(err));
      });
    return () => {
      cancelled = true;
    };
  }, [hasBeenVisible, projectPath, path]);

  // Construct the "before" file by dropping the matched lines from
  // the real file. That makes every match render as an "added"
  // line in the resulting diff, which is exactly the highlight we
  // want — and the surrounding lines stay as context.
  const synthetic = React.useMemo(() => {
    if (!contents) return null;
    const lines = contents.split("\n");
    const beforeLines: string[] = [];
    for (let i = 0; i < lines.length; i++) {
      const oneBased = i + 1;
      if (!matchLines.has(oneBased)) {
        beforeLines.push(lines[i]);
      }
    }
    return {
      before: beforeLines.join("\n"),
      after: contents,
    };
  }, [contents, matchLines]);

  const matchCount = matchLines.size;
  const basename = path.includes("/")
    ? path.slice(path.lastIndexOf("/") + 1)
    : path;
  const dirname = path.includes("/")
    ? path.slice(0, path.lastIndexOf("/"))
    : "";

  // `renderCustomHeader` callback identity matters for memo — keep
  // it stable per (path, matchCount) so the diff library doesn't
  // see a fresh function on every render and tear down state.
  const renderCustomHeader = React.useCallback(
    (_meta: FileDiffMetadata) => (
      <div className="flex items-center gap-2 px-2 py-1.5">
        <Button
          variant="outline"
          size="xs"
          onClick={() => onOpenFile(path)}
          title={`Open ${path}`}
        >
          <ExternalLink className="h-3 w-3" />
          Open
        </Button>
        <span
          className="min-w-0 truncate font-mono text-[11px] text-foreground"
          title={path}
        >
          {basename}
        </span>
        {dirname && (
          <span
            className="min-w-0 truncate font-mono text-[10px] text-muted-foreground/70"
            title={path}
          >
            {dirname}
          </span>
        )}
        <span className="ml-auto shrink-0 tabular-nums text-[10px] text-muted-foreground">
          {matchCount} {matchCount === 1 ? "match" : "matches"}
        </span>
      </div>
    ),
    [path, basename, dirname, matchCount, onOpenFile],
  );

  // Lightweight always-visible header so the user sees the file
  // path + match count even before the group has scrolled into
  // view (or while it's still fetching). The full custom header
  // inside MultiFileDiff replaces this once the diff mounts; we
  // use it for the placeholder + error states.
  const placeholderHeader = (
    <div className="flex items-center gap-2 border-b border-border/40 bg-background/95 px-2 py-1.5">
      <Button
        variant="outline"
        size="xs"
        onClick={() => onOpenFile(path)}
        title={`Open ${path}`}
      >
        <ExternalLink className="h-3 w-3" />
        Open
      </Button>
      <span
        className="min-w-0 truncate font-mono text-[11px] text-foreground"
        title={path}
      >
        {basename}
      </span>
      {dirname && (
        <span
          className="min-w-0 truncate font-mono text-[10px] text-muted-foreground/70"
          title={path}
        >
          {dirname}
        </span>
      )}
      <span className="ml-auto shrink-0 tabular-nums text-[10px] text-muted-foreground">
        {matchCount} {matchCount === 1 ? "match" : "matches"}
      </span>
    </div>
  );

  if (loadError) {
    return (
      <section
        ref={sectionRef}
        className="border-b border-border/50"
        data-search-path={path}
      >
        {placeholderHeader}
        <div
          style={{ minHeight: PLACEHOLDER_MIN_HEIGHT }}
          className="flex items-center justify-center px-3 text-[10px] text-destructive"
        >
          {loadError}
        </div>
      </section>
    );
  }
  if (!hasBeenVisible || !synthetic) {
    return (
      <section
        ref={sectionRef}
        className="border-b border-border/50"
        data-search-path={path}
      >
        {placeholderHeader}
        <div
          aria-hidden
          style={{ minHeight: PLACEHOLDER_MIN_HEIGHT }}
          className={
            hasBeenVisible
              ? "flex items-center justify-center text-[10px] text-muted-foreground"
              : ""
          }
        >
          {hasBeenVisible ? "Loading…" : null}
        </div>
      </section>
    );
  }

  return (
    <section
      ref={sectionRef}
      className="border-b border-border/50"
      data-search-path={path}
    >
      <DiffCommentOverlay
        sessionId={sessionId}
        surface="search"
        pathAttr="data-search-path"
      >
      <MultiFileDiff
        oldFile={{ name: path, contents: synthetic.before }}
        newFile={{ name: path, contents: synthetic.after }}
        renderCustomHeader={renderCustomHeader}
        options={{
          // Search results are always unified — the synthetic
          // before file is meaningless to look at on its own.
          diffStyle: "unified",
          theme: { dark: "pierre-dark", light: "pierre-light" },
          themeType: resolvedTheme,
          // Drop the +/- gutter prefix; the line tint already
          // signals a match and the prefix would look diff-y.
          diffIndicators: "none",
          // Critical: leave this FALSE so the renderer only
          // emits matched hunks + their ±3 lines of context
          // and collapses the unchanged regions outside the
          // hunks into clickable expand affordances. Setting it
          // to true makes the library render the entire file,
          // which is the "I shouldn't see a whole file" bug.
          expandUnchanged: false,
          // How many lines a single click on the expand
          // affordance reveals at a time. 100 is the library
          // default and it's a sane click-count vs. context
          // tradeoff for "show me a bit more around this hit".
          expansionLineCount: 100,
          overflow: "scroll",
          tokenizeMaxLineLength: 5_000,
          // Same line-diff defenses as the diff panel.
          maxLineDiffLength: 2_000,
        }}
      />
      </DiffCommentOverlay>
    </section>
  );
});
