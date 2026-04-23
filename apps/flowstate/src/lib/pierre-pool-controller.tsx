import * as React from "react";
import {
  useWorkerPool,
  WorkerPoolContextProvider,
  type WorkerInitializationRenderOptions,
  type WorkerPoolOptions,
} from "@pierre/diffs/react";
import {
  createPierreDiffsWorker,
  POOL_SIZE_MIN,
} from "./pierre-diffs-worker";

// ─────────────────────────────────────────────────────────────────
// PierrePoolProvider — memory-aware wrapper around @pierre/diffs'
// WorkerPoolContextProvider.
// ─────────────────────────────────────────────────────────────────
//
// The raw @pierre/diffs provider creates a fixed-size worker pool
// at mount time and never changes it. That's fine for small pools
// but means the default of 8 workers hangs ~160-400 MB of Shiki
// state in memory even when the user never opens a diff.
//
// This wrapper adds three behaviors:
//
//   1. Starts at poolSize=1 (see getDefaultPoolSize) — one worker
//      handles any single diff fine.
//
//   2. Scale-up: observes WorkerPoolManager stats and doubles the
//      pool (1 → 2 → 4 → ... up to the user-configured max) when
//      queued tasks persist past a short threshold. Scaling remounts
//      the underlying provider — grammar caches reset — but this
//      only happens under real load, where the user is already
//      waiting on diffs to render.
//
//   3. Idle-kill: when no diff/code view is mounted AND the pool
//      reports zero busy/queued/pending tasks for 30 s, unmounts
//      the provider entirely, terminating its worker threads and
//      freeing their heaps. The next mount of DiffPanel or CodeView
//      re-spawns workers via `useEnsurePierrePoolActive()`.
//
// Scale-up and idle-kill use the same `WorkerPoolManager.
// subscribeToStatChanges` stream. Nothing here polls.

interface PierrePoolProviderProps {
  children: React.ReactNode;
  highlighterOptions: WorkerInitializationRenderOptions;
  /** Upper bound on the auto-scaled pool size. Typically the user's
   *  Settings → Performance value, clamped into
   *  [POOL_SIZE_MIN, getMaxPoolSize()]. Must be ≥ POOL_SIZE_MIN. */
  maxPoolSize: number;
}

interface PierrePoolControllerValue {
  /** Increment the active-consumer refcount. Returns a cleanup
   *  that decrements it. Consumers call this from a mount effect
   *  via `useEnsurePierrePoolActive()`. */
  bumpMount: () => () => void;
}

const PierrePoolControllerContext =
  React.createContext<PierrePoolControllerValue | null>(null);

// Scale-up is considered when the queue has been non-trivial for
// this long. Shorter = more aggressive scaling on bursts. 200 ms
// lets a quick pair of consecutive diffs serialize through one
// worker without spawning a second, while sustained load still
// gets more parallelism.
const SCALE_UP_QUEUE_THRESHOLD = 2;
const SCALE_UP_DEBOUNCE_MS = 200;

// Idle-kill fires when nothing is happening AND no consumer is
// mounted. 30 s is long enough that navigating away-and-back
// doesn't trip it; short enough that closing the app's diff
// panels and walking away returns memory promptly.
const IDLE_KILL_MS = 30_000;

// Unmount grace: when the last consumer unmounts we wait this long
// before *starting* the idle-kill countdown, to avoid a mount/unmount
// ping (e.g. route transitions) tearing down the pool.
const UNMOUNT_GRACE_MS = 2_000;

export function PierrePoolProvider({
  children,
  highlighterOptions,
  maxPoolSize,
}: PierrePoolProviderProps): React.JSX.Element {
  const effectiveMaxPoolSize = Math.max(POOL_SIZE_MIN, maxPoolSize);
  // targetPoolSize drives a `key` on WorkerPoolContextProvider — when
  // it changes we intentionally remount so the manager picks up the
  // new size. Remount drops the in-worker grammar cache, so we only
  // bump this under real queue pressure.
  const [targetPoolSize, setTargetPoolSize] =
    React.useState<number>(POOL_SIZE_MIN);

  // Active=false fully unmounts the WorkerPoolContextProvider. Its
  // `destroy` path terminates the worker threads. Consumers calling
  // useWorkerPool() while inactive will see `undefined`; they should
  // call useEnsurePierrePoolActive() to flip this back on.
  const [active, setActive] = React.useState<boolean>(true);

  // Mount refcount — number of DiffPanel/CodeView (or any
  // useEnsurePierrePoolActive caller) instances currently mounted.
  const mountCountRef = React.useRef<number>(0);

  // Pending idle-kill timer. Cancelled whenever something resumes
  // activity (new mount, stats non-zero) before it fires.
  const idleKillTimerRef = React.useRef<number | null>(null);

  const cancelIdleKillTimer = React.useCallback(() => {
    if (idleKillTimerRef.current !== null) {
      window.clearTimeout(idleKillTimerRef.current);
      idleKillTimerRef.current = null;
    }
  }, []);

  const ensureActive = React.useCallback(() => {
    cancelIdleKillTimer();
    setActive(true);
  }, [cancelIdleKillTimer]);

  const bumpMount = React.useCallback((): (() => void) => {
    mountCountRef.current += 1;
    ensureActive();
    let released = false;
    return () => {
      if (released) return;
      released = true;
      mountCountRef.current = Math.max(0, mountCountRef.current - 1);
      // If other consumers are still mounted, nothing to do.
      // If none are, the idle-kill subtree (PoolIdleWatcher) will
      // notice via its own stat subscription + mountCountRef read,
      // so we don't need to trigger anything here.
    };
  }, [ensureActive]);

  const ctxValue = React.useMemo<PierrePoolControllerValue>(
    () => ({ bumpMount }),
    [bumpMount],
  );

  const poolOptions = React.useMemo<WorkerPoolOptions>(
    () => ({
      workerFactory: createPierreDiffsWorker,
      poolSize: targetPoolSize,
    }),
    [targetPoolSize],
  );

  const requestScaleUp = React.useCallback(
    (next: number) => {
      setTargetPoolSize((curr) => (next > curr ? next : curr));
    },
    [],
  );

  const requestKill = React.useCallback(() => {
    // Only kill if nobody's currently mounted. The watcher also
    // checks this, but guard here too in case races slip through.
    if (mountCountRef.current > 0) return;
    setActive(false);
    // Reset target poolSize so the next activation starts lean
    // again rather than remembering a previously-scaled-up value.
    setTargetPoolSize(POOL_SIZE_MIN);
  }, []);

  // When activating (active flips true), cancel any pending timer —
  // a reactivation during idle wait should fully cancel.
  React.useEffect(() => {
    if (active) cancelIdleKillTimer();
  }, [active, cancelIdleKillTimer]);

  // Cleanup on unmount of the provider itself.
  React.useEffect(() => cancelIdleKillTimer, [cancelIdleKillTimer]);

  return (
    <PierrePoolControllerContext.Provider value={ctxValue}>
      {active ? (
        <WorkerPoolContextProvider
          key={targetPoolSize}
          poolOptions={poolOptions}
          highlighterOptions={highlighterOptions}
        >
          <PoolStatsWatcher
            mountCountRef={mountCountRef}
            idleKillTimerRef={idleKillTimerRef}
            targetPoolSize={targetPoolSize}
            maxPoolSize={effectiveMaxPoolSize}
            onScaleUp={requestScaleUp}
            onKill={requestKill}
          />
          {children}
        </WorkerPoolContextProvider>
      ) : (
        // Render children without the provider. useWorkerPool() will
        // return undefined until a consumer calls bumpMount(), which
        // flips active back to true and remounts the provider.
        children
      )}
    </PierrePoolControllerContext.Provider>
  );
}

// ─────────────────────────────────────────────────────────────────
// PoolStatsWatcher — lives inside the WorkerPoolContextProvider
// when active, subscribes to stats, drives both scale-up and
// idle-kill. Renders nothing.
// ─────────────────────────────────────────────────────────────────

interface PoolStatsWatcherProps {
  mountCountRef: React.MutableRefObject<number>;
  idleKillTimerRef: React.MutableRefObject<number | null>;
  targetPoolSize: number;
  maxPoolSize: number;
  onScaleUp: (nextSize: number) => void;
  onKill: () => void;
}

function PoolStatsWatcher({
  mountCountRef,
  idleKillTimerRef,
  targetPoolSize,
  maxPoolSize,
  onScaleUp,
  onKill,
}: PoolStatsWatcherProps): null {
  const pool = useWorkerPool();

  // Time the queue first exceeded SCALE_UP_QUEUE_THRESHOLD. Null =
  // currently below threshold. Used by the debounce.
  const queueElevatedSinceRef = React.useRef<number | null>(null);

  // Last time the pool reported any activity (busy/queued/pending
  // all zero ⇒ "idle"). Used to schedule idle-kill once nobody's
  // mounted.
  const lastActiveAtRef = React.useRef<number>(Date.now());

  React.useEffect(() => {
    if (!pool) return;

    const scheduleKillIfAppropriate = () => {
      if (idleKillTimerRef.current !== null) return;
      const now = Date.now();
      const idleFor = now - lastActiveAtRef.current;
      const delay = Math.max(0, IDLE_KILL_MS - idleFor);
      idleKillTimerRef.current = window.setTimeout(() => {
        idleKillTimerRef.current = null;
        // Re-check preconditions — state may have changed during the wait.
        if (mountCountRef.current > 0) return;
        const stillIdleFor = Date.now() - lastActiveAtRef.current;
        if (stillIdleFor < IDLE_KILL_MS) return;
        onKill();
      }, delay);
    };

    const unsubscribe = pool.subscribeToStatChanges((stats) => {
      const now = Date.now();
      const isBusy =
        stats.busyWorkers > 0 ||
        stats.queuedTasks > 0 ||
        stats.pendingTasks > 0;

      if (isBusy) {
        lastActiveAtRef.current = now;
        // Cancel any scheduled kill since we're live again.
        if (idleKillTimerRef.current !== null) {
          window.clearTimeout(idleKillTimerRef.current);
          idleKillTimerRef.current = null;
        }
      }

      // ── Scale-up ────────────────────────────────────────────────
      if (stats.queuedTasks >= SCALE_UP_QUEUE_THRESHOLD) {
        if (queueElevatedSinceRef.current === null) {
          queueElevatedSinceRef.current = now;
        } else if (
          now - queueElevatedSinceRef.current >= SCALE_UP_DEBOUNCE_MS &&
          targetPoolSize < maxPoolSize
        ) {
          const next = Math.min(
            Math.max(targetPoolSize * 2, targetPoolSize + 1),
            maxPoolSize,
          );
          if (next > targetPoolSize) {
            onScaleUp(next);
            queueElevatedSinceRef.current = null;
          }
        }
      } else {
        queueElevatedSinceRef.current = null;
      }

      // ── Idle-kill ────────────────────────────────────────────────
      // Only arm the kill timer when there are no mounted consumers
      // AND the pool is idle. If a consumer is still mounted, we
      // keep workers alive even through long idles.
      if (!isBusy && mountCountRef.current === 0) {
        scheduleKillIfAppropriate();
      }
    });

    // Safety net: consumers may unmount without triggering a stats
    // event (stats only fire on pool-side state changes). Poll the
    // refcount on a low-frequency interval to catch the "last
    // consumer went away while the pool is quiet" case.
    const pollIntervalId = window.setInterval(() => {
      if (mountCountRef.current > 0) return;
      // Snapshot current stats — if idle, schedule the kill.
      const stats = pool.getStats();
      const isBusy =
        stats.busyWorkers > 0 ||
        stats.queuedTasks > 0 ||
        stats.pendingTasks > 0;
      if (!isBusy) scheduleKillIfAppropriate();
    }, UNMOUNT_GRACE_MS);

    return () => {
      unsubscribe();
      window.clearInterval(pollIntervalId);
    };
  }, [
    pool,
    targetPoolSize,
    maxPoolSize,
    onScaleUp,
    onKill,
    mountCountRef,
    idleKillTimerRef,
  ]);

  return null;
}

// ─────────────────────────────────────────────────────────────────
// useEnsurePierrePoolActive — consumer hook.
// ─────────────────────────────────────────────────────────────────
//
// DiffPanel and CodeView call this in a mount effect to (a) wake
// the pool if it was killed, and (b) participate in the mount
// refcount so idle-kill doesn't fire while they're on screen.
export function useEnsurePierrePoolActive(): void {
  const ctx = React.useContext(PierrePoolControllerContext);
  React.useEffect(() => {
    if (!ctx) return;
    return ctx.bumpMount();
  }, [ctx]);
}
