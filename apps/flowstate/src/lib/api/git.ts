import { Channel, invoke } from "@tauri-apps/api/core";

// Git-shelling wrappers for the branch switcher, diff panel, and
// streamed diff summary. Each call resolves to a typed payload the
// Rust side has already parsed — frontend never runs git itself.

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
