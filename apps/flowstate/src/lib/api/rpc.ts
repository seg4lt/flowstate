import { Channel, invoke } from "@tauri-apps/api/core";
import type {
  AttachmentData,
  ClientMessage,
  ContextBreakdown,
  ServerMessage,
} from "../types";

// Core RPC envelope + session-scoped request helpers. Everything in
// this file funnels through `handle_message` on the Rust side —
// typed wrappers that `sendMessage` and decode the single-message
// response into a narrower return type, throwing on
// `ServerMessage::Error` so callers can `.catch` for user-facing
// toasts.

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
 * Persist per-session settings (compaction priorities, future
 * fields) into the runtime's `provider_state.metadata`. Sparse —
 * only fields the caller passes are merged on the Rust side; absent
 * fields keep their prior value. Pass an empty string to clear a
 * field. Throws on `ServerMessage::Error`. Resolves once the
 * runtime has flushed to disk, so the very next turn picks up the
 * new value.
 */
export async function updateSessionSettings(
  sessionId: string,
  settings: { compactCustomInstructions?: string },
): Promise<void> {
  const resp = await sendMessage({
    type: "update_session_settings",
    session_id: sessionId,
    compact_custom_instructions: settings.compactCustomInstructions,
  });
  if (resp?.type === "ack") return;
  if (resp?.type === "error") throw new Error(resp.message);
  throw new Error("unexpected response to update_session_settings");
}

/**
 * Revert all on-disk file changes made by `turnId` and every later
 * turn back to their pre-turn state. Best-effort and partial — a
 * single bad path errors the whole call; previously-touched paths
 * are NOT rolled back (no transactional FS). Throws on
 * `ServerMessage::Error`. The runtime broadcasts a
 * `RuntimeEvent::FilesRewound` after success which carries the
 * detailed lists; this call's resolved Ack is just a "request
 * accepted" signal.
 */
export async function rewindFiles(
  sessionId: string,
  turnId: string,
): Promise<void> {
  const resp = await sendMessage({
    type: "rewind_files",
    session_id: sessionId,
    turn_id: turnId,
  });
  if (resp?.type === "ack") return;
  if (resp?.type === "error") throw new Error(resp.message);
  throw new Error("unexpected response to rewind_files");
}

export function connectStream(
  onMessage: (message: ServerMessage) => void,
): Promise<void> {
  const channel = new Channel<ServerMessage>();
  channel.onmessage = onMessage;
  return invoke("connect", { onEvent: channel });
}

// Flowstate-app-owned key/value store. Backed by SQLite at
// <app_data_dir>/user_config.sqlite — separate from the agent
// SDK's daemon database. SDK and app each own their own SQLite;
// app-level UI tunables (pool size, future toggles) live here, not
// in the daemon's schema.
//
// `getUserConfig` returns null when the key has never been set;
// callers should treat that as "use the default."
export function getUserConfig(key: string): Promise<string | null> {
  return invoke<string | null>("get_user_config", { key });
}

export function setUserConfig(key: string, value: string): Promise<void> {
  return invoke<void>("set_user_config", { key, value });
}

// Resolved cross-platform app data dir for Flowstate — the same
// directory the daemon database, threads dir, and user_config
// sqlite live under. Surfaced to the Settings UI as a read-only
// path so users can copy it and open in Finder / Explorer / a
// terminal.
export function getAppDataDir(): Promise<string> {
  return invoke<string>("get_app_data_dir");
}

// Spawn an external code editor (`zed`, `code`, `cursor`, `idea`,
// `subl`, …) on the project root. The rust side calls the binary
// with the path as a positional arg and detaches; the promise
// rejects when the binary isn't on $PATH or the path isn't a
// directory so the frontend can show a friendly toast.
export function openInEditor(editor: string, path: string): Promise<void> {
  return invoke<void>("open_in_editor", { editor, path });
}
