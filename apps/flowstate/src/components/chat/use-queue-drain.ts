import * as React from "react";
import type { AttachedImage, SessionStatus } from "@/lib/types";

/** A single queued (not-yet-dispatched) message. Pasted images
 *  travel with the text so a chip can show its thumbnails and the
 *  drain can hand the encoded bytes to the daemon when the message
 *  actually leaves. Mirrors the live `QueuedMessage` shape in
 *  `chat-input.tsx`; kept duplicated rather than imported because
 *  this file is also imported by tests that don't want to pull
 *  the full ChatInput tree. */
export interface QueuedMessage {
  id: string;
  text: string;
  images: AttachedImage[];
}

/** How long to keep `steerInFlightRef` armed before assuming the
 *  expected running→interrupted/ready transition will never arrive
 *  and self-clearing the flag. The daemon's own
 *  interrupt→finalize wait is bounded at 10 s, so a real steer that
 *  the daemon accepted will always fire its transition well before
 *  this. The only callers that need the watchdog are the ones where
 *  the daemon never acted (transport error, daemon killed mid-RPC,
 *  original turn finishing at the same moment as the steer click). */
export const STEER_WATCHDOG_MS = 10_000;

export interface UseQueueDrainOptions {
  /** Seed the queue when the component mounts (e.g. restore from a
   *  module-level persistence map after a thread switch). Only read
   *  on the first render. */
  initialQueue?: QueuedMessage[];
  /** Live session status — the drain trigger watches for the
   *  `running → ready | interrupted` transition. */
  sessionStatus: SessionStatus | undefined;
  /** Dispatch a turn. MUST throw / reject on daemon error; the
   *  drain pops the head only on a successful resolve. Returning
   *  void (sync) is allowed for non-failing callers — the await
   *  collapses to a no-op. */
  onSend: (input: string, images: AttachedImage[]) => Promise<void> | void;
  /** Persist the queue outside this hook's lifecycle (so it
   *  survives ChatInput remounts on thread switch). Called on
   *  every queue mutation with the resolved post-mutation array. */
  onQueueChange?: (queue: QueuedMessage[]) => void;
  /** Optional hook fired right before the drain pops the head, so
   *  the parent can clear other per-item state (e.g. the inline
   *  edit textarea if the head was being edited). Runs
   *  synchronously inside the effect. */
  onDrainStart?: (item: QueuedMessage) => void;
  /** Optional hook fired when the drain's awaited `onSend` rejects.
   *  Receives the daemon's error. The head is NOT popped — the
   *  parent is expected to surface a toast or similar. */
  onSendError?: (err: unknown) => void;
}

export interface QueueDrainHandle {
  /** Current queue (React state). */
  queued: QueuedMessage[];
  /** State setter. Mirrors `React.Dispatch<SetStateAction>` so all
   *  the usual `setQueued((q) => ...)` patterns work. Calls
   *  `onQueueChange` with the resolved new array on every commit. */
  setQueued: React.Dispatch<React.SetStateAction<QueuedMessage[]>>;
  /** Append a new message to the tail of the queue. */
  enqueue: (text: string, images: AttachedImage[]) => void;
  /** Remove a queued message by id. Revokes any image preview URLs
   *  that were attached to it. */
  removeQueued: (id: string) => void;
  /** Mark a steer as in flight. The next non-running transition
   *  will be consumed by the steer (no drain) instead of triggering
   *  a regular send. The flag self-clears on watchdog timeout
   *  (`STEER_WATCHDOG_MS`) so a never-arriving transition can't
   *  permanently wedge the queue. */
  markSteerInFlight: () => void;
  /** Force-clear the steer flag and watchdog. Use from the
   *  `onSteer` async-error path to release the flag promptly. */
  clearSteerInFlight: () => void;
}

/** Per-session message queue with auto-drain on turn completion.
 *
 *  Watches for the `running → ready | interrupted` session-status
 *  transition. On that edge it pops the head of the queue, awaits
 *  `onSend` for daemon acknowledgement, and only commits the pop
 *  on a successful resolve. A daemon error keeps the head visible
 *  so the user can see the chip is still pending — the previous
 *  fire-and-forget design silently lost the message in that case
 *  (chip vanished, no toast, no new turn).
 *
 *  Steer (`onSteer`) is a separate code path because the daemon
 *  emits an extra running→interrupted transition that would
 *  otherwise trip this drain. Callers fire `markSteerInFlight()`
 *  before invoking `onSteer`; the drain effect consumes the next
 *  non-running transition without firing onSend, then resumes
 *  normal draining on the steered turn's natural completion.
 *  A 10 s watchdog self-clears the flag if the expected transition
 *  never arrives.
 */
export function useQueueDrain(opts: UseQueueDrainOptions): QueueDrainHandle {
  const {
    initialQueue,
    sessionStatus,
    onSend,
    onQueueChange,
    onDrainStart,
    onSendError,
  } = opts;

  // `onQueueChange` and `onSendError` / `onDrainStart` are read via
  // refs so a parent that didn't memoise them doesn't churn the
  // setQueued callback identity, and callers don't need to wrap them
  // in useCallback.
  const onQueueChangeRef = React.useRef(onQueueChange);
  onQueueChangeRef.current = onQueueChange;
  const onDrainStartRef = React.useRef(onDrainStart);
  onDrainStartRef.current = onDrainStart;
  const onSendErrorRef = React.useRef(onSendError);
  onSendErrorRef.current = onSendError;

  const [queued, setQueuedRaw] = React.useState<QueuedMessage[]>(
    initialQueue ?? [],
  );
  const setQueued = React.useCallback<
    React.Dispatch<React.SetStateAction<QueuedMessage[]>>
  >((next) => {
    setQueuedRaw((prev) => {
      const resolved =
        typeof next === "function"
          ? (next as (p: QueuedMessage[]) => QueuedMessage[])(prev)
          : next;
      onQueueChangeRef.current?.(resolved);
      return resolved;
    });
  }, []);

  // `prevStatusRef`'s seed handles the come-back-after-finish case:
  // user switched threads while a turn was running, the turn finished
  // off-screen, then they switched back. On mount sessionStatus is
  // already non-running but we want the drain effect's first run to
  // still trigger, so we lie about the prev being "running".
  const prevStatusRef = React.useRef<SessionStatus | undefined>(
    initialQueue && initialQueue.length > 0 &&
      (sessionStatus === "ready" || sessionStatus === "interrupted")
      ? "running"
      : sessionStatus,
  );
  const steerInFlightRef = React.useRef(false);
  const steerWatchdogRef = React.useRef<ReturnType<typeof setTimeout> | null>(
    null,
  );
  const drainInFlightRef = React.useRef(false);

  React.useEffect(() => {
    return () => {
      if (steerWatchdogRef.current !== null) {
        clearTimeout(steerWatchdogRef.current);
        steerWatchdogRef.current = null;
      }
    };
  }, []);

  React.useEffect(() => {
    const wasRunning = prevStatusRef.current === "running";
    const nowReady =
      sessionStatus === "ready" || sessionStatus === "interrupted";
    prevStatusRef.current = sessionStatus;
    if (!wasRunning || !nowReady) return;

    if (steerInFlightRef.current) {
      steerInFlightRef.current = false;
      if (steerWatchdogRef.current !== null) {
        clearTimeout(steerWatchdogRef.current);
        steerWatchdogRef.current = null;
      }
      return;
    }

    // A previous drain hasn't resolved yet (we awaited `onSend` and
    // the daemon hasn't acked). Don't fire a second `send_turn` for
    // the same head — wait for the await to settle and the natural
    // re-render to re-enter the effect with the popped queue.
    if (drainInFlightRef.current) return;

    if (queued.length === 0) return;
    const [first, ...rest] = queued;
    onDrainStartRef.current?.(first);

    // Optimistically pop the head before the daemon acks — chip
    // disappears as soon as the message is dispatched, not after
    // the round-trip completes. On failure, onSendError is called;
    // the chip is already gone.
    drainInFlightRef.current = true;
    setQueued(rest);
    for (const img of first.images) {
      URL.revokeObjectURL(img.previewUrl);
    }
    void (async () => {
      try {
        await onSend(first.text, first.images);
      } catch (err) {
        onSendErrorRef.current?.(err);
      } finally {
        drainInFlightRef.current = false;
      }
    })();
  }, [sessionStatus, queued, onSend, setQueued]);

  const enqueue = React.useCallback(
    (text: string, images: AttachedImage[]) => {
      setQueued((q) => [...q, { id: newQueueId(), text, images }]);
    },
    [setQueued],
  );

  const removeQueued = React.useCallback(
    (id: string) => {
      setQueued((q) => {
        const target = q.find((item) => item.id === id);
        if (target) {
          for (const img of target.images) {
            URL.revokeObjectURL(img.previewUrl);
          }
        }
        return q.filter((item) => item.id !== id);
      });
    },
    [setQueued],
  );

  const clearSteerInFlight = React.useCallback(() => {
    steerInFlightRef.current = false;
    if (steerWatchdogRef.current !== null) {
      clearTimeout(steerWatchdogRef.current);
      steerWatchdogRef.current = null;
    }
  }, []);

  const markSteerInFlight = React.useCallback(() => {
    steerInFlightRef.current = true;
    if (steerWatchdogRef.current !== null) {
      clearTimeout(steerWatchdogRef.current);
    }
    steerWatchdogRef.current = setTimeout(() => {
      steerInFlightRef.current = false;
      steerWatchdogRef.current = null;
    }, STEER_WATCHDOG_MS);
  }, []);

  return {
    queued,
    setQueued,
    enqueue,
    removeQueued,
    markSteerInFlight,
    clearSteerInFlight,
  };
}

let queueIdCounter = 0;
function newQueueId(): string {
  queueIdCounter += 1;
  return `q-${Date.now()}-${queueIdCounter}`;
}
