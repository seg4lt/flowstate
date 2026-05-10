import type { ClientMessage, ProviderKind, ServerMessage } from "./types";
import { readDefaultModel } from "./defaults-settings";

// ─── eager-create new-thread helper ──────────────────────────────
//
// Single source of truth for "spawn a session and route the user
// straight into it". Used by every entry point that produces a new
// thread:
//
//   * ⌘N (`useGlobalShortcuts.startThreadOnCurrentProject`)
//   * ⌘⇧N project picker (`components/project/project-picker.tsx`)
//   * Sidebar pencil + worktree dropdown
//     (`components/sidebar/worktree-new-thread-dropdown.tsx`)
//   * General (folder-less) sidebar pencil (`components/app-sidebar.tsx`)
//   * Project home "new thread" buttons
//     (`components/project/project-home-view.tsx`)
//
// Why eager (immediate `start_session`) instead of the previous lazy
// `/chat/draft/$projectId` → first-send-creates-it pattern: the lazy
// path raced the daemon's first stream of `permission_requested` /
// `tool_call` / `content_delta` events against the user's eventual
// navigation to `/chat/$sessionId`. Events arrived before any
// `useQuery` cache existed for the new sessionId and were silently
// dropped (`useSessionStreamSubscription.ts:96-112` short-circuits on
// `prev === undefined`), so the user only saw the final turn state
// once everything completed; permission prompts stayed invisible
// until they re-clicked the thread to remount ChatView.
//
// Eager-create avoids the race entirely: `session_created` populates
// `state.sessions` synchronously (via the caller's `send` wrapper that
// dispatches the response into the reducer), the sidebar row appears
// before the user types, and ChatView mounts with a populated cache
// the moment the user sends their first message.
//
// `projectId` is `undefined` for the folder-less "General" thread —
// `start_session` accepts an optional `project_id` and the daemon
// produces a project-less session for that bucket.

/** Same signature `useApp().send` exposes — the wrapper that calls
 *  `sendMessage` AND dispatches the response into the reducer so
 *  `session_created` lands in `state.sessions` before this function
 *  resolves. Raw `sendMessage` would leave the sidebar row missing
 *  for one frame until the daemon's `session_started` broadcast
 *  catches up. */
type SendFn = (message: ClientMessage) => Promise<ServerMessage | null>;

export interface StartThreadArgs {
  /** Project the new thread runs under, or `undefined` for the
   *  folder-less "General" bucket. Worktree-bound callers must pass
   *  the worktree's own projectId (NOT the parent), so the new
   *  session's cwd lands on the worktree folder. */
  projectId: string | undefined;
  /** Resolved by `useDefaultProvider()`. */
  defaultProvider: ProviderKind;
  /** `false` until the SQLite read of `defaults.provider` completes;
   *  callers must gate the click on this so a fast user doesn't
   *  silently fall back to a non-preferred provider. */
  defaultProviderLoaded: boolean;
  /** `useApp().send` — the wrapper that dispatches the response into
   *  the reducer. See `SendFn` doc. */
  send: SendFn;
  /** Caller-provided navigation hook. The helper deliberately doesn't
   *  import `useNavigate` so it stays callable from any code path
   *  (toolbar action, dialog `onCreated` callback, …) — caller closes
   *  over the router-typed `navigate` and forwards the new sessionId.
   *  Only invoked on success. */
  navigate: (sessionId: string) => void;
  /** UI-feedback hook (toast in production, no-op in tests). Receives
   *  user-facing failure text — never throw out of this helper, the
   *  caller can't usefully react to a thrown error here. */
  notify: (message: string) => void;
}

/**
 * Spawn a session on `projectId` with the user's default provider /
 * model and navigate the caller's window into it. Returns the new
 * sessionId on success, or `null` if the request was guarded out
 * (provider still loading) or failed (daemon error / unexpected
 * response). Failures surface via `notify`.
 */
export async function startThreadOnProject(
  args: StartThreadArgs,
): Promise<string | null> {
  const {
    projectId,
    defaultProvider,
    defaultProviderLoaded,
    send,
    navigate,
    notify,
  } = args;

  if (!defaultProviderLoaded) {
    // Mirrors the guard in the previous inline ⌘N implementation.
    // Falling through to the constant DEFAULT_PROVIDER would silently
    // disregard a saved choice that just hadn't loaded yet.
    notify("Default provider still loading… try again in a moment");
    return null;
  }

  try {
    const model = await readDefaultModel(defaultProvider);
    const res = await send({
      type: "start_session",
      provider: defaultProvider,
      model: model ?? undefined,
      project_id: projectId,
    });
    if (res?.type === "session_created") {
      const newSessionId = res.session.sessionId;
      navigate(newSessionId);
      return newSessionId;
    }
    if (res?.type === "error") {
      notify(`Failed to start thread: ${res.message}`);
      return null;
    }
    notify("Failed to start thread: unexpected daemon response");
    return null;
  } catch (err) {
    notify(`Failed to start thread: ${String(err)}`);
    return null;
  }
}
