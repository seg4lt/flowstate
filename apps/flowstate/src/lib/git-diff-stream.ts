import * as React from "react";
import {
  watchGitDiffSummary,
  type DiffSummaryEvent,
  type GitFileSummary,
} from "./api";

// Re-export under the existing name so HeaderActions and DiffPanel
// keep their imports stable. Stats now come straight from
// `git diff --numstat` on the rust side, so there's nothing for
// the frontend to compute — we just rename the type.
export type AggregatedFileDiff = GitFileSummary;

export type DiffStreamStatus = "idle" | "streaming" | "done" | "error";

export interface StreamedDiffSummary {
  diffs: GitFileSummary[];
  status: DiffStreamStatus;
  error: string | null;
}

const INITIAL_STATE: StreamedDiffSummary = {
  diffs: [],
  status: "idle",
  error: null,
};

// Streamed replacement for `useQuery(gitDiffSummaryQueryOptions(...))`.
//
// Three things the query version couldn't do and we need:
//
//   1. **Phase-1 + Phase-2 split.** The backend now ships a fast
//      `files` event (from `git status`) immediately, then streams
//      per-file `numstat` events. Old query shape was one-shot.
//
//   2. **Keep-previous-data across restarts.** Bumping `refreshTick`
//      used to blow the query cache and flash the Diff button badge
//      empty for several seconds on a slow repo. We leave the last
//      committed `diffs` in state until the new subscription's
//      Phase 1 lands, then swap atomically — badge never flickers.
//
//   3. **Cancellation.** Cleanup calls `sub.stop()` which kills the
//      git subprocess on the rust side. Closing the panel or
//      navigating no longer leaks a multi-second git run.
//
// The hook also batches `numstat` events via requestAnimationFrame
// so a monorepo with thousands of changed files doesn't force one
// React render per event.
export function useStreamedGitDiffSummary(
  path: string | null,
  refreshTick: number,
  enabled: boolean,
): StreamedDiffSummary {
  const [state, setState] = React.useState<StreamedDiffSummary>(INITIAL_STATE);
  // Remember the path the currently-committed `state.diffs` belong
  // to. Same-path restarts (refresh-tick bumps from turn_completed,
  // branch checkout, panel hover) keep the previous list visible
  // until Phase 1 of the new subscription lands — that's what kills
  // the Diff-button badge flicker. Cross-path switches (thread A ->
  // thread B on different projects, or worktree swaps) MUST wipe
  // the slate: thread A's numbers are meaningless for thread B and
  // would otherwise linger on the action bar until the new stream's
  // first `files` event arrives.
  const lastPathRef = React.useRef<string | null>(null);

  React.useEffect(() => {
    // Cross-path reset BEFORE starting (or skipping) the subscription.
    // Running this inside the subscription effect — rather than a
    // separate effect — guarantees ordering: the state clear lands
    // in the same render as the subscription restart, so there is
    // never a frame where the old diffs are visible alongside the
    // new-path `streaming` status.
    if (lastPathRef.current !== path) {
      lastPathRef.current = path;
      setState(INITIAL_STATE);
    }
    if (!enabled || !path) return;

    // Working map is mutated in place by incoming events. React
    // only sees new snapshots via setState; this avoids cloning a
    // potentially huge Map on every one of N numstat events.
    let working = new Map<string, GitFileSummary>();
    let phaseOneLanded = false;
    let cancelled = false;
    let rafId: number | null = null;
    let pendingFlush = false;

    const flush = () => {
      rafId = null;
      pendingFlush = false;
      if (cancelled || !phaseOneLanded) return;
      const diffs = Array.from(working.values());
      setState((prev) => ({
        diffs,
        status: prev.status === "done" || prev.status === "error"
          ? prev.status
          : "streaming",
        error: prev.error,
      }));
    };

    const scheduleFlush = () => {
      if (pendingFlush) return;
      pendingFlush = true;
      rafId = requestAnimationFrame(flush);
    };

    // Flip status to "streaming" immediately so the UI can render a
    // scanning indicator. Crucially we do NOT reset `diffs` here —
    // keeping the previous list visible until Phase 1 lands is what
    // kills the Diff-button badge flicker across refresh-tick bumps.
    setState((prev) => ({ ...prev, status: "streaming", error: null }));

    const sub = watchGitDiffSummary(path, (event: DiffSummaryEvent) => {
      if (cancelled) return;
      switch (event.kind) {
        case "files": {
          working = new Map(event.files.map((f) => [f.path, f]));
          phaseOneLanded = true;
          // Atomic swap: previous committed diffs are replaced in a
          // single render with the new file list.
          setState({
            diffs: Array.from(working.values()),
            status: "streaming",
            error: null,
          });
          break;
        }
        case "numstat": {
          // Guard against out-of-order events — numstat before files
          // shouldn't happen but we tolerate it by ignoring the
          // record until Phase 1 has seeded the map.
          if (!phaseOneLanded) return;
          const existing = working.get(event.path);
          if (existing) {
            working.set(event.path, {
              ...existing,
              additions: event.additions,
              deletions: event.deletions,
            });
          } else {
            // Unknown path: a file git diff reports that git status
            // didn't. Seed it with "modified" so the UI renders it.
            working.set(event.path, {
              path: event.path,
              status: "modified",
              additions: event.additions,
              deletions: event.deletions,
            });
          }
          scheduleFlush();
          break;
        }
        case "done": {
          // Flush any in-flight numstat updates before flipping to
          // terminal status so the final frame matches the rust-side
          // view of the world.
          const diffs = Array.from(working.values());
          setState({
            diffs,
            status: event.ok ? "done" : "error",
            error: event.error,
          });
          break;
        }
      }
    });

    return () => {
      cancelled = true;
      if (rafId !== null) cancelAnimationFrame(rafId);
      sub.stop();
    };
  }, [path, refreshTick, enabled]);

  return state;
}
