/* eslint-disable react-hooks/exhaustive-deps */
import * as React from "react";
import {
  CaseSensitive,
  FileText,
  Filter,
  Loader2,
  Regex,
  Sparkles,
  TextSearch,
} from "lucide-react";

import { Dialog, DialogContent } from "@/components/ui/dialog";
import { cn } from "@/lib/utils";
import {
  defaultContentSearchOptions,
  nextContentSearchToken,
  readProjectFile,
  searchFileContents,
  stopContentSearch,
  type BlockLine,
  type ContentBlock,
  type ContentSearchOptions,
} from "@/lib/api";
import {
  matchesPickerQuery,
  parsePickerQuery,
  splitGlobList,
} from "@/lib/glob";
import { rankFileMatches } from "@/lib/mention-utils";

// Port of zen-tools' split-view search palette. Single popup hosts
// both Files (⌘P) and Content (⌘⇧F) modes; Tab swaps between them
// without closing. Left pane = ranked result list; right pane =
// plain-text preview with the matched line highlighted in amber.
//
// Heavy lifting reuses the existing flowstate-finder APIs:
//   * Files: `rankFileMatches` (lib/mention-utils) on top of the
//     project's pre-walked file list — synchronous, no IPC.
//   * Content: `searchFileContents` (lib/api) → fff-search +
//     ripgrep on the Rust side, with `stopContentSearch` for
//     cancellation.
//   * Preview: `readProjectFile` with a FIFO 64-entry Map cache.

export type SearchMode = "files" | "content";

/** Hard cap on rendered rows. Matches the previous primitive picker's
 *  cap so behaviour is unchanged in the file ranker. */
const PICKER_RESULT_LIMIT = 200;
/** Content-mode debounce. 600 ms matches the old top-input behaviour
 *  and the zen-tools palette. */
const CONTENT_SEARCH_DEBOUNCE_MS = 600;
/** Lines shown in preview for Files-mode hits. */
const FILES_PREVIEW_LINES = 60;
/** Lines of context shown above/below the match line in Content-mode
 *  preview. The ±18 window is wide enough to read the surrounding
 *  function header but narrow enough that the preview pane stays
 *  scannable. */
const CONTENT_PREVIEW_PADDING = 18;
/** Bounded preview-text cache. FIFO eviction relies on Map preserving
 *  insertion order — `keys().next().value` is the oldest entry. */
const PREVIEW_CACHE_LIMIT = 64;
/** localStorage key for the file-mode fuzzy toggle. Kept in sync with
 *  the legacy code-view.tsx key so users keep their preference across
 *  the port. */
const FUZZY_FILES_STORAGE_KEY = "flowstate:fuzzy-files";

/** Per-row payload — one entry per file (Files mode) or one entry
 *  per match line (Content mode). The right pane reads the full file
 *  lazily, so each row only needs enough to identify the hit. */
type FlatResult =
  | { kind: "file"; path: string; key: string }
  | {
      kind: "content";
      path: string;
      line: number;
      text: string;
      key: string;
    };

export interface SearchPaletteProps {
  /** Controlled open flag. Parent flips this in response to ⌘P / ⌘⇧F. */
  open: boolean;
  onOpenChange: (open: boolean) => void;
  /** Controlled mode. Parent passes the requested mode whenever a
   *  shortcut fires; the palette also dispatches `onModeChange` when
   *  the user clicks a tab or presses Tab. */
  mode: SearchMode;
  onModeChange: (mode: SearchMode) => void;
  /** Project root. Used as the first arg to `searchFileContents`
   *  and `readProjectFile`. Required — the palette renders an empty
   *  state when null. */
  projectPath: string | null;
  /** Project-relative file paths from `useQuery(projectFilesQueryOptions)`.
   *  Filtered + ranked client-side in Files mode. */
  files: readonly string[];
  /** True while fff-search is still walking the worktree. Shown as a
   *  hint in the result header so the user understands an empty list
   *  may just mean "indexing isn't done". */
  indexing: boolean;
  /** Open a file in the editor. Wired to `useEditorTabs().openFile`
   *  in CodeView. The optional `lineNumber` is forwarded so callers
   *  that DO want goto-line plumbing (none today) can opt in later
   *  without refactoring this component. */
  onPickFile: (path: string, lineNumber?: number) => void;
}

export function SearchPalette({
  open,
  onOpenChange,
  mode,
  onModeChange,
  projectPath,
  files,
  indexing,
  onPickFile,
}: SearchPaletteProps) {
  // ── local query / option state ─────────────────────────────────
  const [query, setQuery] = React.useState("");
  // File-mode fuzzy uses `rankFileMatches`' "fuzzy" mode, which
  // engages `lib/fuzzy.ts`'s IntelliJ / Zed-style acronym pre-pass:
  //   "acph"  matches "agent-context-panel-host"
  //   "ACPH"  matches "AgentContextPanelHost"
  //   "aCPH"  matches "agentContextPanelHost"
  //   "acph"  matches "agent_context_panel_host"
  // Default ON because defaulting it OFF makes acronyms silently
  // inert — the substring pre-filter drops acronym-only candidates
  // before the fuzzy ranker ever sees them. Substring + glob hits
  // still rank highest in fuzzy mode, so power users typing exact
  // filenames lose nothing. Persisted to localStorage under the
  // same key the legacy inline picker used, so the port preserves
  // the user's preference.
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
  // Content-mode toggles. `useFuzzyContent` disables regex/case
  // because fff-search's Smith-Waterman scorer is inherently
  // case-insensitive and doesn't use regex.
  const [useRegex, setUseRegex] = React.useState(false);
  const [caseSensitive, setCaseSensitive] = React.useState(true);
  const [useFuzzyContent, setUseFuzzyContent] = React.useState(false);
  const [includes, setIncludes] = React.useState("");
  const [excludes, setExcludes] = React.useState("");
  const [showAdvanced, setShowAdvanced] = React.useState(false);

  // ── async result state ────────────────────────────────────────
  const [fileResults, setFileResults] = React.useState<string[]>([]);
  const [contentBlocks, setContentBlocks] = React.useState<ContentBlock[]>([]);
  const [searching, setSearching] = React.useState(false);
  const [errorMsg, setErrorMsg] = React.useState<string | null>(null);
  const [highlightIdx, setHighlightIdx] = React.useState(0);

  // ── refs ──────────────────────────────────────────────────────
  // Content-mode cancellation token (passed to the Rust side).
  const tokenRef = React.useRef<number>(0);
  // File-mode "newest wins" counter. Not strictly needed because
  // file ranking is synchronous today, but kept here so a future
  // async file search would land in an existing race-safe shape.
  const fileSearchAbortRef = React.useRef<number>(0);
  const inputRef = React.useRef<HTMLInputElement | null>(null);
  const listRef = React.useRef<HTMLDivElement | null>(null);

  // ── reset on open ──────────────────────────────────────────────
  // Wipes per-session state so a stale query / blocks from the last
  // open don't flash in. Also focuses the input inside an rAF so
  // Radix's own focus management has settled by the time we steal
  // focus — see the `onOpenAutoFocus={prevent}` prop on the Dialog
  // content below for the matching half of this dance.
  React.useEffect(() => {
    if (!open) return;
    setQuery("");
    setHighlightIdx(0);
    setContentBlocks([]);
    setFileResults([]);
    setErrorMsg(null);
    requestAnimationFrame(() => inputRef.current?.focus());
  }, [open, mode]);

  // Cancel any in-flight content search when the palette closes
  // so a slow ripgrep walk doesn't keep burning CPU after the user
  // dismisses.
  React.useEffect(() => {
    if (open) return;
    if (tokenRef.current !== 0) {
      void stopContentSearch(tokenRef.current).catch(() => {});
      tokenRef.current = 0;
    }
  }, [open]);

  // ── file-mode ranking (synchronous) ───────────────────────────
  // Empty query → take the head of the project's file list (in the
  // indexer's order); non-empty → run the same three-branch query
  // router the inline picker had, then rank with `rankFileMatches`.
  // Cap at PICKER_RESULT_LIMIT so the DOM stays small without
  // virtualization. Recents are intentionally skipped (user choice).
  //
  // Query syntax (all features the inline picker supported):
  //   "tabs"               substring/fuzzy match anywhere in the path
  //   "acph"               acronym match → agent-context-panel-host
  //                        (fuzzy mode only — see useFuzzyFiles)
  //   "src tabs.ts"        scoped: basename "tabs.ts" inside paths
  //                        whose directory contains "src"
  //   "lib/api git.ts"     scoped to a deeper prefix
  //   "*.tsx"              glob: every .tsx
  //   "**/code *.tsx"      glob + scope: .tsx inside any "code" dir
  //   "lib/api, src/code"  comma alternatives — match either prefix
  React.useEffect(() => {
    if (!open || mode !== "files") return;
    const trimmed = query.trim();
    if (!trimmed) {
      setFileResults((files as string[]).slice(0, PICKER_RESULT_LIMIT));
      return;
    }
    const myToken = ++fileSearchAbortRef.current;

    // Classify the query so we can route around the substring
    // pre-filter when fuzzy is on. The pre-filter uses
    // `matchesPickerQuery` (substring `.includes()` under the hood),
    // which silently drops anything not literally containing the
    // query — fatal for fuzzy (typing `tbsv` returns zero before the
    // fuzzy ranker ever sees the list). The same fatal-skip would
    // bite acronyms like `acph` → `agent-context-panel-host`, which
    // is exactly the path users care about.
    const hasComma = trimmed.includes(",");
    const hasGlob = /[*?]/.test(trimmed);
    const spaceIdx = trimmed.indexOf(" ");
    const isScopedQuery = !hasComma && !hasGlob && spaceIdx > 0;
    const isPlainQuery = !hasComma && !hasGlob && spaceIdx < 0;

    // Three branches when fuzzy is on:
    //   1. Plain query (`tbsv`, `acph`)   → no pre-filter; fuzzy
    //                                       ranks the full list.
    //   2. Scoped query (`src tbsv`)      → substring-filter by the
    //                                       FOLDER portion only;
    //                                       fuzzy ranks survivors on
    //                                       basename. The user opted
    //                                       into folder scoping, so
    //                                       we honor it — but the
    //                                       basename substring check
    //                                       has to be skipped or
    //                                       fuzzy gets an empty list
    //                                       again.
    //   3. Glob / comma query             → keep the existing
    //                                       substring pre-filter
    //                                       (explicit user intent).
    //                                       Fuzzy then orders.
    //
    // Substring mode (default off) always falls through to branch 3
    // — no behaviour change versus the legacy inline picker.
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
          : (files as string[]).filter((f) => matchesPickerQuery(f, parsed));
    }

    // Strip the optional folder/glob qualifier off for the ranker:
    // it scores on the full path, so passing the original query is
    // fine for plain substring/fuzzy queries; for scoped queries
    // (`src tabs.ts`) we hand the file part to the ranker so the
    // basename-priority kicks in.
    const rankerQuery = trimmed.includes(" ")
      ? trimmed.split(" ").pop()!
      : trimmed;
    const ranked = rankFileMatches(
      survivors,
      rankerQuery,
      PICKER_RESULT_LIMIT,
      useFuzzyFiles ? "fuzzy" : "substring",
    );
    if (myToken !== fileSearchAbortRef.current) return;
    setFileResults(ranked);
  }, [open, mode, query, files, useFuzzyFiles]);

  // ── content-mode debounced search ─────────────────────────────
  React.useEffect(() => {
    if (!open || mode !== "content") return;
    const trimmed = query.trim();
    if (!projectPath || !trimmed) {
      setContentBlocks([]);
      setErrorMsg(null);
      setSearching(false);
      return;
    }
    // Cancel any in-flight search before scheduling a new one. The
    // Rust side flips an AtomicBool inside the running grep; the
    // search bails on its next cooperative check instead of running
    // its 30 s budget to completion.
    if (tokenRef.current !== 0) {
      void stopContentSearch(tokenRef.current).catch(() => {});
    }
    const timer = window.setTimeout(() => {
      const myToken = nextContentSearchToken();
      tokenRef.current = myToken;
      setSearching(true);
      setErrorMsg(null);
      const apiOptions: ContentSearchOptions = {
        ...defaultContentSearchOptions(),
        useRegex: useFuzzyContent ? false : useRegex,
        caseSensitive: useFuzzyContent ? false : caseSensitive,
        useFuzzy: useFuzzyContent,
        includes: splitGlobList(includes),
        excludes: splitGlobList(excludes),
      };
      searchFileContents(projectPath, trimmed, apiOptions, myToken)
        .then((blocks) => {
          // Late return — a newer search has started; drop result.
          if (tokenRef.current !== myToken) return;
          setContentBlocks(blocks);
        })
        .catch((err) => {
          if (tokenRef.current !== myToken) return;
          setErrorMsg(String((err as { message?: string })?.message ?? err));
          setContentBlocks([]);
        })
        .finally(() => {
          // Only clear the spinner if we're still the active search.
          // (Fixes the `|| true` bug zen-tools called out: if a newer
          // search has started, leave the spinner up for it.)
          if (tokenRef.current === myToken) setSearching(false);
        });
    }, CONTENT_SEARCH_DEBOUNCE_MS);
    return () => window.clearTimeout(timer);
  }, [
    open,
    mode,
    query,
    projectPath,
    useRegex,
    caseSensitive,
    useFuzzyContent,
    includes,
    excludes,
  ]);

  // ── flatten results into row entries ──────────────────────────
  const flat: FlatResult[] = React.useMemo(() => {
    if (mode === "files") {
      return fileResults.map((p) => ({ kind: "file" as const, path: p, key: p }));
    }
    const out: FlatResult[] = [];
    for (const block of contentBlocks) {
      // Emit one row per match line — surrounding context lives in
      // the preview pane (lazy-read from disk).
      for (const ln of block.lines) {
        if (!ln.isMatch) continue;
        out.push({
          kind: "content",
          path: block.path,
          line: ln.line,
          text: ln.text,
          key: `${block.path}:${ln.line}`,
        });
      }
    }
    return out;
  }, [mode, fileResults, contentBlocks]);

  // Reset highlight whenever the flat array's reference changes
  // (mode flip, new query, new options). Reference-equality is
  // safe because `flat` is memo'd on the right deps.
  React.useEffect(() => {
    setHighlightIdx(0);
  }, [flat]);

  // Scroll the highlighted row into view on keyboard nav.
  React.useEffect(() => {
    const el = listRef.current?.querySelector<HTMLElement>(
      `[data-row-index="${highlightIdx}"]`,
    );
    el?.scrollIntoView({ block: "nearest" });
  }, [highlightIdx]);

  const selected = flat[highlightIdx] ?? null;

  // ── keyboard handling on the input ────────────────────────────
  function onInputKeyDown(e: React.KeyboardEvent<HTMLInputElement>) {
    if (e.key === "ArrowDown") {
      e.preventDefault();
      setHighlightIdx((i) => Math.min(flat.length - 1, i + 1));
    } else if (e.key === "ArrowUp") {
      e.preventDefault();
      setHighlightIdx((i) => Math.max(0, i - 1));
    } else if (e.key === "Enter") {
      e.preventDefault();
      const pick = flat[highlightIdx] ?? flat[0];
      if (!pick) return;
      pickRow(pick);
    } else if (e.key === "Tab") {
      e.preventDefault();
      onModeChange(mode === "files" ? "content" : "files");
    }
    // Escape is intentionally NOT handled here — Radix Dialog's
    // capture-phase handler picks it up and routes through
    // `onOpenChange(false)` regardless of input focus.
  }

  function pickRow(pick: FlatResult) {
    if (pick.kind === "file") {
      onPickFile(pick.path);
    } else {
      onPickFile(pick.path, pick.line);
    }
    onOpenChange(false);
  }

  // Disabled state: no project means the picker has nothing to act
  // on. We render the dialog so the shortcut feels responsive but
  // surface a friendly placeholder rather than erroring on the empty
  // `files` list.
  const noProject = projectPath === null;

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent
        // 64rem matches zen-tools — wide enough to host the split
        // without crowding the preview. Inline `maxWidth` overrides
        // the wrapper's `sm:max-w-sm` Tailwind responsive cap, which
        // would otherwise win at the sm+ breakpoint.
        style={{ maxWidth: "64rem" }}
        className="w-full gap-0 overflow-hidden p-0"
        showCloseButton={false}
        onOpenAutoFocus={(e) => {
          // Yield focus management to our own rAF-driven focus(); see
          // the reset-on-open effect.
          e.preventDefault();
        }}
      >
        {/* Mode tabs */}
        <div className="flex items-center gap-1 border-b border-border px-2 py-1.5">
          <ModeTab
            label="Files"
            icon={FileText}
            shortcut="⌘P"
            active={mode === "files"}
            onClick={() => onModeChange("files")}
          />
          <ModeTab
            label="Content"
            icon={TextSearch}
            shortcut="⌘⇧F"
            active={mode === "content"}
            onClick={() => onModeChange("content")}
          />
          <span className="ml-auto text-[10px] text-muted-foreground/60">
            <kbd className="rounded border border-border bg-muted px-1 py-0.5 font-mono">
              Tab
            </kbd>{" "}
            to switch ·{" "}
            <kbd className="rounded border border-border bg-muted px-1 py-0.5 font-mono">
              Esc
            </kbd>{" "}
            to close
          </span>
        </div>

        {/* Query input + per-mode option toggles */}
        <div className="flex items-center gap-1 border-b border-border px-2 py-1">
          <input
            ref={inputRef}
            type="text"
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            onKeyDown={onInputKeyDown}
            disabled={noProject}
            placeholder={
              noProject
                ? "No project for this session"
                : mode === "files"
                  ? // Hint at the full query DSL: plain / acronym
                    // (with Fuzzy on) / scoped / glob / comma.
                    "Type to filter files…  e.g. tabs.ts  ·  acph  ·  src tabs.ts  ·  **/code *.tsx"
                  : "Type to grep across the project…"
            }
            className="flex-1 bg-transparent px-1 py-1 text-sm outline-none placeholder:text-muted-foreground"
          />
          {mode === "files" ? (
            <OptionToggle
              icon={Sparkles}
              label="Fuzzy match (acronym + subsequence)"
              active={useFuzzyFiles}
              onToggle={() => setUseFuzzyFiles((v) => !v)}
            />
          ) : (
            <>
              <OptionToggle
                icon={Sparkles}
                label="Fuzzy (all-words)"
                active={useFuzzyContent}
                onToggle={() => setUseFuzzyContent((v) => !v)}
              />
              <OptionToggle
                icon={Regex}
                label="Regex"
                active={useRegex}
                onToggle={() => setUseRegex((v) => !v)}
                disabled={useFuzzyContent}
              />
              <OptionToggle
                icon={CaseSensitive}
                label="Case-sensitive"
                active={caseSensitive}
                onToggle={() => setCaseSensitive((v) => !v)}
                disabled={useFuzzyContent}
              />
              <OptionToggle
                icon={Filter}
                label="Filters (include / exclude globs)"
                active={showAdvanced}
                onToggle={() => setShowAdvanced((v) => !v)}
              />
            </>
          )}
        </div>

        {/* Advanced globs row (content mode only) */}
        {mode === "content" && showAdvanced ? (
          <div className="flex flex-col gap-1 border-b border-border px-2 py-1.5 text-xs">
            <GlobInput
              label="Include"
              placeholder="src/**, **/*.ts (comma-separated)"
              value={includes}
              onChange={setIncludes}
            />
            <GlobInput
              label="Exclude"
              placeholder="dist/**, **/*.test.ts"
              value={excludes}
              onChange={setExcludes}
            />
          </div>
        ) : null}

        {/* Status row */}
        <div className="flex shrink-0 items-center gap-2 border-b border-border bg-muted/20 px-3 py-1 text-[10px] text-muted-foreground/80">
          <span>
            {mode === "files"
              ? `${flat.length} file${flat.length === 1 ? "" : "s"}`
              : searching
                ? "Searching…"
                : errorMsg !== null
                  ? `Error: ${errorMsg}`
                  : `${flat.length} match${flat.length === 1 ? "" : "es"}`}
          </span>
          {mode === "files" && indexing ? (
            <span className="text-muted-foreground/60">
              · indexing {files.length} files…
            </span>
          ) : null}
          {mode === "content" && searching ? (
            <Loader2 className="size-3 animate-spin" />
          ) : null}
        </div>

        {/* Split: results list (left) + preview pane (right). The
            `min-w-0` is load-bearing — without it long URLs in result
            rows can blow the dialog past the 64rem max-width. */}
        <div className="flex h-[min(60vh,520px)] min-h-[320px] min-w-0 overflow-hidden">
          <div
            ref={listRef}
            className="w-2/5 max-w-[420px] shrink-0 overflow-y-auto border-r border-border p-1"
          >
            {flat.length === 0 ? (
              <div className="px-3 py-6 text-center text-[11px] text-muted-foreground/60">
                {noProject
                  ? "Open a thread to search its project files."
                  : !query.trim() && mode === "content"
                    ? "Type to search file contents."
                    : searching
                      ? "Searching…"
                      : mode === "files"
                        ? "No matching files"
                        : "No matches"}
              </div>
            ) : (
              flat.map((item, idx) => (
                <ResultRow
                  key={item.key}
                  item={item}
                  idx={idx}
                  active={idx === highlightIdx}
                  projectPath={projectPath}
                  onHover={() => setHighlightIdx(idx)}
                  onClick={() => pickRow(item)}
                />
              ))
            )}
          </div>
          <PreviewPane selected={selected} projectPath={projectPath} />
        </div>
      </DialogContent>
    </Dialog>
  );
}

// ─────────────────────────────────────────────────────────────────
// Mode tabs
// ─────────────────────────────────────────────────────────────────

interface ModeTabProps {
  label: string;
  icon: React.ComponentType<{ className?: string }>;
  shortcut: string;
  active: boolean;
  onClick: () => void;
}

function ModeTab({ label, icon: Icon, shortcut, active, onClick }: ModeTabProps) {
  return (
    <button
      type="button"
      onClick={onClick}
      className={cn(
        "inline-flex items-center gap-1.5 rounded px-2 py-1 text-xs transition-colors",
        active
          ? "bg-primary/15 font-semibold text-primary"
          : "text-muted-foreground hover:bg-muted/50 hover:text-foreground",
      )}
    >
      <Icon className="size-3.5" />
      {label}
      <span className="text-[10px] text-muted-foreground/60">{shortcut}</span>
    </button>
  );
}

// ─────────────────────────────────────────────────────────────────
// Option toggle (icon button)
// ─────────────────────────────────────────────────────────────────

interface OptionToggleProps {
  icon: React.ComponentType<{ className?: string }>;
  label: string;
  active: boolean;
  onToggle: () => void;
  disabled?: boolean;
}

function OptionToggle({
  icon: Icon,
  label,
  active,
  onToggle,
  disabled,
}: OptionToggleProps) {
  return (
    <button
      type="button"
      onClick={onToggle}
      title={
        disabled ? `${label}: disabled in fuzzy mode` : `${label}: ${active ? "on" : "off"}`
      }
      aria-pressed={active}
      disabled={disabled}
      className={cn(
        "inline-flex size-7 items-center justify-center rounded transition-colors",
        active && !disabled
          ? "bg-primary/15 text-primary"
          : "text-muted-foreground hover:bg-muted/50 hover:text-foreground",
        disabled && "cursor-not-allowed opacity-40 hover:bg-transparent",
      )}
    >
      <Icon className="size-3.5" />
    </button>
  );
}

// ─────────────────────────────────────────────────────────────────
// Glob input (Include / Exclude)
// ─────────────────────────────────────────────────────────────────

interface GlobInputProps {
  label: string;
  placeholder: string;
  value: string;
  onChange: (value: string) => void;
}

function GlobInput({ label, placeholder, value, onChange }: GlobInputProps) {
  return (
    <label className="flex items-center gap-2">
      <span className="w-14 text-[10px] uppercase tracking-wider text-muted-foreground/70">
        {label}
      </span>
      <input
        type="text"
        value={value}
        placeholder={placeholder}
        onChange={(e) => onChange(e.target.value)}
        className="flex-1 rounded border border-border/60 bg-background px-1.5 py-0.5 font-mono text-[11px] outline-none focus:border-primary"
      />
    </label>
  );
}

// ─────────────────────────────────────────────────────────────────
// Result rows
// ─────────────────────────────────────────────────────────────────

interface ResultRowProps {
  item: FlatResult;
  idx: number;
  active: boolean;
  projectPath: string | null;
  onHover: () => void;
  onClick: () => void;
}

function ResultRow({
  item,
  idx,
  active,
  onHover,
  onClick,
}: ResultRowProps) {
  const onKeyDown = (e: React.KeyboardEvent) => {
    if (e.key === "Enter" || e.key === " ") {
      e.preventDefault();
      onClick();
    }
  };

  if (item.kind === "file") {
    return (
      <div
        role="button"
        tabIndex={0}
        data-row-index={idx}
        onMouseMove={onHover}
        onClick={onClick}
        onKeyDown={onKeyDown}
        title={item.path}
        className={cn(
          "flex w-full min-w-0 items-start gap-2 overflow-hidden rounded px-2 py-1.5 text-left text-xs",
          active && "bg-accent text-accent-foreground",
        )}
      >
        <FileText className="mt-[2px] size-3.5 shrink-0 text-primary/70" />
        <div className="flex min-w-0 flex-1 flex-col gap-0">
          <div className="block w-full min-w-0 truncate font-medium">
            {basenameNoExt(item.path)}
          </div>
          <FrontTruncated text={item.path} />
        </div>
      </div>
    );
  }

  return (
    <div
      role="button"
      tabIndex={0}
      data-row-index={idx}
      onMouseMove={onHover}
      onClick={onClick}
      onKeyDown={onKeyDown}
      title={item.path}
      className={cn(
        "flex w-full min-w-0 items-start gap-2 overflow-hidden rounded px-2 py-1.5 text-left text-xs",
        active && "bg-accent text-accent-foreground",
      )}
    >
      <TextSearch className="mt-[2px] size-3.5 shrink-0 text-fuchsia-500/70" />
      <div className="flex min-w-0 flex-1 flex-col gap-0">
        <div className="flex w-full min-w-0 items-baseline gap-2 overflow-hidden">
          <span className="shrink-0 font-mono text-[11px]">
            {basenameNoExt(item.path)}
          </span>
          <span className="shrink-0 text-[10px] text-muted-foreground/60">
            :{item.line}
          </span>
          <span className="block min-w-0 flex-1 truncate font-mono text-[11px] text-muted-foreground">
            {item.text.trim()}
          </span>
        </div>
        <FrontTruncated text={item.path} />
      </div>
    </div>
  );
}

/** Truncate from the LEFT so the basename stays visible. CSS-only —
 *  `direction: rtl` plus `<bdi>` to preserve the path's natural
 *  reading order while letting `text-overflow: ellipsis` chop the
 *  prefix. No JS measurement, works in every modern browser. */
function FrontTruncated({ text }: { text: string }) {
  return (
    <span
      className="block min-w-0 overflow-hidden whitespace-nowrap text-[10px] text-muted-foreground/55"
      style={{ direction: "rtl", textOverflow: "ellipsis" }}
    >
      <bdi>{text}</bdi>
    </span>
  );
}

function basenameNoExt(path: string): string {
  const slash = path.lastIndexOf("/");
  const base = slash >= 0 ? path.slice(slash + 1) : path;
  const dot = base.lastIndexOf(".");
  if (dot <= 0) return base;
  return base.slice(0, dot);
}

// ─────────────────────────────────────────────────────────────────
// Preview pane
// ─────────────────────────────────────────────────────────────────

interface PreviewPaneProps {
  selected: FlatResult | null;
  projectPath: string | null;
}

function PreviewPane({ selected, projectPath }: PreviewPaneProps) {
  // ── ALL hooks unconditionally at the top ────────────────────
  // React enforces stable hook order — early-returning between
  // `useState` / `useEffect` calls would crash with "Rendered fewer
  // hooks than expected". Hoist + null-guard inside.
  const cacheRef = React.useRef<Map<string, string>>(new Map());
  const viewportRef = React.useRef<HTMLDivElement | null>(null);
  const [content, setContent] = React.useState<string | null>(null);
  const [loadingPath, setLoadingPath] = React.useState<string | null>(null);

  const selectedPath = selected?.path ?? null;
  const selectedLine = selected?.kind === "content" ? selected.line : null;

  // Lazy file read with FIFO cache.
  React.useEffect(() => {
    if (!selectedPath || !projectPath) {
      setContent(null);
      setLoadingPath(null);
      return;
    }
    const cached = cacheRef.current.get(selectedPath);
    if (cached !== undefined) {
      setContent(cached);
      setLoadingPath(null);
      return;
    }
    let cancelled = false;
    setContent(null);
    setLoadingPath(selectedPath);
    void readProjectFile(projectPath, selectedPath)
      .then((text) => {
        if (cancelled) return;
        cacheRef.current.set(selectedPath, text);
        if (cacheRef.current.size > PREVIEW_CACHE_LIMIT) {
          // Map keys() returns iteration order = insertion order.
          // The first entry is the oldest — evict it.
          const first = cacheRef.current.keys().next().value as
            | string
            | undefined;
          if (first) cacheRef.current.delete(first);
        }
        setContent(text);
        setLoadingPath(null);
      })
      .catch((err) => {
        if (cancelled) return;
        console.warn("[search-palette] preview read failed", selectedPath, err);
        setContent(null);
        setLoadingPath(null);
      });
    return () => {
      cancelled = true;
    };
  }, [projectPath, selectedPath]);

  // Centre the matched line whenever it changes (content mode only).
  React.useEffect(() => {
    if (selectedLine == null) return;
    const el = viewportRef.current?.querySelector<HTMLElement>(
      `[data-preview-line="${selectedLine}"]`,
    );
    el?.scrollIntoView({ block: "center", behavior: "auto" });
  }, [selectedPath, selectedLine, content]);

  if (!selected) {
    return (
      <div className="flex flex-1 items-center justify-center p-6 text-xs text-muted-foreground/60">
        Pick a result to preview.
      </div>
    );
  }

  const allLines = (content ?? "").split("\n");
  const targetLine = selected.kind === "content" ? selected.line : 1;
  const windowStart =
    selected.kind === "content"
      ? Math.max(1, targetLine - CONTENT_PREVIEW_PADDING)
      : 1;
  const windowEnd =
    selected.kind === "content"
      ? Math.min(allLines.length, targetLine + CONTENT_PREVIEW_PADDING)
      : Math.min(allLines.length, FILES_PREVIEW_LINES);
  const windowLines = allLines.slice(windowStart - 1, windowEnd);

  return (
    <div className="flex min-w-0 flex-1 flex-col">
      <div className="flex shrink-0 items-center gap-2 border-b border-border bg-muted/30 px-3 py-1 text-[11px] text-muted-foreground/80">
        <span className="truncate font-mono">{selected.path}</span>
        {selected.kind === "content" ? (
          <span className="ml-auto shrink-0 tabular-nums text-muted-foreground/60">
            line {selected.line}
          </span>
        ) : null}
      </div>
      <div
        ref={viewportRef}
        className="min-h-0 flex-1 overflow-y-auto bg-background"
      >
        {loadingPath === selected.path ? (
          <div className="px-3 py-2 text-xs text-muted-foreground">
            <Loader2 className="mr-1 inline size-3 animate-spin" />
            Loading preview…
          </div>
        ) : content == null ? (
          <div className="px-3 py-2 text-xs text-muted-foreground/60">
            Couldn't read file.
          </div>
        ) : (
          <pre className="px-2 py-1.5 font-mono text-[11px] leading-relaxed">
            {windowLines.map((text, i) => {
              const lineNo = windowStart + i;
              const isMatch =
                selected.kind === "content" && lineNo === targetLine;
              return (
                <div
                  key={lineNo}
                  data-preview-line={lineNo}
                  className={cn(
                    "flex gap-2 whitespace-pre",
                    isMatch
                      ? "bg-amber-500/20 font-semibold text-foreground"
                      : "text-muted-foreground/85",
                  )}
                >
                  <span className="w-10 shrink-0 select-none text-right tabular-nums text-muted-foreground/40">
                    {lineNo}
                  </span>
                  {/* Non-empty placeholder — an empty line as a child
                      collapses the div to 0 height and breaks the
                      gutter's vertical rhythm. */}
                  <span className="break-words">{text || " "}</span>
                </div>
              );
            })}
            {selected.kind === "file" && allLines.length > windowEnd ? (
              <div className="mt-1 px-2 text-[10px] italic text-muted-foreground/50">
                … {allLines.length - windowEnd} more line
                {allLines.length - windowEnd === 1 ? "" : "s"}
              </div>
            ) : null}
          </pre>
        )}
      </div>
    </div>
  );
}

// Re-export the row-payload type so callers can still satisfy
// TypeScript when they want to invoke `pickRow` shape externally.
// Currently no external caller does, but keeping the alias here
// avoids surprise if a future host component (e.g. a peek-only
// preview) wants to render a `<ResultRow />` outside the palette.
export type { FlatResult, ContentBlock, BlockLine };
