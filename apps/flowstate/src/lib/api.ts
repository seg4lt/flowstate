import { Channel, invoke } from "@tauri-apps/api/core";
import type {
  AttachmentData,
  ClientMessage,
  ContextBreakdown,
  ServerMessage,
} from "./types";

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

/**
 * Fetch the per-category context-usage breakdown for a session's
 * active turn. Only works while a turn is in flight — the provider
 * adapter's `get_context_usage` is a mid-turn RPC under the hood,
 * which only resolves when `run_turn`'s drain loop is alive to
 * route the response. Returns `null` when the session has no live
 * bridge or the provider doesn't support the RPC. Throws on
 * `ServerMessage::Error` (timeouts, kind mismatches, etc.) so the
 * caller can surface a distinct message rather than silently
 * treating errors as "unavailable".
 */
export async function getContextUsage(
  sessionId: string,
): Promise<ContextBreakdown | null> {
  const resp = await sendMessage({
    type: "get_context_usage",
    session_id: sessionId,
  });
  if (resp?.type === "context_usage") return resp.breakdown;
  if (resp?.type === "error") throw new Error(resp.message);
  throw new Error("unexpected response to get_context_usage");
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

export function gitDeleteBranch(path: string, branch: string): Promise<void> {
  return invoke<void>("git_delete_branch", { path, branch });
}

/** Resolve the git repository root for `path` via
 * `git rev-parse --show-toplevel`. Returns `null` when the path is
 * not inside a git repo. Critical for submodule / linked-worktree
 * directories where the raw path may differ from the repo root. */
export function resolveGitRoot(path: string): Promise<string | null> {
  return invoke<string | null>("resolve_git_root", { path });
}

export interface GitWorktree {
  path: string;
  head: string | null;
  branch: string | null;
}

export function listGitWorktrees(path: string): Promise<GitWorktree[]> {
  return invoke<GitWorktree[]>("list_git_worktrees", { path });
}

// Create a new linked worktree at `worktreePath` rooted in
// `projectPath`. When `checkoutExisting` is false (default), git
// runs `worktree add -b <branch> <worktreePath> <baseRef>` to
// create a new branch. When `checkoutExisting` is true, git runs
// `worktree add <worktreePath> <branch>` to check out an existing
// branch. On success returns the freshly-parsed GitWorktree entry
// so the caller can avoid an extra list round-trip.
export function createGitWorktree(
  projectPath: string,
  worktreePath: string,
  branch: string,
  baseRef: string,
  checkoutExisting?: boolean,
): Promise<GitWorktree> {
  return invoke<GitWorktree>("create_git_worktree", {
    projectPath,
    worktreePath,
    branch,
    baseRef,
    checkoutExisting: checkoutExisting ?? false,
  });
}

// Remove the worktree at `worktreePath` (rooted in `projectPath`).
// `force=false` runs plain `git worktree remove`, which fails loud
// on dirty working trees — the frontend surfaces stderr and can
// retry with `force=true`, which adds `--force`.
export function removeGitWorktree(
  projectPath: string,
  worktreePath: string,
  force: boolean,
): Promise<void> {
  return invoke<void>("remove_git_worktree", {
    projectPath,
    worktreePath,
    force,
  });
}

// Cheap existence probe used by the chat view to detect when a
// worktree folder has been removed out from under flowstate — the
// composer flips to read-only via the same infra as archived
// threads.
export function pathExists(path: string): Promise<boolean> {
  return invoke<boolean>("path_exists", { path });
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

// ── Streaming diff summary ────────────────────────────────────────
//
// `getGitDiffSummary` above is a one-shot call that doesn't return
// until git has fully computed the diff — on a monorepo with many
// changes that can take tens of seconds, during which the UI has
// nothing to render. `watchGitDiffSummary` streams the same shape in
// two phases over a Tauri Channel:
//
//   Phase 1 (`files`): fast `git status` pass, returns the file list
//     near-instantly with untracked line counts already populated
//     and tracked entries placeholdered at 0 / 0.
//   Phase 2 (`numstat`): one event per tracked file as git produces
//     its numstat record, so counts hydrate progressively.
//   `done`: terminal event; `ok: false` carries the error message
//     for timeouts / cancellations / subprocess failures.
//
// The returned handle includes a `stop()` that the caller MUST
// invoke on cleanup — it kills the git subprocess and frees the
// slot in the Rust-side task map.

export type DiffSummaryEvent =
  | { kind: "files"; files: GitFileSummary[] }
  | {
      kind: "numstat";
      path: string;
      additions: number;
      deletions: number;
    }
  | { kind: "done"; ok: boolean; error: string | null };

let nextDiffToken = 1;
function allocDiffToken(): number {
  const t = nextDiffToken;
  nextDiffToken += 1;
  return t;
}

export interface DiffSummarySubscription {
  token: number;
  stop: () => void;
}

export function watchGitDiffSummary(
  path: string,
  onEvent: (event: DiffSummaryEvent) => void,
): DiffSummarySubscription {
  const token = allocDiffToken();
  const channel = new Channel<DiffSummaryEvent>();
  channel.onmessage = onEvent;
  // Fire and forget — the command returns as soon as the Rust side
  // has spawned its blocking worker. Everything after that flows
  // through the channel.
  void invoke("watch_git_diff_summary", { path, token, onEvent: channel });
  return {
    token,
    stop: () => {
      void invoke("stop_git_diff_summary", { token });
    },
  };
}

// Every file in `path` that isn't ignored by .gitignore / .ignore,
// returned as forward-slash relative paths. Used by the /code
// editor view's Cmd+P-style picker. Capped at 20k entries on the
// Rust side so huge monorepos don't blow up the IPC bridge.
export function listProjectFiles(path: string): Promise<string[]> {
  return invoke<string[]>("list_project_files", { path });
}

// Flowstate-app-owned key/value store. Backed by SQLite at
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

// Per-session and per-project display metadata: titles, names,
// previews, ordering. Persisted in the same `user_config.sqlite`
// file as the kv store above, in dedicated tables. The agent SDK
// no longer stores any of this — it only persists fields the
// runtime needs to execute or resume agents. See
// `rs-agent-sdk/crates/core/persistence/CLAUDE.md` for the boundary.

export interface SessionDisplay {
  title: string | null;
  lastTurnPreview: string | null;
}

export interface ProjectDisplay {
  name: string | null;
  sortOrder: number | null;
}

export function setSessionDisplay(
  sessionId: string,
  display: SessionDisplay,
): Promise<void> {
  return invoke<void>("set_session_display", { sessionId, display });
}

export function getSessionDisplay(
  sessionId: string,
): Promise<SessionDisplay | null> {
  return invoke<SessionDisplay | null>("get_session_display", { sessionId });
}

export function listSessionDisplay(): Promise<Record<string, SessionDisplay>> {
  return invoke<Record<string, SessionDisplay>>("list_session_display");
}

export function deleteSessionDisplay(sessionId: string): Promise<void> {
  return invoke<void>("delete_session_display", { sessionId });
}

export function setProjectDisplay(
  projectId: string,
  display: ProjectDisplay,
): Promise<void> {
  return invoke<void>("set_project_display", { projectId, display });
}

export function getProjectDisplay(
  projectId: string,
): Promise<ProjectDisplay | null> {
  return invoke<ProjectDisplay | null>("get_project_display", { projectId });
}

export function listProjectDisplay(): Promise<Record<string, ProjectDisplay>> {
  return invoke<Record<string, ProjectDisplay>>("list_project_display");
}

export function deleteProjectDisplay(projectId: string): Promise<void> {
  return invoke<void>("delete_project_display", { projectId });
}

// Parent/child worktree links. Each worktree has its own SDK
// project so the agent SDK's existing cwd resolution "just works",
// and this table records "this SDK project is a git worktree of
// that SDK project, on branch Z". The flowstate sidebar reads these
// to group worktree threads under the parent project visually —
// the SDK has no concept of worktrees.
export interface ProjectWorktree {
  projectId: string;
  parentProjectId: string;
  branch: string | null;
}

export function setProjectWorktree(
  projectId: string,
  parentProjectId: string,
  branch: string | null,
): Promise<void> {
  return invoke<void>("set_project_worktree", {
    projectId,
    parentProjectId,
    branch,
  });
}

export function getProjectWorktree(
  projectId: string,
): Promise<ProjectWorktree | null> {
  return invoke<ProjectWorktree | null>("get_project_worktree", { projectId });
}

export function listProjectWorktree(): Promise<Record<string, ProjectWorktree>> {
  return invoke<Record<string, ProjectWorktree>>("list_project_worktree");
}

export function deleteProjectWorktree(projectId: string): Promise<void> {
  return invoke<void>("delete_project_worktree", { projectId });
}

// Resolved cross-platform app data dir for Flowstate — the same
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
