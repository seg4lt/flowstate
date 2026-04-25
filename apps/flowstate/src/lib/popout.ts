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

/** Fired by `setPopoutPinned` so any component holding a derived
 *  copy of `readPopoutPinPref()` (HeaderActions's `pinned` state, in
 *  particular) can resync after the keyboard shortcut flips the
 *  preference. Without this, the header's pin button would lag
 *  behind reality until the popout was reopened. */
export const POPOUT_PIN_CHANGED_EVENT = "flowstate:popout-pin-changed";

/** True when the current webview was opened as a thread popout.
 *  The Rust `popout_thread` command appends `?popout=1` to the
 *  URL it hands to `WebviewWindowBuilder`, so the flag is visible
 *  from the first render — no IPC round-trip required.
 *
 *  Cached at first call: a window's role (main vs popout) is fixed
 *  at creation by Tauri and never flips. Without the cache, a
 *  client-side navigation inside the popout (e.g. TanStack Router
 *  `navigate({ to: "/code/$sessionId" })`) would drop the
 *  `?popout=1` query string and subsequent calls would wrongly
 *  return `false` — causing guarded `SidebarTrigger`s to suddenly
 *  reappear and header toggles (Pin vs Pop-out) to flip. */
let cachedIsPopout: boolean | null = null;
export function isPopoutWindow(): boolean {
  if (typeof window === "undefined") return false;
  if (cachedIsPopout === null) {
    cachedIsPopout =
      new URLSearchParams(window.location.search).get("popout") === "1";
  }
  return cachedIsPopout;
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
 *  the main window's flag), but the UI shouldn't expose it.
 *
 *  Dispatches `POPOUT_PIN_CHANGED_EVENT` after the localStorage
 *  write so any UI mirroring `readPopoutPinPref()` (header pin
 *  button, primarily) can resync immediately when the keyboard
 *  shortcut flips the pref out from under React state. */
export async function setPopoutPinned(enabled: boolean): Promise<void> {
  writePopoutPinPref(enabled);
  if (typeof window !== "undefined") {
    window.dispatchEvent(
      new CustomEvent(POPOUT_PIN_CHANGED_EVENT, { detail: { enabled } }),
    );
  }
  const label = getCurrentWindow().label;
  await invoke<void>("set_window_always_on_top", { label, enabled });
}
