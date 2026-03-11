import { queryOptions, type QueryClient } from "@tanstack/react-query";
import { getGitBranch, getGitDiffSummary, sendMessage } from "./api";
import type { GitFileSummary } from "./api";
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

export function gitDiffSummaryQueryOptions(
  path: string | null,
  refreshTick: number,
) {
  return queryOptions({
    queryKey: ["git", "diff-summary", path, refreshTick] as const,
    queryFn: async (): Promise<GitFileSummary[]> => {
      if (!path) return [];
      return getGitDiffSummary(path);
    },
    enabled: !!path,
    // `refreshTick` is bumped whenever the chat view knows the diff
    // should be re-read (session load, turn_completed, panel open).
    // Bumping the tick produces a fresh queryKey, which is what
    // drives the refetch — so we can keep staleTime Infinity and
    // never refetch the same (path, tick) pair twice.
    staleTime: Infinity,
  });
}
