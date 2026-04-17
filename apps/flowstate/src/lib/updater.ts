// Singleton store + thin wrapper around `@tauri-apps/plugin-updater`.
//
// The updater plugin needs three things to work end-to-end:
//   1. The `plugins.updater` block in tauri.conf.json — endpoint URL
//      pointing at https://github.com/seg4lt/flowstate/releases/latest/download/latest.json
//      and the embedded minisign public key.
//   2. The matching private key, available at build time as the
//      `TAURI_SIGNING_PRIVATE_KEY` GitHub secret consumed by the
//      release workflow. The CI build embeds signatures into the
//      bundle artifacts and emits a `latest.json` manifest that the
//      plugin reads here.
//   3. The capability `updater:default` + `process:default` /
//      `process:allow-restart` (so we can relaunch the app after
//      install). Both are wired in capabilities/default.json.
//
// We model the lifecycle as a small finite-state singleton (similar
// shape to use-toast.ts) so multiple parts of the UI can subscribe:
// the global UpdateBanner shows progress + the "Install & Restart"
// CTA, and the Settings page exposes a manual "Check now" button
// that flips the same state.
import * as React from "react";

import { check, type Update } from "@tauri-apps/plugin-updater";
import { relaunch } from "@tauri-apps/plugin-process";

export type UpdaterStatus =
  | { kind: "idle" }
  | { kind: "checking" }
  | { kind: "up-to-date" }
  | { kind: "available"; update: Update }
  | { kind: "downloading"; downloaded: number; total: number | null }
  | { kind: "installing" }
  | { kind: "error"; message: string };

let memoryState: UpdaterStatus = { kind: "idle" };
const listeners: Array<(state: UpdaterStatus) => void> = [];

function setState(next: UpdaterStatus) {
  memoryState = next;
  for (const listener of listeners) listener(next);
}

export function getUpdaterStatus(): UpdaterStatus {
  return memoryState;
}

/**
 * Hit the updater endpoint and update the singleton state. Safe to
 * call concurrently — if a check is already in flight (or a download
 * is running), this is a no-op and returns the current state.
 *
 * Returns the resulting status so callers (e.g. the Settings button)
 * can decide whether to show a "you're up to date" toast inline
 * without waiting for a state subscription tick.
 */
export async function checkForUpdate(): Promise<UpdaterStatus> {
  if (
    memoryState.kind === "checking" ||
    memoryState.kind === "downloading" ||
    memoryState.kind === "installing"
  ) {
    return memoryState;
  }

  // If we previously found an update and the user hasn't acted on it
  // yet, surface the same `available` state instead of refetching —
  // hitting GitHub again would just return the same manifest, and
  // we'd risk replacing the cached `Update` object the banner is
  // about to call `downloadAndInstall` on.
  if (memoryState.kind === "available") {
    return memoryState;
  }

  setState({ kind: "checking" });
  try {
    const update = await check();
    if (update) {
      setState({ kind: "available", update });
    } else {
      setState({ kind: "up-to-date" });
    }
    return memoryState;
  } catch (err) {
    const message = err instanceof Error ? err.message : String(err);
    setState({ kind: "error", message });
    return memoryState;
  }
}

/**
 * Download + install the given update, then relaunch the app. The
 * banner subscribes to state for progress; on success the process
 * is replaced before this promise resolves on the new instance.
 */
export async function installUpdate(update: Update): Promise<void> {
  setState({ kind: "downloading", downloaded: 0, total: null });
  try {
    let downloaded = 0;
    let total: number | null = null;

    await update.downloadAndInstall((event) => {
      switch (event.event) {
        case "Started":
          total = event.data.contentLength ?? null;
          setState({ kind: "downloading", downloaded: 0, total });
          break;
        case "Progress":
          downloaded += event.data.chunkLength;
          setState({ kind: "downloading", downloaded, total });
          break;
        case "Finished":
          setState({ kind: "installing" });
          break;
      }
    });

    // The process is about to be replaced. State change is mostly
    // for completeness — the new process boots fresh.
    await relaunch();
  } catch (err) {
    const message = err instanceof Error ? err.message : String(err);
    setState({ kind: "error", message });
  }
}

/**
 * Manually clear the status back to idle. Used after the Settings
 * button surfaces a "you're up to date" or "error" toast so the
 * banner doesn't keep showing stale state.
 */
export function resetUpdaterStatus() {
  if (
    memoryState.kind === "up-to-date" ||
    memoryState.kind === "error"
  ) {
    setState({ kind: "idle" });
  }
}

export function useUpdaterStatus(): UpdaterStatus {
  const [state, setLocal] = React.useState<UpdaterStatus>(memoryState);
  React.useEffect(() => {
    listeners.push(setLocal);
    return () => {
      const idx = listeners.indexOf(setLocal);
      if (idx > -1) listeners.splice(idx, 1);
    };
  }, []);
  return state;
}
