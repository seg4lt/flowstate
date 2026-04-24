import * as React from "react";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { useApp } from "@/stores/app-store";
import { isPopoutWindow } from "@/lib/popout";

/**
 * Sync the macOS Dock / Linux launcher / Windows taskbar icon badge
 * with the number of threads that want user attention.
 *
 * Count = |awaitingInputSessionIds ∪ doneSessionIds|. Each thread
 * counts once regardless of how many permission/question prompts are
 * queued on it. Unlike the in-app sidebar dot, we do NOT exclude the
 * active session — the dock badge is visible when the app is
 * backgrounded, where "active" doesn't mean the user is actually
 * looking at that thread.
 *
 * Decrements are automatic: the two Sets shrink when a prompt is
 * answered, a turn completes while the user is in that thread, or the
 * thread is opened (clears doneSessionIds). React re-runs this effect
 * on every change and the OS badge is overwritten to match.
 *
 * Pass `undefined` to clear the badge when nothing wants attention —
 * per Tauri's API, an explicit undefined removes the badge rather than
 * showing a dot or zero.
 */
export function useDockBadge(): void {
  const { state } = useApp();
  const awaiting = state.awaitingInputSessionIds;
  const done = state.doneSessionIds;

  React.useEffect(() => {
    // Thread popouts share the same AppProvider (they hydrate from
    // the same broadcast stream) and would otherwise race the main
    // window to set the dock badge — whichever window's effect
    // fired last would clobber the other's count. Since the dock
    // icon is app-wide, delegating the badge exclusively to the
    // main window is both correct and avoids the flicker.
    if (isPopoutWindow()) return;

    const ids = new Set<string>();
    for (const id of awaiting) ids.add(id);
    for (const id of done) ids.add(id);
    const count = ids.size;
    getCurrentWindow()
      .setBadgeCount(count > 0 ? count : undefined)
      .catch((err) => {
        // Non-fatal — a missing ACL permission, non-Tauri runtime, or
        // an unsupported platform should not break the app. Surface it
        // to the console once per failure so regressions are visible in
        // dev without spamming in production.
        console.warn("[dock-badge] setBadgeCount failed:", err);
      });
  }, [awaiting, done]);
}
