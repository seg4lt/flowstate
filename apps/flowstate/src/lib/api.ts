import { Channel, invoke } from "@tauri-apps/api/core";
import type {
  AttachmentData,
  CheckpointSettings,
  ClientMessage,
  ContextBreakdown,
  RateLimitInfo,
  RewindOutcomeWire,
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
  if (resp?.type === "context_usage") return resp.breakdown ?? null;
  if (resp?.type === "error") throw new Error(resp.message);
  throw new Error("unexpected response to get_context_usage");
}

/**
 * Rewind the session's workspace to its state just before `turnId`.
 *
 * Two flags that are ALWAYS explicit on the wire (the Rust enum has
 * them as bare `bool` not `Option<bool>` — the TS types reflect that
 * faithfully):
 *
 * - `dryRun` — when true, the runtime reports the paths it WOULD
 *   touch and leaves disk unchanged. Use this to render the preview
 *   dialog. Defaults to false.
 * - `confirmConflicts` — when false (default), any file the session
 *   has seen modified elsewhere since last observation causes the
 *   call to halt with `NeedsConfirmation`. Re-issue with `true` to
 *   proceed and clobber the outside change.
 *
 * Returns the structured outcome (applied | needs_confirmation |
 * unavailable); callers exhaustively switch on `outcome.kind`.
 * Throws only on transport errors or infrastructure `Error`
 * responses (IO failure, sqlite, etc) — semantic "unavailable"
 * states surface as an Applied/Unavailable outcome, not an exception.
 */
export async function rewindFiles(params: {
  sessionId: string;
  turnId: string;
  dryRun?: boolean;
  confirmConflicts?: boolean;
}): Promise<RewindOutcomeWire> {
  const resp = await sendMessage({
    type: "rewind_files",
    session_id: params.sessionId,
    turn_id: params.turnId,
    dry_run: params.dryRun ?? false,
    confirm_conflicts: params.confirmConflicts ?? false,
  });
  if (resp?.type === "rewind_files_result") return resp.outcome;
  if (resp?.type === "error") throw new Error(resp.message);
  throw new Error("unexpected response to rewind_files");
}

/**
 * Fetch the current checkpoint settings (global default + any project
 * overrides). Bootstrap already ships the same payload on first
 * connect; this API exists for explicit refreshes (settings dialog
 * reopen, after a long sleep, etc.).
 */
export async function getCheckpointSettings(): Promise<CheckpointSettings> {
  const resp = await sendMessage({ type: "get_checkpoint_settings" });
  if (resp?.type === "checkpoint_settings_snapshot") return resp.settings;
  if (resp?.type === "error") throw new Error(resp.message);
  throw new Error("unexpected response to get_checkpoint_settings");
}

/**
 * Flip the global checkpoint-enablement flag. Returns the new
 * snapshot so callers can update their local cache without waiting
 * for the `CheckpointEnablementChanged` broadcast round-trip.
 */
export async function setCheckpointsGlobalEnabled(
  enabled: boolean,
): Promise<CheckpointSettings> {
  const resp = await sendMessage({
    type: "set_checkpoints_enabled",
    enabled,
  });
  if (resp?.type === "checkpoint_settings_snapshot") return resp.settings;
  if (resp?.type === "error") throw new Error(resp.message);
  throw new Error("unexpected response to set_checkpoints_enabled");
}

/** Result handle returned by [`connectStream`]. Call `cancel()` from
 *  the caller's React effect cleanup so a StrictMode double-invoke
 *  (or unmount mid-handshake) doesn't leak a retry timer or a
 *  half-open channel. */
export interface ConnectStreamHandle {
  /** Stop any in-flight retry timer. Idempotent. After cancel the
   *  loop will not call `onConnected` / `onGiveUp` even if a stale
   *  timer fires. Has no effect on a channel that already connected
   *  successfully — that channel keeps streaming until the daemon
   *  closes it or the page unloads. */
  cancel(): void;
}

/** Optional lifecycle callbacks `connectStream` will fire as it works
 *  through its retry loop. The default-export contract (calling
 *  `connectStream(onMessage)` with no options) keeps the legacy
 *  fire-and-forget shape for the few non-AppProvider callers. */
export interface ConnectStreamOptions {
  /** Fires once when `invoke("connect")` resolves successfully and
   *  the channel is live. Distinct from `welcome` arriving — that's
   *  delivered through `onMessage` like any other ServerMessage. */
  onConnected?: () => void;
  /** Fires if the retry budget is exhausted without ever connecting.
   *  The splash uses this to swap to a "couldn't reach daemon" error
   *  card so the user has something actionable instead of a forever
   *  spinner. `attempts` and `elapsedMs` are passed for telemetry. */
  onGiveUp?: (info: { attempts: number; elapsedMs: number; lastError: unknown }) => void;
  /** Called on every failed attempt (NOT the final give-up). Useful
   *  for `console.debug` so a long-but-eventually-successful boot
   *  leaves a breadcrumb trail in the devtools console. */
  onAttemptFailed?: (info: { attempt: number; nextDelayMs: number; error: unknown }) => void;
}

// Backoff schedule. The first few short retries cover the race where
// AppProvider mounts a few hundred ms before `transport.serve()`
// manages `TauriDaemonState` (warm-cache case). The longer steady-
// state retries cover the cold-cache case where `provision_runtimes`
// is grinding through `npm ci` for ~30-90s.
//
// Total budget = sum + steady-state * (CONNECT_MAX_ATTEMPTS - len) =
//   250 + 500 + 1000 + 2000 + 5000 + 5000*55 ≈ 5min 8s. Long enough
// to outlast the slowest first-launch install we've measured (~3 min)
// without trapping the user behind a stuck splash forever if the
// daemon never comes up at all.
const CONNECT_BACKOFF_MS = [250, 500, 1000, 2000, 5000] as const;
const CONNECT_STEADY_DELAY_MS = 5000;
const CONNECT_MAX_ATTEMPTS = 60;

/** Subscribe to the daemon's ServerMessage stream. Retries the
 *  `connect` Tauri command until it succeeds, with capped backoff,
 *  so a cold-cache first launch (where `provision_runtimes` blocks
 *  the daemon spawn for ~60s) doesn't leave the splash stuck on
 *  "Finishing up…" forever — the previous single-shot version
 *  silently dropped the rejected promise and never reattempted.
 *
 *  Each attempt builds a fresh `Channel`. Tauri rejects the invoke
 *  before reaching the Rust command body when `TauriDaemonState`
 *  isn't yet managed, so a failed attempt has no Rust-side state to
 *  clean up — the unused channel is GC'd by JS. Once one invoke
 *  resolves we stop retrying; the live channel keeps streaming until
 *  the daemon closes it.
 *
 *  Returns a handle whose `cancel()` should be called from the
 *  React effect cleanup. */
export function connectStream(
  onMessage: (message: ServerMessage) => void,
  options: ConnectStreamOptions = {},
): ConnectStreamHandle {
  let cancelled = false;
  let pendingTimer: ReturnType<typeof setTimeout> | null = null;
  const startedAt = Date.now();

  const attempt = async (n: number): Promise<void> => {
    if (cancelled) return;
    const channel = new Channel<ServerMessage>();
    channel.onmessage = onMessage;
    try {
      await invoke("connect", { onEvent: channel });
      if (cancelled) {
        // Caller went away mid-handshake. We can't recall the channel
        // from Rust, but the daemon will notice the disconnect when
        // the webview tears down. Don't fire onConnected.
        return;
      }
      options.onConnected?.();
    } catch (err) {
      if (cancelled) return;
      if (n + 1 >= CONNECT_MAX_ATTEMPTS) {
        options.onGiveUp?.({
          attempts: n + 1,
          elapsedMs: Date.now() - startedAt,
          lastError: err,
        });
        return;
      }
      const delay =
        n < CONNECT_BACKOFF_MS.length
          ? CONNECT_BACKOFF_MS[n]!
          : CONNECT_STEADY_DELAY_MS;
      options.onAttemptFailed?.({
        attempt: n + 1,
        nextDelayMs: delay,
        error: err,
      });
      pendingTimer = setTimeout(() => {
        pendingTimer = null;
        void attempt(n + 1);
      }, delay);
    }
  };

  void attempt(0);

  return {
    cancel() {
      cancelled = true;
      if (pendingTimer !== null) {
        clearTimeout(pendingTimer);
        pendingTimer = null;
      }
    },
  };
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

// Snapshot of the per-worktree file index returned by
// `list_project_files`. Mirrors the Rust `ProjectFileListing` struct
// (camelCase from serde's `rename_all = "camelCase"`).
//
// `files` is the full forward-slash relative-path list as currently
// indexed by fff-search. There is **no** server-side cap — on a
// 100k-file repo it's ~8 MB of JSON and the picker virtualises it
// client-side. While `indexing` is true the background scanner is
// still walking; React Query refetches on a short stale window so
// the picker fills in live.
export interface ProjectFileListing {
  files: string[];
  indexing: boolean;
  // Files indexed so far (== files.length). Surfaced separately so
  // the picker header can show "Indexing N files…" without
  // recomputing.
  scanned: number;
}

// Every file in `path` that isn't ignored by .gitignore / .ignore,
// returned as forward-slash relative paths inside a
// `ProjectFileListing`. Used by the /code editor view's Cmd+P-style
// picker. **Not capped** — the previous 20k cap silently dropped
// most files on a 100k-file repo and made it impossible to find
// files even by typing the exact name.
export function listProjectFiles(path: string): Promise<ProjectFileListing> {
  return invoke<ProjectFileListing>("list_project_files", { path });
}

// Drop the cached fff-search file picker for `path` so the next
// `listProjectFiles` rebuilds it from a fresh scan. Wired up from
// the chat session's `turn_completed` event — agent edits that
// touch many files in quick succession can outrun fs-event
// coalescing on macOS, so we explicitly reindex at the moment the
// user is most likely to look at the picker again. No-op when
// `path` was never indexed.
export function reindexProjectFiles(path: string): Promise<void> {
  return invoke<void>("reindex_project_files", { path });
}

// One entry returned by `listDirectory`. `isIgnored` is true when the
// entry is covered by a gitignore rule — the frontend shows it anyway,
// just dimmed, so users can drill into ignored dirs on demand.
export interface DirEntry {
  name: string;
  isDir: boolean;
  isIgnored: boolean;
}

// List the immediate children (1 level only) of a project-relative
// directory, INCLUDING gitignored entries. Used by the /code view's
// file tree for lazy, on-click expansion so node_modules/ and dist/
// never get eagerly walked. Pass an empty `subPath` to list the
// project root.
export function listDirectory(
  path: string,
  subPath: string,
): Promise<DirEntry[]> {
  return invoke<DirEntry[]>("list_directory", { path, subPath });
}

// Flowstate-app-owned key/value store. Backed by SQLite at
// <app_data_dir>/user_config.sqlite — separate from the agent
// SDK's daemon database. SDK and app each own their own SQLite;
// app-level UI tunables (pool size, future toggles) live here, not
// in the daemon's schema.
//
// `getUserConfig` returns null when the key has never been set;
// callers should treat that as "use the default."

// Module-level read-through cache for `get_user_config`. The keys
// addressed here (`defaults.model.<provider>`, `defaults.provider`,
// `provider.enabled.<provider>`, `defaults.effort`, etc.) are
// written from a single place per key (Settings UI / explicit user
// action) and read from many places, so a JS-side cache is safe
// and dramatically reduces IPC pressure during boot.
//
// Why this matters: during a fresh launch, `provider-dropdown.tsx`
// and `worktree-new-thread-dropdown.tsx` each run an effect with a
// `[state.providers]` dep that calls `readDefaultModel(p.kind)` for
// every ready provider. As providers transition to `ready` one by
// one (one `state.providers` identity change per transition), the
// effect re-fires; multiplied across multiple sidebar instances and
// 4–5 providers, a real recording from a user's machine showed
// **70 get_user_config IPC calls in ~50 ms** — the same key
// (`defaults.model.claude`) fetched 14 times. That storm queues on
// Tauri's IPC thread, blocks the JS event loop while responses
// arrive, and downstream produced a 53 ms `message` handler, a
// 50 ms `message` handler, an 82 ms `scroll` handler, and a 307-
// paint storm in a single 100 ms window — the visible "refresh
// blip" + scroll/cursor displacement the user reported.
//
// The cache stores Promises (not resolved values) so concurrent
// callers all await the same in-flight invoke instead of racing
// new ones. `setUserConfig` writes through so subsequent reads
// see the new value without an extra IPC round-trip; the rejection
// path drops the entry so a transient failure doesn't poison the
// cache forever.
const userConfigCache = new Map<string, Promise<string | null>>();

export function getUserConfig(key: string): Promise<string | null> {
  const cached = userConfigCache.get(key);
  if (cached !== undefined) return cached;
  const pending = invoke<string | null>("get_user_config", { key });
  userConfigCache.set(key, pending);
  pending.catch(() => {
    // Drop the failed promise so the next caller can retry rather
    // than re-await a rejected handle for the rest of the session.
    if (userConfigCache.get(key) === pending) {
      userConfigCache.delete(key);
    }
  });
  return pending;
}

export async function setUserConfig(key: string, value: string): Promise<void> {
  await invoke<void>("set_user_config", { key, value });
  // Write-through: prime the cache with the just-written value so
  // any reader that lands after this point gets the fresh answer
  // without an extra round-trip. We resolve a fresh promise rather
  // than reusing a possibly-still-pending older one.
  userConfigCache.set(key, Promise.resolve(value));
}

/** Clear all cached user-config reads. Exposed for tests and as an
 *  escape hatch if a future migration writes config out-of-band; not
 *  used by app code today. */
export function clearUserConfigCache(): void {
  userConfigCache.clear();
}

// ─────────────────────────────────────────────────────────────────
// macOS caffeinate — display-sleep prevention. Commands are only
// registered on macOS by the Tauri shell, so callers must guard on
// platform before invoking. The settings UI uses the shared
// `platform()` import from `@tauri-apps/plugin-os` for that gate.
// ─────────────────────────────────────────────────────────────────

export interface CaffeinateStatus {
  enabled: boolean;
  running: boolean;
  pid: number | null;
}

export function getCaffeinateStatus(): Promise<CaffeinateStatus> {
  return invoke<CaffeinateStatus>("caffeinate_status");
}

/** Tell the controller to re-evaluate now (after a toggle write). */
export function refreshCaffeinate(): Promise<void> {
  return invoke<void>("caffeinate_refresh");
}

/** Force-kill the running caffeinate child. Setting stays enabled —
 *  caffeinate respawns on the next 0→1 turn transition. */
export function killCaffeinate(): Promise<void> {
  return invoke<void>("caffeinate_kill");
}

// ─────────────────────────────────────────────────────────────────
// Binary search paths — user-configured "look here too" override
// ─────────────────────────────────────────────────────────────────
//
// Wraps the `binaries.search_paths` user_config key. Storage is a
// JSON-encoded array of strings; the Settings UI calls
// `setUserConfig(...)` to persist and then `refreshBinarySearchPaths`
// so the in-process resolver picks up the change without a daemon
// restart.

/** Tell the daemon to re-read `binaries.search_paths` from
 *  user_config and update the in-process resolver. Call right after
 *  writing the key via `setUserConfig`. */
export function refreshBinarySearchPaths(): Promise<void> {
  return invoke<void>("refresh_binary_search_paths");
}

/** Snapshot of the directories the resolver is currently consulting
 *  beyond PATH + the curated platform fallbacks. Useful for the
 *  Settings UI to confirm the daemon is seeing what was configured. */
export function listBinarySearchPaths(): Promise<string[]> {
  return invoke<string[]>("list_binary_search_paths");
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

// Read-through caches for the three "list_*" display endpoints.
// Each endpoint returns 5–25 KB of JSON and lives on the boot
// critical path — `app-store.tsx`'s mount effect fires
// `Promise.all([listSessionDisplay, listProjectDisplay,
// listProjectWorktree])` while `connectStream` is also in flight.
// In a real user recording the three calls returned 22.8 / 5.0 /
// 10.7 KB respectively in ~12 ms parallel; that's fine *once* but
// `usage-top-sessions-table.tsx` re-fires `listSessionDisplay()`
// on every visit to /usage, and any future feature that needs the
// same map again pays a fresh IPC + 22 KB reparse. The cache makes
// repeat reads free and dedupes concurrent callers (multiple
// components mounting in the same render frame all await one
// in-flight invoke). Single-shot mutators (`setSessionDisplay`
// etc.) write through, batch reorderings invalidate the relevant
// cache so the next read sees the persisted truth.
let sessionDisplayCache: Promise<Record<string, SessionDisplay>> | null = null;
let projectDisplayCache: Promise<Record<string, ProjectDisplay>> | null = null;
let projectWorktreeCache: Promise<Record<string, ProjectWorktree>> | null = null;

export async function setSessionDisplay(
  sessionId: string,
  display: SessionDisplay,
): Promise<void> {
  await invoke<void>("set_session_display", { sessionId, display });
  // Patch the cached map so subsequent listSessionDisplay() reads
  // see this write without an extra IPC. Falling back to a full
  // invalidate keeps semantics correct even if the cache is empty.
  if (sessionDisplayCache) {
    sessionDisplayCache = sessionDisplayCache
      .then((rec) => ({ ...rec, [sessionId]: display }))
      .catch(() => {
        sessionDisplayCache = null;
        return {} as Record<string, SessionDisplay>;
      });
  }
}

export function getSessionDisplay(
  sessionId: string,
): Promise<SessionDisplay | null> {
  return invoke<SessionDisplay | null>("get_session_display", { sessionId });
}

export function listSessionDisplay(): Promise<Record<string, SessionDisplay>> {
  if (sessionDisplayCache !== null) return sessionDisplayCache;
  const pending = invoke<Record<string, SessionDisplay>>("list_session_display");
  sessionDisplayCache = pending;
  pending.catch(() => {
    if (sessionDisplayCache === pending) sessionDisplayCache = null;
  });
  return pending;
}

export async function deleteSessionDisplay(sessionId: string): Promise<void> {
  await invoke<void>("delete_session_display", { sessionId });
  if (sessionDisplayCache) {
    sessionDisplayCache = sessionDisplayCache
      .then((rec) => {
        // Avoid in-place mutation — other awaiters may still hold
        // the same Record reference.
        const { [sessionId]: _omit, ...rest } = rec;
        return rest;
      })
      .catch(() => {
        sessionDisplayCache = null;
        return {} as Record<string, SessionDisplay>;
      });
  }
}

export async function setProjectDisplay(
  projectId: string,
  display: ProjectDisplay,
): Promise<void> {
  await invoke<void>("set_project_display", { projectId, display });
  if (projectDisplayCache) {
    projectDisplayCache = projectDisplayCache
      .then((rec) => ({ ...rec, [projectId]: display }))
      .catch(() => {
        projectDisplayCache = null;
        return {} as Record<string, ProjectDisplay>;
      });
  }
}

export function getProjectDisplay(
  projectId: string,
): Promise<ProjectDisplay | null> {
  return invoke<ProjectDisplay | null>("get_project_display", { projectId });
}

export function listProjectDisplay(): Promise<Record<string, ProjectDisplay>> {
  if (projectDisplayCache !== null) return projectDisplayCache;
  const pending = invoke<Record<string, ProjectDisplay>>("list_project_display");
  projectDisplayCache = pending;
  pending.catch(() => {
    if (projectDisplayCache === pending) projectDisplayCache = null;
  });
  return pending;
}

export async function deleteProjectDisplay(projectId: string): Promise<void> {
  await invoke<void>("delete_project_display", { projectId });
  if (projectDisplayCache) {
    projectDisplayCache = projectDisplayCache
      .then((rec) => {
        const { [projectId]: _omit, ...rest } = rec;
        return rest;
      })
      .catch(() => {
        projectDisplayCache = null;
        return {} as Record<string, ProjectDisplay>;
      });
  }
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

export async function setProjectWorktree(
  projectId: string,
  parentProjectId: string,
  branch: string | null,
): Promise<void> {
  await invoke<void>("set_project_worktree", {
    projectId,
    parentProjectId,
    branch,
  });
  if (projectWorktreeCache) {
    const record: ProjectWorktree = { projectId, parentProjectId, branch };
    projectWorktreeCache = projectWorktreeCache
      .then((rec) => ({ ...rec, [projectId]: record }))
      .catch(() => {
        projectWorktreeCache = null;
        return {} as Record<string, ProjectWorktree>;
      });
  }
}

export function getProjectWorktree(
  projectId: string,
): Promise<ProjectWorktree | null> {
  return invoke<ProjectWorktree | null>("get_project_worktree", { projectId });
}

export function listProjectWorktree(): Promise<Record<string, ProjectWorktree>> {
  if (projectWorktreeCache !== null) return projectWorktreeCache;
  const pending = invoke<Record<string, ProjectWorktree>>("list_project_worktree");
  projectWorktreeCache = pending;
  pending.catch(() => {
    if (projectWorktreeCache === pending) projectWorktreeCache = null;
  });
  return pending;
}

export async function deleteProjectWorktree(projectId: string): Promise<void> {
  await invoke<void>("delete_project_worktree", { projectId });
  if (projectWorktreeCache) {
    projectWorktreeCache = projectWorktreeCache
      .then((rec) => {
        const { [projectId]: _omit, ...rest } = rec;
        return rest;
      })
      .catch(() => {
        projectWorktreeCache = null;
        return {} as Record<string, ProjectWorktree>;
      });
  }
}

/** Drop all cached display lists. Exposed for tests / migrations;
 *  not used by app code today. */
export function clearDisplayCaches(): void {
  sessionDisplayCache = null;
  projectDisplayCache = null;
  projectWorktreeCache = null;
}

// Resolved cross-platform app data dir for Flowstate — the same
// directory the daemon database, threads dir, and user_config
// sqlite live under. Surfaced to the Settings UI as a read-only
// path so users can copy it and open in Finder / Explorer / a
// terminal.
export function getAppDataDir(): Promise<string> {
  return invoke<string>("get_app_data_dir");
}

// Platform-conventional log directory — `~/Library/Logs/Flowstate`
// on macOS, XDG state on Linux, %LOCALAPPDATA%/Flowstate/logs on
// Windows. Surfaced in Settings → Diagnostics next to a Reveal
// button so users can find `flowstate.log` when troubleshooting.
export function getLogDir(): Promise<string> {
  return invoke<string>("get_log_dir");
}

// Cache directory holding the embedded Node.js runtime + provider
// SDK `node_modules/` trees (~350 MB after first launch). Surfaced
// so users can find / wipe the cache when troubleshooting a botched
// first install.
export function getCacheDir(): Promise<string> {
  return invoke<string>("get_cache_dir");
}

// Recursively delete the runtime cache directory. Resolves with the
// number of bytes freed (best-effort). Process-wide OnceLocks still
// hold paths into the now-deleted dir, so the UI nudges the user to
// relaunch. Resolves with 0 if the dir was already gone.
export function clearRuntimeCache(): Promise<number> {
  return invoke<number>("clear_runtime_cache");
}

// CLI installation. Copies/symlinks the bundled `flow` binary onto
// the user's PATH so they can run `flow .` from any terminal.
//
// `target`:
//   - "user_local"  → ~/.local/bin/flow on macOS/Linux, or
//                     %LOCALAPPDATA%\Programs\flowstate\bin\flow.exe
//                     on Windows. No password prompt.
//   - "system"      → /usr/local/bin/flow on macOS/Linux via the
//                     OS admin password dialog. Unsupported on
//                     Windows (the command rejects with a clear
//                     error message).
export type InstallCliTarget = "user_local" | "system";

export interface InstallCliReport {
  installedPath: string;
  sourcePath: string;
  /** True when the install dir is on the user's PATH. */
  onPath: boolean;
  target: InstallCliTarget;
}

export interface InstallCliStatus {
  installed: boolean;
  installedPath: string | null;
  sourcePath: string;
  /** True when the existing install resolves to the same binary
   *  shipping with this Flowstate version. False signals a stale
   *  link from a moved or upgraded app — the UI offers Reinstall. */
  pointsAtCurrent: boolean;
  onPath: boolean;
}

// Rust returns snake_case keys via serde. We translate to
// camelCase here so call sites read like idiomatic TS without
// littering the components with `installed_path` reads.
interface InstallCliReportWire {
  installed_path: string;
  source_path: string;
  on_path: boolean;
  target: InstallCliTarget;
}

interface InstallCliStatusWire {
  installed: boolean;
  installed_path: string | null;
  source_path: string;
  points_at_current: boolean;
  on_path: boolean;
}

export async function installCli(
  target: InstallCliTarget,
): Promise<InstallCliReport> {
  const wire = await invoke<InstallCliReportWire>("install_cli", { target });
  return {
    installedPath: wire.installed_path,
    sourcePath: wire.source_path,
    onPath: wire.on_path,
    target: wire.target,
  };
}

export async function installCliStatus(): Promise<InstallCliStatus> {
  const wire = await invoke<InstallCliStatusWire>("install_cli_status");
  return {
    installed: wire.installed,
    installedPath: wire.installed_path,
    sourcePath: wire.source_path,
    pointsAtCurrent: wire.points_at_current,
    onPath: wire.on_path,
  };
}

// Re-run a single runtime-provisioning phase ("node" | "claude-sdk"
// | "copilot-sdk"). Used by the Settings page Retry buttons.
// Resolves on success; rejects with the error string from Rust on
// failure (which is what the toast surfaces).
export function retryProvisionPhase(phase: string): Promise<void> {
  return invoke<void>("retry_provision_phase", { phase });
}

// Usage analytics — reads of the flowstate-app-owned
// `<app_data_dir>/usage.sqlite`. Writes happen on the Rust side
// via a subscriber task on `RuntimeEvent::TurnCompleted`; the
// frontend only reads aggregates. The SDK's daemon database is
// never touched by these queries — analytics are display-only
// and live entirely in the app's store. See
// `src-tauri/src/usage.rs` for schema and boundary rationale.

// Mirrors `flowstate_app_layer::usage::UsageRange` (serde
// snake_case, externally-tagged enum). Presets are bare strings;
// the custom variant carries `from`/`to` as `YYYY-MM-DD` UTC day
// strings — the dashboard's date input only resolves to whole
// days, and the SQL filter is `>= from AND <= to` on day strings,
// so the user's selected range fully covers from the start of `from`
// (00:00:00) through the end of `to` (23:59:59) without ever sending
// a time-of-day value over the wire.
export type UsageRange =
  | "last7_days"
  | "last30_days"
  | "last90_days"
  | "last120_days"
  | "last180_days"
  | "all_time"
  | { custom: { from: string; to: string } };

export function isCustomRange(
  r: UsageRange,
): r is { custom: { from: string; to: string } } {
  return typeof r === "object" && r !== null && "custom" in r;
}

export function customRange(from: string, to: string): UsageRange {
  return { custom: { from, to } };
}

export type UsageGroupBy = "by_provider" | "by_model";

export type UsageBucket = "daily";

export interface UsageTotals {
  turnCount: number;
  inputTokens: number;
  outputTokens: number;
  cacheReadTokens: number;
  cacheWriteTokens: number;
  totalCostUsd: number;
  costHasUnknowns: boolean;
  totalDurationMs: number;
  distinctSessions: number;
  distinctModels: number;
}

export interface UsageGroupRow {
  key: string;
  label: string;
  turnCount: number;
  inputTokens: number;
  outputTokens: number;
  cacheReadTokens: number;
  cacheWriteTokens: number;
  totalCostUsd: number;
  costHasUnknowns: boolean;
  totalDurationMs: number;
}

export interface UsageSummaryPayload {
  range: UsageRange;
  totals: UsageTotals;
  byProvider: UsageGroupRow[];
  groups: UsageGroupRow[];
  generatedAt: string;
  /**
   * Date the per-token rate table used for per-agent cost
   * allocation was last verified against anthropic.com/pricing.
   * Always equals the Rust `PRICING_TABLE_DATE` constant — carried
   * in the payload so the dashboard footer can render
   * "Pricing data verified <date>" without the frontend ever
   * holding a stale copy of the date.
   */
  pricingTableDate: string;
}

export interface UsageTimeseriesPoint {
  bucketStart: string;
  totals: UsageTotals;
}

export interface UsageSeries {
  key: string;
  label: string;
  points: UsageTimeseriesPoint[];
}

export interface UsageTimeseriesPayload {
  range: UsageRange;
  bucket: UsageBucket;
  points: UsageTimeseriesPoint[];
  series: UsageSeries[];
  generatedAt: string;
}

export interface TopSessionRow {
  sessionId: string;
  provider: string;
  providerLabel: string;
  model: string | null;
  projectId: string | null;
  turnCount: number;
  totalCostUsd: number;
  costHasUnknowns: boolean;
  lastActivityAt: string;
}

export function getUsageSummary(
  range: UsageRange,
  groupBy: UsageGroupBy = "by_provider",
): Promise<UsageSummaryPayload> {
  return invoke<UsageSummaryPayload>("get_usage_summary", { range, groupBy });
}

export function getUsageTimeseries(
  range: UsageRange,
  bucket: UsageBucket = "daily",
  splitBy?: UsageGroupBy,
): Promise<UsageTimeseriesPayload> {
  return invoke<UsageTimeseriesPayload>("get_usage_timeseries", {
    range,
    bucket,
    splitBy: splitBy ?? null,
  });
}

export function getTopSessions(
  range: UsageRange,
  limit: number = 10,
): Promise<TopSessionRow[]> {
  return invoke<TopSessionRow[]>("get_top_sessions", { range, limit });
}

// Per-agent dashboard breakdown. One row per agent role over the
// range: the synthetic "main" key for the parent agent plus one row
// per subagent type ("Explore", "general-purpose", …). Cost is
// allocated proportionally at insert time (see
// `insert_agent_rows` in `src-tauri/src/usage.rs`), so totals sum
// back to the turn-level cost without double-counting.
export interface UsageAgentGroupRow {
  key: string;
  label: string;
  turnCount: number;
  invocationCount: number;
  inputTokens: number;
  outputTokens: number;
  cacheReadTokens: number;
  cacheWriteTokens: number;
  totalCostUsd: number;
  costHasUnknowns: boolean;
}

export interface UsageAgentPayload {
  range: UsageRange;
  groups: UsageAgentGroupRow[];
  generatedAt: string;
}

export function getUsageByAgent(
  range: UsageRange,
): Promise<UsageAgentPayload> {
  return invoke<UsageAgentPayload>("get_usage_by_agent", { range });
}

// Two-row Main-vs-Subagents rollup. Shares the `UsageAgentPayload`
// shape with `getUsageByAgent` — `groups` is just always capped at
// two rows (`key = "main"` / `key = "subagent"`), missing when that
// bucket had no activity in the range.
export function getUsageByAgentRole(
  range: UsageRange,
): Promise<UsageAgentPayload> {
  return invoke<UsageAgentPayload>("get_usage_by_agent_role", { range });
}

// Last-seen snapshot of every rate-limit bucket the providers have
// reported, persisted in `usage.sqlite`. Called once on app boot so
// the chat-toolbar's 5h / Wk chips render last-known values
// immediately instead of staying blank until the user sends their
// first message — the Anthropic plan limits only arrive as a side-
// effect of inference responses, so without this rehydration the
// chips look broken on every relaunch. Live `rate_limit_updated`
// runtime events overwrite individual buckets via the existing
// reducer arm in app-store.
export function getRateLimitCache(): Promise<RateLimitInfo[]> {
  return invoke<RateLimitInfo[]>("get_rate_limit_cache");
}

// Read a single project file as a UTF-8 string. Rejects on:
//   * file outside the project root (canonicalisation escape)
//   * file above CODE_VIEW_MAX_FILE_BYTES (4 MiB)
//   * non-UTF-8 content
// Callers should `.catch` to render a friendly placeholder.
export function readProjectFile(path: string, file: string): Promise<string> {
  return invoke<string>("read_project_file", { path, file });
}

// Write UTF-8 `contents` to `path / file`. Rejects on:
//   * file outside the project root (parent-canonicalisation escape)
//   * I/O error from std::fs::write
// Used by the /code editor's save flow (Cmd+S, Vim :w, auto-save
// on focus-out). The frontend guards file size at 10 MiB before
// invoking — no size cap on the Rust side.
export function writeProjectFile(
  path: string,
  file: string,
  contents: string,
): Promise<void> {
  return invoke<void>("write_project_file", { path, file, contents });
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
  /** Fuzzy-match each line against the query using fff-search's
   *  Smith-Waterman scorer — typo-tolerant and inherently
   *  case-insensitive. Takes precedence over `useRegex`. Default
   *  false. */
  useFuzzy: boolean;
  /** Default true. The `aA` toggle in the UI flips this off. */
  caseSensitive: boolean;
  /** Glob patterns restricting which files are searched. */
  includes: string[];
  /** Glob patterns excluded from the search (plain globs —
   *  no leading `!` required, though it's tolerated). */
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
// child and returns a numeric id; the provided onEvent channel
// multiplexes raw byte output and the lifecycle exit notification
// in source order. All the other helpers take that id as the first
// arg.
export type PtyId = number;

/** Multiplexed channel payload for a live PTY session. Mirrors
 *  `pty::PtyEvent` on the Rust side (serde-tagged with `kind`). */
export type PtyEvent =
  | { kind: "data"; bytes: number[] }
  | { kind: "exit"; code: number | null };

export interface OpenPtyOptions {
  cols: number;
  rows: number;
  cwd?: string;
  shell?: string;
  /** Fires for every byte-chunk emitted by the shell. */
  onData: (bytes: number[]) => void;
  /** Fires once when the shell process exits (clean `exit`, signal,
   *  or external kill). The dock uses this to auto-close the tab. */
  onExit: (code: number | null) => void;
}

export function openPty(opts: OpenPtyOptions): Promise<PtyId> {
  const channel = new Channel<PtyEvent>();
  channel.onmessage = (event) => {
    if (event.kind === "data") {
      opts.onData(event.bytes);
    } else {
      opts.onExit(event.code);
    }
  };
  return invoke<PtyId>("pty_open", {
    cols: opts.cols,
    rows: opts.rows,
    cwd: opts.cwd ?? null,
    shell: opts.shell ?? null,
    onEvent: channel,
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

// Monotonic token allocator for `searchFileContents` cancellation.
// Each call gets a fresh token; pass it to `stopContentSearch` to
// cooperatively interrupt the in-flight grep. We mint tokens
// client-side (rather than asking Rust for one) so the caller can
// tear down a stale search the moment a new one starts, without an
// extra round-trip.
let nextSearchToken = 1;
export function nextContentSearchToken(): number {
  return nextSearchToken++;
}

// Live content search across the project. Backed by fff-search's
// indexed grep (literal / regex / fuzzy modes via `options`); the
// `cancelToken` is registered with a Rust-side `AtomicBool` flag so
// `stopContentSearch(token)` can interrupt a slow query. Returns
// one ContentBlock per disjoint match group with ±3 lines of
// context — designed for a Zed-style multibuffer renderer.
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
