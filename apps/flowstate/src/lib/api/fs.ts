import { invoke } from "@tauri-apps/api/core";

// Code-view filesystem helpers. The `/code` editor view reads files
// and streams ripgrep matches via the Rust side so the frontend
// never runs either subprocess itself. All paths are resolved and
// sandboxed server-side — an escape attempt returns an error.

// Snapshot of the per-worktree file index returned by
// `list_project_files`. Mirrors the Rust `ProjectFileListing` struct
// (camelCase comes from serde's `rename_all = "camelCase"`).
//
// `files` is the full forward-slash relative-path list as currently
// indexed by fff-search — there is **no** server-side cap. On a
// 100k-file repo the list is just ~8 MB of JSON and the picker
// virtualises it client-side. While `indexing` is true the
// background scanner is still walking; React Query refetches on a
// short stale window so the picker fills in live.
export interface ProjectFileListing {
  files: string[];
  indexing: boolean;
  // Files indexed so far (== files.length). Surfaced separately so
  // the picker header can render "Indexing N files…" without
  // recomputing.
  scanned: number;
}

// Every file in `path` that isn't ignored by .gitignore / .ignore,
// returned as forward-slash relative paths. Used by the /code
// editor view's Cmd+P-style picker. **Not capped** — fff-search's
// mmap-backed file table makes the full list cheap. The previous
// 20k cap silently dropped most files on a 100k-file repo and made
// it impossible to find files by typing the exact name.
export function listProjectFiles(path: string): Promise<ProjectFileListing> {
  return invoke<ProjectFileListing>("list_project_files", { path });
}

// Drop the cached fff-search file picker for `path` so the next
// `listProjectFiles` call rebuilds it from a fresh scan. Wired up
// from the chat session's `turn_completed` event — agent edits that
// touch many files in quick succession can outrun fs-event
// coalescing on macOS, so we explicitly reindex at the moment the
// user is most likely to look at the picker again. No-op when
// `path` was never indexed.
export function reindexProjectFiles(path: string): Promise<void> {
  return invoke<void>("reindex_project_files", { path });
}

// Read a single project file as a UTF-8 string. Rejects on:
//   * file outside the project root (canonicalisation escape)
//   * file above CODE_VIEW_MAX_FILE_BYTES (4 MiB)
//   * non-UTF-8 content
// Callers should `.catch` to render a friendly placeholder.
export function readProjectFile(path: string, file: string): Promise<string> {
  return invoke<string>("read_project_file", { path, file });
}

/** Payload returned by `read_file_as_base64`. Mirrors the Rust
 *  `DroppedFilePayload` struct — the camelCase names come from
 *  serde's `rename_all = "camelCase"`. */
export interface DroppedFilePayload {
  name: string;
  mediaType: string;
  dataBase64: string;
  sizeBytes: number;
}

/** Read an arbitrary absolute path and return its bytes base64-
 *  encoded along with a best-effort MIME type. Backs the chat
 *  composer's drag-and-drop flow — when the user drops an image /
 *  audio / video file, the bytes are lifted through this call and
 *  attached as an `AttachedImage` chip. For non-media files
 *  (source code, pdfs, csvs, etc.) the chat composer skips this
 *  helper entirely and just inserts the path as an `@file` mention
 *  chip so the agent can read it via its `Read` tool.
 *
 *  Rejects on: non-regular-file paths, unreadable paths, or files
 *  above the Rust-side 50 MB cap. */
export function readFileAsBase64(path: string): Promise<DroppedFilePayload> {
  return invoke<DroppedFilePayload>("read_file_as_base64", { path });
}

export interface BlockLine {
  // 1-based line number, matching ripgrep / editor convention.
  line: number;
  // Line text, trimmed of trailing newline and clipped server-side
  // so a single huge minified line can't blow up the IPC payload.
  text: string;
  // True if this line was a match for the query; false if it's
  // surrounding-context only.
  isMatch: boolean;
}

export interface ContentBlock {
  path: string;
  // 1-based line of the first entry in `lines` — convenient for
  // the gutter even though every line carries its own number.
  startLine: number;
  // Match line(s) plus surrounding context, in source order.
  // Adjacent matches share a single block (ripgrep's
  // context_break is the boundary).
  lines: BlockLine[];
}

// Per-search options forwarded to the rust side's content-search
// command. Defaults map to the boring case-sensitive literal
// behavior with no path filtering — callers that don't care about
// the advanced options can pass `defaultContentSearchOptions()`.
export interface ContentSearchOptions {
  /** Treat the query as a regex (ripgrep dialect) instead of a
   *  literal string. Default false. Ignored when `useFuzzy` is
   *  true. */
  useRegex: boolean;
  /** Fuzzy match each line against the query using fff-search's
   *  Smith-Waterman scorer — typo-tolerant and inherently
   *  case-insensitive. Takes precedence over `useRegex`. Default
   *  false. */
  useFuzzy: boolean;
  /** Default true. The `aA` toggle in the UI flips this off. */
  caseSensitive: boolean;
  /** Glob patterns restricting which files the walker visits. */
  includes: string[];
  /** Glob patterns excluded from the walker. Plain globs — the
   *  Rust side handles the `!` prefix internally. */
  excludes: string[];
}

export function defaultContentSearchOptions(): ContentSearchOptions {
  return {
    useRegex: false,
    useFuzzy: false,
    caseSensitive: true,
    includes: [],
    excludes: [],
  };
}

// Monotonic token allocator for `searchFileContents` cancellation.
// Each call gets a fresh token; pass it to `stopContentSearch` to
// cooperatively interrupt the in-flight grep. We mint tokens client-
// side (rather than asking Rust for one) so the caller can tear down
// a stale search the moment a new one starts, without an extra
// round-trip.
let nextSearchToken = 1;
export function nextContentSearchToken(): number {
  return nextSearchToken++;
}

// Live content search across the project. Backed by fff-search's
// indexed grep (literal / regex / fuzzy modes via `options`); the
// `cancelToken` is registered with a Rust-side `AtomicBool` flag so
// `stopContentSearch(token)` can interrupt a slow query. Returns one
// ContentBlock per disjoint match group with ±3 lines of context —
// designed for a Zed-style multibuffer renderer.
export function searchFileContents(
  path: string,
  query: string,
  options: ContentSearchOptions,
  cancelToken?: number,
): Promise<ContentBlock[]> {
  return invoke<ContentBlock[]>("search_file_contents", {
    path,
    query,
    options,
    cancelToken: cancelToken ?? null,
  });
}

// Cancel the content search registered under `token`. Idempotent —
// unknown tokens are silently ignored on the Rust side.
export function stopContentSearch(token: number): Promise<void> {
  return invoke<void>("stop_content_search", { token });
}
