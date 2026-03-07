import * as React from "react";
import { useNavigate } from "@tanstack/react-router";
import { File as PierreFile } from "@pierre/diffs/react";
import { Virtualizer } from "@pierre/diffs/react";
import { ArrowLeft, FileText, Search } from "lucide-react";
import { SidebarTrigger } from "@/components/ui/sidebar";
import { Button } from "@/components/ui/button";
import { useApp } from "@/stores/app-store";
import { listProjectFiles, readProjectFile } from "@/lib/api";

// MVP read-only editor view: filename picker + single-file viewer.
//
// Layout (mirrors ChatView's header + body column):
//   * Header: sidebar trigger, breadcrumb (project / open file), back-to-chat
//   * Body: search input (always visible) + results dropdown (when query
//     present) + file viewer (full remaining height)
//
// All heavy work is offloaded:
//   * File listing: rust `list_project_files` walks via `ignore` crate
//   * Syntax highlighting + virtualization: @pierre/diffs <File> wrapped
//     in <Virtualizer>, both pulling from the WorkerPoolContextProvider
//     wired up at app root in main.tsx
//
// MVP intentionally omits: tabs, file tree, content search, scroll-to-line,
// fuzzy ranking, file watcher. All listed in the plan's "next steps".

const PICKER_RESULT_LIMIT = 50;

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

  const [files, setFiles] = React.useState<string[]>([]);
  const [filesLoading, setFilesLoading] = React.useState(false);
  const [filesError, setFilesError] = React.useState<string | null>(null);
  const [query, setQuery] = React.useState("");
  const [highlightedIndex, setHighlightedIndex] = React.useState(0);
  const [selectedPath, setSelectedPath] = React.useState<string | null>(null);
  const [fileContents, setFileContents] = React.useState<string | null>(null);
  const [fileError, setFileError] = React.useState<string | null>(null);
  const [fileLoading, setFileLoading] = React.useState(false);

  const inputRef = React.useRef<HTMLInputElement>(null);

  // Load the file list once per project. Cleared and re-fetched
  // when the user navigates to a different session whose project
  // path differs.
  React.useEffect(() => {
    setSelectedPath(null);
    setFileContents(null);
    setFileError(null);
    setQuery("");
    setHighlightedIndex(0);

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

  // Substring filter on full path. Good enough for MVP — upgrade to
  // nucleo-matcher / fff-search later if users want real fuzzy
  // ranking with score-based ordering.
  const filtered = React.useMemo(() => {
    const q = query.trim().toLowerCase();
    if (!q) return files.slice(0, PICKER_RESULT_LIMIT);
    return files
      .filter((f) => f.toLowerCase().includes(q))
      .slice(0, PICKER_RESULT_LIMIT);
  }, [files, query]);

  // Reset highlight to the top of the list whenever the query
  // changes — no point in keeping a stale index after the matches
  // have shifted around.
  React.useEffect(() => {
    setHighlightedIndex(0);
  }, [query]);

  // Lazy file fetch when the user picks something. `cancelled` flag
  // guards against fast pick-spamming so we don't render an old
  // file's contents into a newer selection.
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

  // Cmd/Ctrl+P focuses the picker input from anywhere in the view.
  // Mirrors the muscle memory users already have from VS Code.
  React.useEffect(() => {
    function onKeyDown(e: KeyboardEvent) {
      if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === "p") {
        e.preventDefault();
        inputRef.current?.focus();
        inputRef.current?.select();
      }
    }
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, []);

  function handleInputKeyDown(e: React.KeyboardEvent<HTMLInputElement>) {
    if (filtered.length === 0) return;
    if (e.key === "ArrowDown") {
      e.preventDefault();
      setHighlightedIndex((i) => Math.min(filtered.length - 1, i + 1));
    } else if (e.key === "ArrowUp") {
      e.preventDefault();
      setHighlightedIndex((i) => Math.max(0, i - 1));
    } else if (e.key === "Enter") {
      e.preventDefault();
      const pick = filtered[highlightedIndex] ?? filtered[0];
      if (pick) {
        setSelectedPath(pick);
        inputRef.current?.blur();
      }
    } else if (e.key === "Escape") {
      e.preventDefault();
      if (query) {
        setQuery("");
      } else {
        inputRef.current?.blur();
      }
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
          onClick={() => navigate({ to: "/chat/$sessionId", params: { sessionId } })}
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

      <div className="relative flex shrink-0 items-center gap-2 border-b border-border px-2 py-1.5">
        <Search className="h-3 w-3 shrink-0 text-muted-foreground" />
        <input
          ref={inputRef}
          type="text"
          value={query}
          onChange={(e) => setQuery(e.target.value)}
          onKeyDown={handleInputKeyDown}
          placeholder={
            projectPath
              ? "Search files…  (Cmd/Ctrl+P)"
              : "No project for this session"
          }
          disabled={!projectPath}
          className="min-w-0 flex-1 bg-transparent text-[12px] outline-none placeholder:text-muted-foreground"
        />
        {filesLoading && (
          <span className="shrink-0 text-[10px] text-muted-foreground">
            indexing…
          </span>
        )}
        {!filesLoading && files.length > 0 && (
          <span className="shrink-0 tabular-nums text-[10px] text-muted-foreground">
            {filtered.length}
            {filtered.length === PICKER_RESULT_LIMIT && "+"} / {files.length}
          </span>
        )}
      </div>

      {query && (
        <div className="max-h-64 shrink-0 overflow-auto border-b border-border bg-background/95">
          {filtered.length === 0 ? (
            <div className="px-3 py-3 text-center text-[11px] text-muted-foreground">
              No files match.
            </div>
          ) : (
            filtered.map((path, i) => {
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
                  // mousedown rather than click so the input doesn't
                  // lose focus before the click registers (which
                  // would close the dropdown via blur)
                  onMouseDown={(e) => {
                    e.preventDefault();
                    setSelectedPath(path);
                    setQuery("");
                    inputRef.current?.blur();
                  }}
                  onMouseEnter={() => setHighlightedIndex(i)}
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
            })
          )}
        </div>
      )}

      <div className="min-h-0 flex-1 overflow-hidden">
        <CodeViewBody
          path={selectedPath}
          contents={fileContents}
          loading={fileLoading}
          error={fileError}
          filesError={filesError}
          hasProject={projectPath !== null}
        />
      </div>
    </div>
  );
}

interface CodeViewBodyProps {
  path: string | null;
  contents: string | null;
  loading: boolean;
  error: string | null;
  filesError: string | null;
  hasProject: boolean;
}

// Memoized so the picker dropdown's hover/highlight state changes
// don't re-tokenise the rendered file. The viewer only re-renders
// when path/contents actually change.
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
        Start typing to search files. Press Cmd/Ctrl+P from anywhere to
        focus the picker.
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
    // The Virtualizer attaches its scroll listener to the outer div
    // it renders (i.e. whatever element receives our `className` /
    // `style` props). For that listener to actually fire, the
    // element needs to be a scroll container — bounded height +
    // overflow:auto. `h-full` gives the bound, `overflow-auto`
    // makes it the scroll viewport.
    //
    // (`File`'s `overflow: "scroll"` option below is unrelated —
    // it only controls horizontal long-line behavior inside the
    // rendered file, not the vertical viewport.)
    <Virtualizer className="h-full overflow-auto">
      <PierreFile
        key={path}
        file={{ name: path, contents }}
        options={{
          theme: { dark: "pierre-dark", light: "pierre-light" },
          themeType: "system",
          overflow: "scroll",
          // Skip Shiki tokenisation on absurdly long lines so a
          // minified bundle or lockfile row can't lock up even
          // the worker. (No `maxLineDiffLength` here — that's
          // diff-only, the single-file viewer doesn't run a diff.)
          tokenizeMaxLineLength: 5_000,
        }}
      />
    </Virtualizer>
  );
});
