import { Channel, invoke } from "@tauri-apps/api/core";
import type { AttachmentData, ClientMessage, ServerMessage } from "./types";

export function sendMessage(
  message: ClientMessage,
): Promise<ServerMessage | null> {
  return invoke<ServerMessage | null>("handle_message", { message });
}

/** Lazy fetch of a persisted image attachment. Called when the user
 * clicks a chip on a replayed turn — never on session load. */
export async function getAttachment(
  attachmentId: string,
): Promise<AttachmentData> {
  const resp = await sendMessage({
    type: "get_attachment",
    attachment_id: attachmentId,
  });
  if (resp?.type === "attachment") return resp.data;
  if (resp?.type === "error") throw new Error(resp.message);
  throw new Error("unexpected response to get_attachment");
}

export function connectStream(
  onMessage: (message: ServerMessage) => void,
): Promise<void> {
  const channel = new Channel<ServerMessage>();
  channel.onmessage = onMessage;
  return invoke("connect", { onEvent: channel });
}

export function getGitBranch(path: string): Promise<string | null> {
  return invoke<string | null>("get_git_branch", { path });
}

export interface GitBranchList {
  current: string | null;
  local: string[];
  remote: string[];
}

export function listGitBranches(path: string): Promise<GitBranchList> {
  return invoke<GitBranchList>("list_git_branches", { path });
}

export function gitCheckout(
  path: string,
  branch: string,
  createTrack: string | null,
): Promise<void> {
  return invoke<void>("git_checkout", { path, branch, createTrack });
}

export function gitCreateBranch(path: string, branch: string): Promise<void> {
  return invoke<void>("git_create_branch", { path, branch });
}

export interface GitWorktree {
  path: string;
  head: string | null;
  branch: string | null;
}

export function listGitWorktrees(path: string): Promise<GitWorktree[]> {
  return invoke<GitWorktree[]>("list_git_worktrees", { path });
}

export type GitFileStatus =
  | "modified"
  | "added"
  | "deleted"
  | "renamed"
  | "copied";

export interface GitFileSummary {
  path: string;
  status: GitFileStatus;
  additions: number;
  deletions: number;
}

export interface GitFileContents {
  before: string;
  after: string;
}

// Cheap call: every changed file in the working tree (vs HEAD) plus
// untracked files, with line stats only — no contents. Empty list
// when `path` isn't a git repo. Drives the file list and the
// header badge.
export function getGitDiffSummary(path: string): Promise<GitFileSummary[]> {
  return invoke<GitFileSummary[]>("get_git_diff_summary", { path });
}

// Lazy per-file content fetch — called by the diff panel only when
// the user actually expands a file. Keeps the heavy work out of the
// initial open path.
export function getGitDiffFile(
  path: string,
  file: string,
): Promise<GitFileContents> {
  return invoke<GitFileContents>("get_git_diff_file", { path, file });
}

// Every file in `path` that isn't ignored by .gitignore / .ignore,
// returned as forward-slash relative paths. Used by the /code
// editor view's Cmd+P-style picker. Capped at 20k entries on the
// Rust side so huge monorepos don't blow up the IPC bridge.
export function listProjectFiles(path: string): Promise<string[]> {
  return invoke<string[]>("list_project_files", { path });
}

// Flowzen-app-owned key/value store. Backed by SQLite at
// <app_data_dir>/user_config.sqlite — separate from the agent
// SDK's daemon database. SDK and app each own their own SQLite;
// app-level UI tunables (pool size, future toggles) live here, not
// in the daemon's schema.
//
// `getUserConfig` returns null when the key has never been set;
// callers should treat that as "use the default."
export function getUserConfig(key: string): Promise<string | null> {
  return invoke<string | null>("get_user_config", { key });
}

export function setUserConfig(key: string, value: string): Promise<void> {
  return invoke<void>("set_user_config", { key, value });
}

// Resolved cross-platform app data dir for Flowzen — the same
// directory the daemon database, threads dir, and user_config
// sqlite live under. Surfaced to the Settings UI as a read-only
// path so users can copy it and open in Finder / Explorer / a
// terminal.
export function getAppDataDir(): Promise<string> {
  return invoke<string>("get_app_data_dir");
}

// Read a single project file as a UTF-8 string. Rejects on:
//   * file outside the project root (canonicalisation escape)
//   * file above CODE_VIEW_MAX_FILE_BYTES (4 MiB)
//   * non-UTF-8 content
// Callers should `.catch` to render a friendly placeholder.
export function readProjectFile(path: string, file: string): Promise<string> {
  return invoke<string>("read_project_file", { path, file });
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

// Spawn an external code editor (`zed`, `code`, `cursor`, `idea`,
// `subl`, …) on the project root. The rust side calls the binary
// with the path as a positional arg and detaches; the promise
// rejects when the binary isn't on $PATH or the path isn't a
// directory so the frontend can show a friendly toast.
export function openInEditor(editor: string, path: string): Promise<void> {
  return invoke<void>("open_in_editor", { editor, path });
}

// Integrated terminal — PTY control plane. Frontend pairs this
// with @xterm/xterm on the render side. `openPty` creates a shell
// child and returns a numeric id; the provided onData channel
// delivers the shell's raw byte output (as a number array today;
// upgradeable to ArrayBuffer when we care). All the other helpers
// take that id as the first arg.
export type PtyId = number;

export interface OpenPtyOptions {
  cols: number;
  rows: number;
  cwd?: string;
  shell?: string;
  onData: (bytes: number[]) => void;
}

export function openPty(opts: OpenPtyOptions): Promise<PtyId> {
  const channel = new Channel<number[]>();
  channel.onmessage = opts.onData;
  return invoke<PtyId>("pty_open", {
    cols: opts.cols,
    rows: opts.rows,
    cwd: opts.cwd ?? null,
    shell: opts.shell ?? null,
    onData: channel,
  });
}

export function writePty(id: PtyId, data: Uint8Array): Promise<void> {
  return invoke<void>("pty_write", { id, data: Array.from(data) });
}

export function resizePty(
  id: PtyId,
  cols: number,
  rows: number,
): Promise<void> {
  return invoke<void>("pty_resize", { id, cols, rows });
}

export function pausePty(id: PtyId): Promise<void> {
  return invoke<void>("pty_pause", { id });
}

export function resumePty(id: PtyId): Promise<void> {
  return invoke<void>("pty_resume", { id });
}

export function killPty(id: PtyId): Promise<void> {
  return invoke<void>("pty_kill", { id });
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
