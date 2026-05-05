import {
  afterEach,
  beforeEach,
  describe,
  expect,
  it,
  vi,
  type MockInstance,
} from "vitest";
import { act, renderHook, waitFor } from "@testing-library/react";

import {
  STEER_WATCHDOG_MS,
  useQueueDrain,
  type QueuedMessage,
} from "./use-queue-drain";
import type { SessionStatus } from "@/lib/types";

// `URL.revokeObjectURL` is a no-op in jsdom but is called on every
// successful drain — stub it so the tests don't blow up on missing
// implementations and so we can assert it WASN'T called when a drain
// fails (the chip thumbnail must stay visible for retry).
let revokeSpy: MockInstance;
beforeEach(() => {
  revokeSpy = vi.spyOn(URL, "revokeObjectURL").mockImplementation(() => {});
});
afterEach(() => {
  revokeSpy.mockRestore();
});

function makeMessage(id: string, text: string): QueuedMessage {
  return { id, text, images: [] };
}

describe("useQueueDrain", () => {
  it("drains the head of the queue on running → ready", async () => {
    const onSend = vi.fn(async () => {});
    const { result, rerender } = renderHook(
      ({ status }: { status: SessionStatus }) =>
        useQueueDrain({ sessionStatus: status, onSend }),
      { initialProps: { status: "running" as SessionStatus } },
    );

    // Queue a message while the turn is running. The drain must NOT
    // fire — sessionStatus is still "running".
    act(() => {
      result.current.enqueue("hello", []);
    });
    expect(result.current.queued).toHaveLength(1);
    expect(onSend).not.toHaveBeenCalled();

    // Turn finishes — the running → ready edge should drive the drain.
    rerender({ status: "ready" });

    await waitFor(() => {
      expect(onSend).toHaveBeenCalledTimes(1);
    });
    expect(onSend).toHaveBeenCalledWith("hello", []);
    await waitFor(() => {
      expect(result.current.queued).toHaveLength(0);
    });
  });

  it("keeps the chip in the queue when onSend rejects (regression for the silent-drop bug)", async () => {
    // Repro of the user-reported symptom: queue chip showed up when
    // the turn finished, but the message never appeared in chat
    // and no error was surfaced. The pre-fix drain effect popped the
    // head BEFORE awaiting `onSend`, so a daemon error
    // (ServerMessage::Error) silently dropped the message.
    const onSend = vi.fn(async () => {
      throw new Error("daemon rejected: session being torn down");
    });
    const onSendError = vi.fn();

    const { result, rerender } = renderHook(
      ({ status }: { status: SessionStatus }) =>
        useQueueDrain({ sessionStatus: status, onSend, onSendError }),
      { initialProps: { status: "running" as SessionStatus } },
    );

    act(() => {
      result.current.enqueue("hello", []);
    });
    rerender({ status: "ready" });

    await waitFor(() => {
      expect(onSend).toHaveBeenCalledTimes(1);
    });
    // The error callback fired with the daemon's reason. The drain
    // effect surfaced the failure to the parent for toasting.
    await waitFor(() => {
      expect(onSendError).toHaveBeenCalledTimes(1);
    });
    expect((onSendError.mock.calls[0]![0] as Error).message).toContain(
      "daemon rejected",
    );

    // Critical: the head is STILL in the queue. The chip stays
    // visible so the user can retry. Pre-fix this was [] (silent drop).
    expect(result.current.queued).toHaveLength(1);
    expect(result.current.queued[0]!.text).toBe("hello");
    // Image URLs survive too — would matter if there were any
    // attachments. None here, but also none revoked.
    expect(revokeSpy).not.toHaveBeenCalled();
  });

  it("does not fire onSend twice when the effect re-runs mid-await (re-entry guard)", async () => {
    // The drain effect's deps include `onSend` (parents typically
    // don't memoise it), so the effect re-fires on every parent
    // re-render. While the first drain is awaiting daemon ack, a
    // re-render with a new onSend identity must NOT trigger a
    // second send_turn for the same head.
    let resolveSend: (() => void) | null = null;
    const sendPromise = new Promise<void>((r) => {
      resolveSend = r;
    });
    const onSend = vi.fn(
      // Explicit arg-typed signature so the wrapper below can pass
      // `(input, images)` through without the vi.fn() inference
      // collapsing to a no-arg function.
      async (_input: string, _images: import("@/lib/types").AttachedImage[]) => {
        await sendPromise;
      },
    );

    const { result, rerender } = renderHook(
      ({ status, sendKey }: { status: SessionStatus; sendKey: number }) => {
        // New `onSend` identity every render even though the body
        // is the same — mirrors the parent that rebuilds
        // `handleSend` on every render.
        const wrapped = React.useCallback<
          (
            input: string,
            images: import("@/lib/types").AttachedImage[],
          ) => Promise<void>
        >(
          (input, images) => {
            void sendKey; // capture so the callback's identity changes
            return onSend(input, images);
          },
          [sendKey],
        );
        return useQueueDrain({ sessionStatus: status, onSend: wrapped });
      },
      { initialProps: { status: "running" as SessionStatus, sendKey: 0 } },
    );

    act(() => {
      result.current.enqueue("hello", []);
    });
    rerender({ status: "ready", sendKey: 0 });

    await waitFor(() => {
      expect(onSend).toHaveBeenCalledTimes(1);
    });

    // Force a re-render with a new onSend identity while the first
    // send is still pending. The drain MUST NOT fire again.
    rerender({ status: "ready", sendKey: 1 });
    rerender({ status: "ready", sendKey: 2 });
    rerender({ status: "ready", sendKey: 3 });
    expect(onSend).toHaveBeenCalledTimes(1);

    // Resolve the first send. The pop should commit and the queue
    // empties.
    await act(async () => {
      resolveSend!();
      await sendPromise;
    });
    await waitFor(() => {
      expect(result.current.queued).toHaveLength(0);
    });
  });

  it("suppresses the steer's interrupted transition then drains the rest on ready", async () => {
    const onSend = vi.fn(async () => {});
    const { result, rerender } = renderHook(
      ({ status }: { status: SessionStatus }) =>
        useQueueDrain({ sessionStatus: status, onSend }),
      { initialProps: { status: "running" as SessionStatus } },
    );

    // Queue [a, b]. User clicks "Send now" on a separate (already-
    // plucked) chip — caller signals via markSteerInFlight().
    act(() => {
      result.current.enqueue("a", []);
      result.current.enqueue("b", []);
      result.current.markSteerInFlight();
    });
    expect(result.current.queued).toHaveLength(2);

    // Daemon's steer flow: running → interrupted (consumed by the
    // steer flag, no drain).
    rerender({ status: "interrupted" });
    expect(onSend).not.toHaveBeenCalled();
    expect(result.current.queued).toHaveLength(2);

    // running again for the steered turn.
    rerender({ status: "running" });
    expect(onSend).not.toHaveBeenCalled();

    // running → ready when the steered turn finishes. THIS is the
    // transition that should drain the head.
    rerender({ status: "ready" });
    await waitFor(() => {
      expect(onSend).toHaveBeenCalledTimes(1);
    });
    expect(onSend).toHaveBeenCalledWith("a", []);
  });

  it("self-clears the steer flag if no transition arrives within the watchdog window (regression for stuck-flag bug)", async () => {
    // Pre-fix bug: the steer flag was a plain boolean cleared only
    // on the next non-running transition. If the steer's
    // `sendMessage` rejected async (or the original turn was
    // already finishing when the user clicked Send Now, so the
    // daemon skipped the interrupt), the expected
    // running→interrupted transition never came — and the next
    // legitimate drain was silently swallowed because the flag was
    // still true. Symptom: queue chips visible, no message ever
    // sent on subsequent turn completions.
    vi.useFakeTimers();
    try {
      const onSend = vi.fn(async () => {});
      const { result, rerender } = renderHook(
        ({ status }: { status: SessionStatus }) =>
          useQueueDrain({ sessionStatus: status, onSend }),
        { initialProps: { status: "running" as SessionStatus } },
      );

      act(() => {
        result.current.enqueue("a", []);
        result.current.markSteerInFlight();
      });

      // No transition arrives. Advance past the watchdog.
      act(() => {
        vi.advanceTimersByTime(STEER_WATCHDOG_MS + 100);
      });

      // Now a legitimate running → ready transition arrives. The
      // drain MUST fire — the watchdog already cleared the steer
      // flag.
      vi.useRealTimers();
      rerender({ status: "ready" });
      await waitFor(() => {
        expect(onSend).toHaveBeenCalledTimes(1);
      });
    } finally {
      vi.useRealTimers();
    }
  });

  it("fires onQueueChange on every queue mutation so the parent can persist", () => {
    const onQueueChange = vi.fn();
    const { result } = renderHook(() =>
      useQueueDrain({
        sessionStatus: "running",
        onSend: vi.fn(),
        onQueueChange,
      }),
    );

    act(() => {
      result.current.enqueue("a", []);
    });
    expect(onQueueChange).toHaveBeenLastCalledWith([
      expect.objectContaining({ text: "a" }),
    ]);

    act(() => {
      result.current.enqueue("b", []);
    });
    expect(onQueueChange).toHaveBeenLastCalledWith([
      expect.objectContaining({ text: "a" }),
      expect.objectContaining({ text: "b" }),
    ]);

    const firstId = result.current.queued[0]!.id;
    act(() => {
      result.current.removeQueued(firstId);
    });
    expect(onQueueChange).toHaveBeenLastCalledWith([
      expect.objectContaining({ text: "b" }),
    ]);
  });

  it("drains the seed queue on first render when mounting after a turn finished off-screen", async () => {
    // User queues a message, switches threads, the turn finishes
    // while they're away, they switch back. ChatInput remounts
    // with an `initialQueue` recovered from sessionQueues. The
    // hook's `prevStatusRef` seed pretends the previous status
    // was "running" so the drain effect's first run picks up the
    // ready-with-non-empty-queue case.
    const onSend = vi.fn(async () => {});
    const { result } = renderHook(() =>
      useQueueDrain({
        sessionStatus: "ready",
        initialQueue: [makeMessage("seed", "left over")],
        onSend,
      }),
    );

    await waitFor(() => {
      expect(onSend).toHaveBeenCalledWith("left over", []);
    });
    await waitFor(() => {
      expect(result.current.queued).toHaveLength(0);
    });
  });
});

// React must be imported as a value for the inline useCallback in
// the re-entry test. Importing at the bottom keeps the top of file
// readable and matches how vitest tolerates late imports.
import * as React from "react";
