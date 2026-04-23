import { invoke } from "@tauri-apps/api/core";

// Code-view filesystem helpers. The `/code` editor view reads files
// and streams ripgrep matches via the Rust side so the frontend
// never runs either subprocess itself. All paths are resolved and
// sandboxed server-side — an escape attempt returns an error.

// Every file in `path` that isn't ignored by .gitignore / .ignore,
// returned as forward-slash relative paths. Used by the /code
// editor view's Cmd+P-style picker. Capped at 20k entries on the
// Rust side so huge monorepos don't blow up the IPC bridge.
export function listProjectFiles(path: string): Promise<string[]> {
  return invoke<string[]>("list_project_files", { path });
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
  /** Treat the query as a `regex` crate regex instead of a
   *  literal string. Default false. */
  useRegex: boolean;
  /** Default true. The `aA` toggle in the UI flips this off. */
  caseSensitive: boolean;
  /** Glob patterns restricting which files the walker visits. */
  includes: string[];
  /** Glob patterns excluded from the walker (rust prefixes them
   *  with `!` for OverrideBuilder so the user types plain globs). */
  excludes: string[];
}

export function defaultContentSearchOptions(): ContentSearchOptions {
  return {
    useRegex: false,
    caseSensitive: true,
    includes: [],
    excludes: [],
  };
}

// Live content search across the project, ripgrep-style. The
// `options` arg controls regex vs literal matching, case
// sensitivity, and include/exclude glob filters (all defaulted
// to the conservative "search everything literally, case-
// sensitive" behavior). Returns one ContentBlock per disjoint
// match group with ±3 lines of surrounding context — designed
// for a Zed-style multibuffer renderer. Total lines streamed
// are capped server-side so pathological queries can't flood
// the bridge.
export function searchFileContents(
  path: string,
  query: string,
  options: ContentSearchOptions,
): Promise<ContentBlock[]> {
  return invoke<ContentBlock[]>("search_file_contents", {
    path,
    query,
    options,
  });
}
