// Types shared with the Rust daemon.
//
// Wire types (everything that crosses the Tauri IPC / WebSocket
// boundary) are generated from `crates/core/provider-api/src/lib.rs`
// by `cargo test -p zenui-provider-api --features ts-bindings --test ts_bindings`
// and live in `./generated/types`. CI fails on `git diff` of the
// generated dir, so a Rust-side field rename that forgets the TS side
// is caught before it ships.
//
// Purely frontend types (`AttachedImage`, `RetryState`) stay here —
// they exist only in browser memory and have no Rust counterpart.

export * from "./generated/types";

// ---------------------------------------------------------------------
// Frontend-only types
// ---------------------------------------------------------------------

/** Pre-send (in-flux) image — lives in ChatInput state until submit.
 * Carries the raw base64 + an object URL for thumbnail rendering. */
export interface AttachedImage {
  /** Local UUID — React key + remove-by-id. */
  id: string;
  /** MIME type, e.g. "image/png". */
  mediaType: string;
  /** Standard base64 (no `data:` prefix). */
  dataBase64: string;
  /** Display name, e.g. "image.png". */
  name: string;
  /** Browser blob URL for rendering thumbnails / lightbox locally,
   * before the bytes hit the server. */
  previewUrl: string;
}

/** Snapshot of an in-flight provider-level auto-retry for a session.
 *  Populated from `turn_retrying` events; cleared on the next
 *  assistant text delta or turn completion. Drives the
 *  `api-retry-banner` above the composer. */
export interface RetryState {
  turnId: string;
  attempt: number;
  maxRetries: number;
  retryDelayMs: number;
  errorStatus?: number;
  error: string;
  /** Epoch ms of when the event landed, so the banner's countdown
   *  can render `retryDelayMs - (now - startedAt)`. */
  startedAt: number;
}

