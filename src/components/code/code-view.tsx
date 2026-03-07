import * as React from "react";
import { useNavigate } from "@tanstack/react-router";
import { File as PierreFile } from "@pierre/diffs/react";
import { Virtualizer } from "@pierre/diffs/react";
import { ArrowLeft, FileText, Search } from "lucide-react";
import { SidebarTrigger } from "@/components/ui/sidebar";
import { Button } from "@/components/ui/button";
import { useApp } from "@/stores/app-store";
import {
  listProjectFiles,
  readProjectFile,
  searchFileContents,
  type ContentBlock,
} from "@/lib/api";
import { FileTree } from "./file-tree";
import { Multibuffer } from "./multibuffer";

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
//  * File listing: rust `list_project_files` (ignore crate)
//  * Content search: rust `search_file_contents` (grep-searcher
//    + grep-regex, same lineage as ignore)
//  * Syntax highlighting + virtualization: @pierre/diffs <File>
//    inside <Virtualizer>, sharing the worker pool from main.tsx

const PICKER_RESULT_LIMIT = 50;
// Trailing-edge debounce for the content-search call. 600ms is
// deliberately on the patient side — long enough that even slow
// typists don't fire a ripgrep walk per keystroke, and any
// in-flight search has time to settle before the next one
// kicks off. The effect's cleanup still cancels stale promises
// so only the latest query's results ever land.
const CONTENT_SEARCH_DEBOUNCE_MS = 600;

const TREE_WIDTH_KEY = "flowzen:code-tree-width";
const TREE_DEFAULT_WIDTH = 260;
const TREE_MIN_WIDTH = 160;
const TREE_MAX_WIDTH = 520;

type SearchMode = "files" | "content";

export function CodeView({ sessionId }: { sessionId: string }) {
  const { state } = useApp();
  const navigate = useNavigate();

  const session = state.sessions.get(sessionId);
  const projectPath = React.useMemo(() => {
    if (!session?.projectId) return null;
    return (
      state.projects.find((p) => p.projectId === session.projectId)?.path ??
      null
    );
  }, [session?.projectId, state.projects]);

  // ─── tree resize state ───────────────────────────────────────
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

  // ─── file list / picker state ────────────────────────────────
  const [files, setFiles] = React.useState<string[]>([]);
  const [filesLoading, setFilesLoading] = React.useState(false);
  const [filesError, setFilesError] = React.useState<string | null>(null);

  // ─── search state ────────────────────────────────────────────
  const [searchMode, setSearchMode] = React.useState<SearchMode>("files");
  const [query, setQuery] = React.useState("");
  const [highlightedIndex, setHighlightedIndex] = React.useState(0);
  const [contentBlocks, setContentBlocks] = React.useState<ContentBlock[]>([]);
  const [contentSearching, setContentSearching] = React.useState(false);
  const [contentSearchError, setContentSearchError] = React.useState<
    string | null
  >(null);

  // ─── viewer state ────────────────────────────────────────────
  const [selectedPath, setSelectedPath] = React.useState<string | null>(null);
  const [fileContents, setFileContents] = React.useState<string | null>(null);
  const [fileError, setFileError] = React.useState<string | null>(null);
  const [fileLoading, setFileLoading] = React.useState(false);

  const inputRef = React.useRef<HTMLInputElement>(null);

  // Reset everything and load file list when project changes.
  React.useEffect(() => {
    setSelectedPath(null);
    setFileContents(null);
    setFileError(null);
    setQuery("");
    setHighlightedIndex(0);
    setContentBlocks([]);
    setContentSearchError(null);

    if (!projectPath) {
      setFiles([]);
      setFilesError(null);
      return;
    }

    let cancelled = false;
    setFilesLoading(true);
    setFilesError(null);
    listProjectFiles(projectPath)
      .then((entries) => {
        if (cancelled) return;
        setFiles(entries);
      })
      .catch((err) => {
        if (cancelled) return;
        setFilesError(String(err));
        setFiles([]);
      })
      .finally(() => {
        if (cancelled) return;
        setFilesLoading(false);
      });

    return () => {
      cancelled = true;
    };
  }, [projectPath]);

  // ─── filename filter (client-side, instant) ─────────────────
  const filteredFiles = React.useMemo(() => {
    if (searchMode !== "files") return [];
    const q = query.trim().toLowerCase();
    if (!q) return files.slice(0, PICKER_RESULT_LIMIT);
    return files
      .filter((f) => f.toLowerCase().includes(q))
      .slice(0, PICKER_RESULT_LIMIT);
  }, [files, query, searchMode]);

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
    let cancelled = false;
    setContentSearching(true);
    setContentSearchError(null);
    const handle = window.setTimeout(() => {
      searchFileContents(projectPath, q)
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
    };
  }, [searchMode, query, projectPath]);

  // Aggregate match count across all blocks for the header badge.
  const contentMatchCount = React.useMemo(() => {
    let n = 0;
    for (const b of contentBlocks) {
      for (const l of b.lines) if (l.isMatch) n++;
    }
    return n;
  }, [contentBlocks]);

  // Show the multibuffer in the right pane when a content search
  // is actively producing results (or about to). The viewer takes
  // priority once the user opens a specific file via "Open".
  const showMultibuffer =
    searchMode === "content" &&
    query.trim().length > 0 &&
    selectedPath === null;

  // Whether to surface a "back to N matches" link in the viewer
  // header (only when the user opened a file from the multibuffer
  // and content matches still exist).
  const canReturnToMultibuffer =
    searchMode === "content" &&
    query.trim().length > 0 &&
    selectedPath !== null &&
    contentBlocks.length > 0;

  // Result count depending on mode — used for keyboard nav bounds.
  // Multibuffer mode doesn't need keyboard nav over matches (the
  // user clicks Open in a chunk header), so content mode reports 0.
  const resultCount = searchMode === "files" ? filteredFiles.length : 0;

  // Reset highlight when query / mode changes.
  React.useEffect(() => {
    setHighlightedIndex(0);
  }, [query, searchMode]);

  // ─── lazy file-content fetch on selection ───────────────────
  React.useEffect(() => {
    if (!projectPath || !selectedPath) {
      setFileContents(null);
      setFileError(null);
      return;
    }

    let cancelled = false;
    setFileLoading(true);
    setFileError(null);
    setFileContents(null);
    readProjectFile(projectPath, selectedPath)
      .then((contents) => {
        if (cancelled) return;
        setFileContents(contents);
      })
      .catch((err) => {
        if (cancelled) return;
        setFileError(String(err));
      })
      .finally(() => {
        if (cancelled) return;
        setFileLoading(false);
      });

    return () => {
      cancelled = true;
    };
  }, [projectPath, selectedPath]);

  // Cmd/Ctrl+P focuses the picker in `files` mode; Cmd/Ctrl+Shift+F
  // focuses it in `content` mode. Mirrors VS Code.
  React.useEffect(() => {
    function onKeyDown(e: KeyboardEvent) {
      const mod = e.metaKey || e.ctrlKey;
      if (!mod) return;
      if (e.shiftKey && e.key.toLowerCase() === "f") {
        e.preventDefault();
        setSearchMode("content");
        inputRef.current?.focus();
        inputRef.current?.select();
      } else if (!e.shiftKey && e.key.toLowerCase() === "p") {
        e.preventDefault();
        setSearchMode("files");
        inputRef.current?.focus();
        inputRef.current?.select();
      }
    }
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, []);

  function openFromPickerIndex(index: number) {
    // Files mode only — content mode uses the multibuffer, where
    // Enter on the input is a no-op (user clicks Open per chunk).
    if (searchMode !== "files") return;
    const pick = filteredFiles[index] ?? filteredFiles[0];
    if (pick) {
      setSelectedPath(pick);
      setQuery("");
      inputRef.current?.blur();
    }
  }

  function handleInputKeyDown(e: React.KeyboardEvent<HTMLInputElement>) {
    if (resultCount === 0) {
      if (e.key === "Escape") {
        setQuery("");
        inputRef.current?.blur();
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
      if (query) setQuery("");
      else inputRef.current?.blur();
    }
  }

  const projectLabel = React.useMemo(() => {
    if (!projectPath) return null;
    const segments = projectPath.split("/").filter(Boolean);
    return segments[segments.length - 1] ?? projectPath;
  }, [projectPath]);

  return (
    <div className="flex h-svh min-w-0 flex-col overflow-hidden">
      <header className="flex h-12 shrink-0 items-center gap-2 border-b border-border px-2 text-sm">
        <SidebarTrigger />
        <Button
          variant="ghost"
          size="xs"
          onClick={() =>
            navigate({ to: "/chat/$sessionId", params: { sessionId } })
          }
          title="Back to chat"
        >
          <ArrowLeft className="h-3 w-3" />
          Chat
        </Button>
        <div className="flex min-w-0 items-center gap-1 text-[11px] text-muted-foreground">
          {projectLabel && (
            <span className="truncate font-medium text-foreground">
              {projectLabel}
            </span>
          )}
          {selectedPath && (
            <>
              <span className="shrink-0">/</span>
              <span className="truncate font-mono" title={selectedPath}>
                {selectedPath}
              </span>
            </>
          )}
        </div>
      </header>

      <div ref={splitContainerRef} className="flex min-h-0 min-w-0 flex-1">
        {/* ── LEFT: file tree column ─────────────────────────── */}
        <aside
          className="flex shrink-0 flex-col border-r border-border bg-background"
          style={{ width: treeWidth }}
        >
          <div className="flex h-9 shrink-0 items-center gap-2 border-b border-border px-2 text-[10px] uppercase tracking-wide text-muted-foreground">
            Files
            {filesLoading && <span>· indexing…</span>}
          </div>
          <div className="min-h-0 flex-1 overflow-auto">
            {filesError ? (
              <div className="px-3 py-3 text-[11px] text-destructive">
                {filesError}
              </div>
            ) : !projectPath ? (
              <div className="px-3 py-3 text-[11px] text-muted-foreground">
                No project for this session.
              </div>
            ) : (
              <FileTree
                files={files}
                selectedPath={selectedPath}
                onSelect={(p) => {
                  setSelectedPath(p);
                  setQuery("");
                }}
              />
            )}
          </div>
        </aside>

        <TreeDragHandle
          containerRef={splitContainerRef}
          width={treeWidth}
          onResize={setTreeWidth}
        />

        {/* ── RIGHT: search + viewer column ──────────────────── */}
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
                    ? "Search files…  (Cmd/Ctrl+P)"
                    : "Search file contents…  (Cmd/Ctrl+Shift+F)"
              }
              disabled={!projectPath}
              className="min-w-0 flex-1 bg-transparent text-[12px] outline-none placeholder:text-muted-foreground"
            />
            <SearchModeToggle mode={searchMode} onChange={setSearchMode} />
            <SearchStatusBadge
              mode={searchMode}
              filesLoading={filesLoading}
              filesTotal={files.length}
              filteredCount={filteredFiles.length}
              contentSearching={contentSearching}
              contentMatchCount={contentMatchCount}
            />
          </div>

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
                  setSelectedPath(p);
                  setQuery("");
                  inputRef.current?.blur();
                }}
              />
            </div>
          )}

          {canReturnToMultibuffer && (
            <button
              type="button"
              onClick={() => setSelectedPath(null)}
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
                  setSelectedPath(p);
                }}
              />
            ) : (
              <CodeViewBody
                path={selectedPath}
                contents={fileContents}
                loading={fileLoading}
                error={fileError}
                filesError={filesError}
                hasProject={projectPath !== null}
              />
            )}
          </div>
        </div>
      </div>
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
  contentSearching: boolean;
  contentMatchCount: number;
}

function SearchStatusBadge({
  mode,
  filesLoading,
  filesTotal,
  filteredCount,
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
    return (
      <span className="shrink-0 tabular-nums text-[10px] text-muted-foreground">
        {filteredCount}
        {filteredCount === PICKER_RESULT_LIMIT && "+"} / {filesTotal}
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
              "flex w-full items-baseline gap-2 px-3 py-1 text-left text-[11px] " +
              (isHighlighted
                ? "bg-muted text-foreground"
                : "text-muted-foreground hover:bg-muted/50")
            }
            title={path}
          >
            <FileText className="h-3 w-3 shrink-0" />
            <span className="truncate font-mono">{basename}</span>
            {dirname && (
              <span className="ml-auto shrink-0 truncate font-mono text-[10px] text-muted-foreground/70">
                {dirname}
              </span>
            )}
          </button>
        );
      })}
    </>
  );
});

interface CodeViewBodyProps {
  path: string | null;
  contents: string | null;
  loading: boolean;
  error: string | null;
  filesError: string | null;
  hasProject: boolean;
}

const CodeViewBody = React.memo(function CodeViewBody({
  path,
  contents,
  loading,
  error,
  filesError,
  hasProject,
}: CodeViewBodyProps) {
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
  if (loading || contents === null) {
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
  return (
    <Virtualizer className="h-full overflow-auto">
      <PierreFile
        key={path}
        file={{ name: path, contents }}
        options={{
          theme: { dark: "pierre-dark", light: "pierre-light" },
          themeType: "system",
          overflow: "scroll",
          tokenizeMaxLineLength: 5_000,
        }}
      />
    </Virtualizer>
  );
});

// ──────────────────────────────────────────────────────────────
// TreeDragHandle — mirrors DiffDragHandle in chat-view.tsx but
// resizes from the LEFT edge instead of the right. Width grows as
// you drag right; persisted to localStorage on mouse-up.
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
      // Width = mouse x relative to container's left edge, clamped.
      const next = Math.max(
        TREE_MIN_WIDTH,
        Math.min(TREE_MAX_WIDTH, Math.round(e.clientX - rect.left)),
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
