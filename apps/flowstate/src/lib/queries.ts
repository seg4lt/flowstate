import { queryOptions, type QueryClient } from "@tanstack/react-query";
import {
  getAttachment,
  getGitBranch,
  listDirectory,
  listGitBranches,
  listGitWorktrees,
  listProjectFiles,
  pathExists,
  resolveGitRoot,
  sendMessage,
} from "./api";
import type {
  DirEntry,
  GitBranchList,
  GitWorktree,
  ProjectFileListing,
} from "./api";
import type { SessionDetail } from "./types";

// Pagination page size for session loads. Requesting the most
// recent N turns on open keeps the round-trip small for long
// threads; older turns load on demand when the user scrolls up.
// Tuned to comfortably fill the viewport on a laptop display.
export const SESSION_PAGE_SIZE = 50;

export interface SessionPage {
  detail: SessionDetail;
  /** Number of turns in `detail.turns` relative to the full history.
   *  When `loadedTurns < totalTurns`, older turns can be fetched by
   *  calling `sendMessage({ type: 'load_session', ..., before_created_at })`
   *  once the pagination wire format lands. */
  loadedTurns: number;
  /** Total turn count as known by the daemon at load time. */
  totalTurns: number;
  /** True when the response did not include every turn in the
   *  session — i.e. the caller can request older turns. */
  hasMoreOlder: boolean;
}

async function fetchSessionPage(
  sessionId: string,
  limit: number | null = SESSION_PAGE_SIZE,
): Promise<SessionPage> {
  const res = await sendMessage({
    type: "load_session",
    session_id: sessionId,
    ...(limit !== null ? { limit } : {}),
  });
  if (!res || res.type !== "session_loaded") {
    throw new Error(
      res && res.type === "error"
        ? res.message
        : `load_session returned unexpected shape for ${sessionId}`,
    );
  }
  const detail = res.session;
  const totalTurns = detail.summary.turnCount ?? detail.turns.length;
  return {
    detail,
    loadedTurns: detail.turns.length,
    totalTurns,
    hasMoreOlder: detail.turns.length < totalTurns,
  };
}

/// Fetch *every* persisted turn for a session and write the result
/// into the tanstack query cache. Used by the "Load older turns"
/// affordance when a user wants to scroll back beyond the default
/// page size. The backend short-circuits on the second call —
/// `get_session_limited(None)` reads the whole history in one SQL
/// query, so this is a single round-trip regardless of page size.
export async function loadFullSession(
  client: QueryClient,
  sessionId: string,
): Promise<SessionPage> {
  const page = await fetchSessionPage(sessionId, null);
  client.setQueryData(sessionQueryKey(sessionId), page);
  return page;
}

// Query key for a single session. Centralised so prefetch,
// invalidate, and setQueryData calls all hit the same cache entry.
export function sessionQueryKey(sessionId: string) {
  return ["session", sessionId] as const;
}

export function sessionQueryOptions(sessionId: string) {
  return queryOptions({
    queryKey: sessionQueryKey(sessionId),
    queryFn: () => fetchSessionPage(sessionId),
    // Sessions never auto-expire from the cache while the app is
    // running — the daemon's broadcast stream is authoritative for
    // live updates, and `setQueryData` keeps this entry in sync
    // whenever a turn changes for the active view.
    staleTime: Infinity,
    gcTime: 30 * 60 * 1000,
  });
}

// Fire-and-forget hover prefetch. Safe to call repeatedly — tanstack
// query deduplicates in-flight fetches and no-ops on warm cache.
export function prefetchSession(client: QueryClient, sessionId: string) {
  void client.prefetchQuery(sessionQueryOptions(sessionId));
}

// Resolve the git repository root for `path` via
// `git rev-parse --show-toplevel`. Returns the repo root when the
// path is inside a submodule or linked worktree (where `.git` is
// a file, not a directory). Cached forever — the git root for a
// given filesystem path never changes during an app session.
export function gitRootQueryOptions(path: string | null) {
  return queryOptions({
    queryKey: ["git", "root", path] as const,
    queryFn: async () => {
      if (!path) return null;
      return resolveGitRoot(path);
    },
    enabled: !!path,
    staleTime: Infinity,
  });
}

export function gitBranchQueryOptions(path: string | null) {
  return queryOptions({
    queryKey: ["git", "branch", path] as const,
    queryFn: async () => {
      if (!path) return null;
      return getGitBranch(path);
    },
    enabled: !!path,
    // Branch doesn't flip often — an hour of cache is fine, and
    // switching between sessions in the same project skips the call
    // entirely. The query gets invalidated explicitly when we know
    // the branch may have moved (e.g. after a turn that ran `git
    // checkout`).
    staleTime: 60 * 60 * 1000,
  });
}

// All refs under the project — local heads + remote tracking branches,
// sorted by recent committer date, returned in one `for-each-ref`
// call. Only fetched when the branch-switcher popover opens (the
// consumer overrides `enabled`), so we don't pay the git subprocess
// cost on sessions where the user never clicks. Short staleTime +
// refetchOnMount because the agent can run `git branch` / `git
// checkout` at any moment and we want the next popover open to
// reflect that without stale reads.
export function gitBranchListQueryOptions(path: string | null) {
  return queryOptions({
    queryKey: ["git", "branch-list", path] as const,
    queryFn: async (): Promise<GitBranchList> => {
      if (!path) return { current: null, local: [], remote: [] };
      return listGitBranches(path);
    },
    enabled: !!path,
    staleTime: 5 * 1000,
    refetchOnMount: true,
  });
}

// Every worktree attached to the repo containing `path`, parsed from
// `git worktree list --porcelain`. Same short-TTL + refetchOnMount
// reasoning as the branch list — an agent running `git worktree add`
// between popover opens should show up on the next open.
export function gitWorktreeListQueryOptions(path: string | null) {
  return queryOptions({
    queryKey: ["git", "worktree-list", path] as const,
    queryFn: async (): Promise<GitWorktree[]> => {
      if (!path) return [];
      return listGitWorktrees(path);
    },
    enabled: !!path,
    staleTime: 5 * 1000,
    refetchOnMount: true,
  });
}

// Cheap existence probe for a worktree's folder. When a worktree
// thread's underlying directory has been removed (either from the
// terminal or from the branch-switcher's delete button) we flip the
// chat view into read-only mode. 10s staleTime keeps it responsive
// without thrashing the filesystem.
export function pathExistsQueryOptions(path: string | null) {
  return queryOptions({
    queryKey: ["fs", "path-exists", path] as const,
    queryFn: async (): Promise<boolean> => {
      if (!path) return true;
      return pathExists(path);
    },
    enabled: !!path,
    staleTime: 10 * 1000,
    refetchOnMount: true,
  });
}

/// Fetch the bytes of a persisted image attachment on demand. Cached
/// indefinitely (the file is immutable for the lifetime of the row),
/// dropped from memory five minutes after the last reference so a long
/// session doesn't slowly accumulate every image the user ever pasted.
export function attachmentQueryOptions(id: string | null) {
  return queryOptions({
    queryKey: ["attachment", id] as const,
    queryFn: () => {
      if (!id) throw new Error("attachmentQueryOptions called without an id");
      return getAttachment(id);
    },
    enabled: !!id,
    staleTime: Infinity,
    gcTime: 5 * 60 * 1000,
  });
}

// Throttle for the hover-driven file-list prefetch on the chat
// header's Search button. prefetchQuery treats anything fresher
// than this staleTime override as a no-op, so wiggling the mouse
// over the button only fires one walk per ~1.5 seconds per
// project — not one per mouse-enter event.
const PROJECT_FILES_PREFETCH_THROTTLE_MS = 1_500;

/// While fff-search's background scanner is still walking the
/// worktree, React Query re-polls on this interval so the picker
/// fills in live without the user touching anything. The first
/// response with `indexing: false` flips the polling off (see
/// `refetchInterval` below). 750 ms keeps the picker visibly alive
/// without hammering the IPC bridge.
const PROJECT_FILES_INDEXING_POLL_MS = 750;

/// Once indexing has settled, treat the cached file list as fresh
/// for 30 s. Short enough that newly-created files appear quickly
/// when the user re-focuses the picker; long enough that the
/// picker's open animation never blocks on a re-walk for typical
/// edit-and-flip-back flows. The `turn_completed` reindex hook
/// (see `useSessionStreamSubscription`) explicitly invalidates this
/// query, so agent-created files don't have to wait for staleness.
const PROJECT_FILES_STALE_MS = 30_000;

// Project file list for the /code editor view's picker + tree.
//
// Backed by fff-search's per-worktree mmap-mounted index — one cold
// scan per worktree, then live updates from the fs-watcher. While
// the cold scan is in progress the response carries `indexing: true`
// and React Query re-polls every `PROJECT_FILES_INDEXING_POLL_MS`
// until the scanner settles, so the picker visibly fills in on huge
// repos instead of looking empty.
//
// Cache freshness:
//   * `staleTime: PROJECT_FILES_STALE_MS` — quick re-fetches on
//     window focus / next mount when the user has been away briefly
//   * `refetchOnWindowFocus: true` — picks up files created while
//     the user was in another app
//   * Explicit `invalidateQueries` from the chat session's
//     `turn_completed` event — agent edits land in the picker
//     immediately on the next open
//
// Why this changed: the previous policy was `staleTime: Infinity`
// + no refetchOnMount, on the assumption that walking a tree was
// expensive. With fff-search the steady-state cost is a memcpy
// against the mmap'd index, so we can afford active freshness.
export function projectFilesQueryOptions(path: string | null) {
  return queryOptions({
    queryKey: ["code", "project-files", path] as const,
    queryFn: async (): Promise<ProjectFileListing> => {
      if (!path) return { files: [], indexing: false, scanned: 0 };
      return listProjectFiles(path);
    },
    enabled: !!path,
    staleTime: PROJECT_FILES_STALE_MS,
    refetchOnWindowFocus: true,
    refetchOnMount: false,
    // Auto-poll only while fff is still walking the cold scan; flip
    // off as soon as `indexing` reports `false`. React Query passes
    // the live Query object so we can read the latest data.
    refetchInterval: (query) => {
      const data = query.state.data as ProjectFileListing | undefined;
      return data?.indexing ? PROJECT_FILES_INDEXING_POLL_MS : false;
    },
    gcTime: 30 * 60 * 1000,
  });
}

// Fire-and-forget hover prefetch for the project file list. Wired
// to onMouseEnter / onFocus on the chat header's Search button so
// the walk starts before the click lands. The staleTime override
// gives natural throttling: rapid mouse movement over the button
// does not queue up redundant walks.
export function prefetchProjectFiles(
  client: QueryClient,
  path: string | null,
) {
  if (!path) return;
  void client.prefetchQuery({
    ...projectFilesQueryOptions(path),
    staleTime: PROJECT_FILES_PREFETCH_THROTTLE_MS,
  });
}

// Single-directory listing for the /code view's file tree. Unlike
// `projectFilesQueryOptions` (which does one monolithic gitignore
// walk), this fetches just one level on demand — every folder click
// hits this query with that folder's relative path. Results include
// ignored entries flagged via `DirEntry.isIgnored` so the tree can
// render them dimmed without walking their contents until the user
// explicitly expands them. Cached per `(projectPath, subPath)` pair.
//
// Same "never auto-refetch" contract as the file list — the user
// drives refreshes explicitly; we don't walk the filesystem on
// window focus or mount.
export function directoryQueryOptions(
  path: string | null,
  subPath: string,
) {
  return queryOptions({
    queryKey: ["code", "directory", path, subPath] as const,
    queryFn: async (): Promise<DirEntry[]> => {
      if (!path) return [];
      return listDirectory(path, subPath);
    },
    enabled: !!path,
    staleTime: Infinity,
    refetchOnMount: false,
    gcTime: 30 * 60 * 1000,
  });
}

