// Thin helpers for the "pop out a thread into its own window"
// feature. Three concerns, nothing more:
//
//  1. Detecting whether the current window IS a popout (so the
//     shell can strip chrome, and buttons like "Pop out" or the
//     main-window dock badge logic can no-op).
//  2. Asking the Rust side to open a popout window for a session.
//  3. Toggling `alwaysOnTop` on the current popout, with the
//     user's last choice persisted in localStorage so their pin
//     preference survives closing/reopening popouts.
//
// Kept as a tiny module so the popout-detection one-liner has
// exactly one home — importing `isPopoutWindow` from here is
// lighter than rederiving the URL check in every consumer.

import { invoke } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";

const PIN_STORAGE_KEY = "flowstate:popout-pin";

/** True when the current webview was opened as a thread popout.
 *  The Rust `popout_thread` command appends `?popout=1` to the
 *  URL it hands to `WebviewWindowBuilder`, so the flag is visible
 *  from the first render — no IPC round-trip required. */
export function isPopoutWindow(): boolean {
  if (typeof window === "undefined") return false;
  return new URLSearchParams(window.location.search).get("popout") === "1";
}

/** Read the user's last pin preference. Defaults to `false` so
 *  first-time popouts don't surprise users by floating above
 *  everything — the pin toggle inside the popout is the intended
 *  discovery path. */
export function readPopoutPinPref(): boolean {
  try {
    return window.localStorage.getItem(PIN_STORAGE_KEY) === "1";
  } catch {
    return false;
  }
}

function writePopoutPinPref(enabled: boolean): void {
  try {
    window.localStorage.setItem(PIN_STORAGE_KEY, enabled ? "1" : "0");
  } catch {
    /* storage may be unavailable (private mode, quota) */
  }
}

/** Open (or focus, if it already exists) a popout window for the
 *  given session. The Rust side uses a deterministic label
 *  (`thread-<sessionId>`) so reclicking just re-focuses instead
 *  of stacking duplicate windows. */
export function popoutThread(sessionId: string): Promise<void> {
  return invoke<void>("popout_thread", {
    sessionId,
    alwaysOnTop: readPopoutPinPref(),
  });
}

/** Toggle `alwaysOnTop` on the current popout window. Caller is
 *  expected to have checked `isPopoutWindow()` — calling this
 *  from the main window is still safe (the Rust side just flips
 *  the main window's flag), but the UI shouldn't expose it. */
export async function setPopoutPinned(enabled: boolean): Promise<void> {
  writePopoutPinPref(enabled);
  const label = getCurrentWindow().label;
  await invoke<void>("set_window_always_on_top", { label, enabled });
}
