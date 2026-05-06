// Cross-window state synchronization for popout windows.
//
// Tauri spawns each popout (`thread-<sessionId>`) as its own
// WebviewWindow with an isolated React tree, sessionStorage, and
// connectStream subscription. The daemon broadcasts server-side events
// (turn output, permission_requested, session_model_updated, ...) to
// every connected window — that's how thread state stays in sync. But
// optimistic client-side dispatches don't cross window boundaries:
//
//   • When you click "Allow" in the popout, the popout dispatches
//     `consume_pending_permission` locally before the daemon round
//     trip — the main window still shows the prompt until the user
//     clicks again (or the daemon happens to push something that
//     drops the request from the queue).
//
//   • When you switch permission mode from the PlanExit dialog or
//     the cycle-mode shortcut, the local React state + sessionStorage
//     update fires only in the answering window. The other window's
//     toolbar badge stays on the old mode.
//
//   • Same story for the "consume_pending_question" optimistic dispatch.
//
// Fix: every window broadcasts a Tauri event after each optimistic
// dispatch; every other window listens, dedupes by source label, and
// applies the same dispatch. We tag the payload with the emitter's
// window label rather than relying on Tauri's `Event.windowLabel`
// because Tauri v2's JS Event type doesn't expose that field — the
// label travels in the payload, and the receiver compares it against
// `getCurrentWindow().label` to skip self-emissions.
//
// Daemon-synced state (selected model via `session_model_updated`,
// pending-permission INSERTs via `permission_requested`, session list,
// etc.) does not need this layer — the daemon already broadcasts those.

import { emit, listen, type UnlistenFn } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import type { PermissionMode } from "./types";

const PERMISSION_CONSUMED_EVENT = "flowstate:permission-consumed";
const QUESTION_CONSUMED_EVENT = "flowstate:question-consumed";
const PERMISSION_MODE_CHANGED_EVENT = "flowstate:permission-mode-changed";

interface ConsumedPayload {
  source: string;
  sessionId: string;
  requestId: string;
}

interface PermissionModePayload {
  source: string;
  sessionId: string;
  mode: PermissionMode;
}

let cachedLabel: string | null = null;
function ownLabel(): string {
  if (cachedLabel === null) {
    try {
      cachedLabel = getCurrentWindow().label;
    } catch {
      // `getCurrentWindow` throws outside the Tauri webview (e.g.
      // vitest with jsdom). Fall back to a synthetic label so the
      // emit/listen helpers no-op cleanly instead of crashing tests.
      cachedLabel = `non-tauri:${Math.random().toString(36).slice(2)}`;
    }
  }
  return cachedLabel;
}

function fireAndForget(event: string, payload: unknown): void {
  // Errors from `emit` are swallowed: a failed broadcast is a UX
  // regression (other windows stay out of sync), not a correctness
  // bug — the answering window's local dispatch already happened
  // and the daemon round-trip will eventually reconcile.
  emit(event, payload).catch((err) => {
    console.warn(`[cross-window-sync] emit ${event} failed:`, err);
  });
}

/** Tell every other window that this window just popped a pending
 *  permission off its FIFO queue. Receivers apply the same
 *  `consume_pending_permission` dispatch so their queue head matches. */
export function broadcastPermissionConsumed(
  sessionId: string,
  requestId: string,
): void {
  fireAndForget(PERMISSION_CONSUMED_EVENT, {
    source: ownLabel(),
    sessionId,
    requestId,
  });
}

/** Tell every other window that this window just consumed the pending
 *  user-question. */
export function broadcastQuestionConsumed(
  sessionId: string,
  requestId: string,
): void {
  fireAndForget(QUESTION_CONSUMED_EVENT, {
    source: ownLabel(),
    sessionId,
    requestId,
  });
}

/** Tell every other window that this window just changed the
 *  composer permission mode. Receivers update their sessionStorage
 *  and dispatch `set_session_permission_mode` so the toolbar badge,
 *  sidebar tint, and ChatView local React state all follow. */
export function broadcastPermissionModeChanged(
  sessionId: string,
  mode: PermissionMode,
): void {
  fireAndForget(PERMISSION_MODE_CHANGED_EVENT, {
    source: ownLabel(),
    sessionId,
    mode,
  });
}

async function listenScoped<T extends { source: string }>(
  event: string,
  handler: (payload: T) => void,
): Promise<UnlistenFn> {
  const own = ownLabel();
  return listen<T>(event, ({ payload }) => {
    // Tauri's `emit` round-trips through the IPC bus to every
    // webview INCLUDING the emitter, so the source-label check is
    // load-bearing — without it the answering window would re-apply
    // its own dispatch and feedback-loop.
    if (!payload || payload.source === own) return;
    handler(payload);
  });
}

export async function listenPermissionConsumed(
  handler: (sessionId: string, requestId: string) => void,
): Promise<UnlistenFn> {
  return listenScoped<ConsumedPayload>(
    PERMISSION_CONSUMED_EVENT,
    ({ sessionId, requestId }) => handler(sessionId, requestId),
  );
}

export async function listenQuestionConsumed(
  handler: (sessionId: string, requestId: string) => void,
): Promise<UnlistenFn> {
  return listenScoped<ConsumedPayload>(
    QUESTION_CONSUMED_EVENT,
    ({ sessionId, requestId }) => handler(sessionId, requestId),
  );
}

export async function listenPermissionModeChanged(
  handler: (sessionId: string, mode: PermissionMode) => void,
): Promise<UnlistenFn> {
  return listenScoped<PermissionModePayload>(
    PERMISSION_MODE_CHANGED_EVENT,
    ({ sessionId, mode }) => handler(sessionId, mode),
  );
}
