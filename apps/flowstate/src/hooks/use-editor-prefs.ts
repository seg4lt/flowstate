import * as React from "react";
import { sessionTransient } from "@/stores/session-transient-store";

// Editor preferences — small set of user-flippable knobs that the
// CodeMirror file viewer respects.
//
// Two lifetime classes live in here:
//
//   * vimEnabled — true preference, one value per user. Backed by
//     localStorage so the choice survives reloads, and broadcast
//     through a module-level store so two editors in a split pane
//     stay in sync without a React context.
//
//   * gitModeEnabled — per-thread transient state. Stored in the
//     `sessionTransient` store keyed by sessionId, so each chat
//     thread remembers whether git mode was on independently.
//     Lost on reload, mirroring the diff-panel-open flag. The
//     hook now takes a `sessionId` argument: pass the active
//     session's id, or `null/undefined` if there is no session
//     (the toggle is read-only off in that case).
//
// Soft-wrap used to live here as a togglable preference but is now
// hardcoded on at the editor level — long lines were breaking the
// CodeMirror viewport (horizontal blow-out on minified or wide
// generated files). There's nothing to flip.
//
// v1 surfaces:
//   * vimEnabled    — when true, `@replit/codemirror-vim` is included
//                     in the editor's extension stack via a Compartment.
//                     Default: true (the user explicitly asked for vim).
//   * gitModeEnabled — when true, the code view replaces the project
//                     tree with a flat list of changed files (vs HEAD)
//                     and the editor paints gutter + line-bg markers
//                     for added / modified lines via a Compartment.
//                     Default: false. Per-session — flipping it on one
//                     thread does NOT affect any other thread.
//
// `vimEnabled` is surfaced to the user in the Settings page
// (Appearance group) — see `VimModeRow` in
// `components/settings/settings-view.tsx`. It used to also live as a
// per-editor toolbar button, but since flipping it always affected
// every editor through the singleton below, the toolbar copy was
// removed in favour of one source of truth in Settings.
//
// We deliberately keep this on localStorage rather than the SQLite
// settings store — it's a UI-only client preference, no value in
// roundtripping through Tauri IPC for a boolean. If the editor grows
// more prefs (font size, indent width, ...), fold them into a proper
// store at that point.

const VIM_KEY = "flowstate:editor.vim-enabled";

interface GlobalEditorPrefs {
  vimEnabled: boolean;
}

function readBool(key: string, fallback: boolean): boolean {
  if (typeof window === "undefined") return fallback;
  const raw = window.localStorage.getItem(key);
  if (raw === "true") return true;
  if (raw === "false") return false;
  return fallback;
}

function writeBool(key: string, value: boolean): void {
  try {
    window.localStorage.setItem(key, value ? "true" : "false");
  } catch {
    /* private mode / quota — fall through, the in-memory state is
     *  still authoritative for this session */
  }
}

// Module-singleton state + subscriber list. Multiple editors share
// the same prefs and re-render together when the toggle flips.
let cached: GlobalEditorPrefs | null = null;
const subscribers = new Set<() => void>();

function getSnapshot(): GlobalEditorPrefs {
  if (cached === null) {
    cached = {
      vimEnabled: readBool(VIM_KEY, true),
    };
  }
  return cached;
}

function subscribe(notify: () => void): () => void {
  subscribers.add(notify);
  return () => subscribers.delete(notify);
}

function notifyAll(): void {
  for (const fn of subscribers) fn();
}

export interface EditorPrefsApi {
  vimEnabled: boolean;
  setVimEnabled: (value: boolean) => void;
  gitModeEnabled: boolean;
  setGitModeEnabled: (value: boolean) => void;
}

export function useEditorPrefs(
  sessionId?: string | null,
): EditorPrefsApi {
  const snapshot = React.useSyncExternalStore(subscribe, getSnapshot, getSnapshot);

  // Per-session git-mode flag. The store-level subscribe path lets
  // toggles made elsewhere (e.g. a future header button or the diff
  // panel) propagate into this hook within the same render. The
  // snapshot factory is sessionId-aware so React.useSyncExternalStore
  // returns a stable boolean rather than a fresh object on every
  // render.
  const getGitMode = React.useCallback(
    () => sessionTransient.getGitMode(sessionId),
    [sessionId],
  );
  const gitModeEnabled = React.useSyncExternalStore(
    sessionTransient.subscribeGitMode,
    getGitMode,
    getGitMode,
  );

  const setVimEnabled = React.useCallback((value: boolean) => {
    if (cached?.vimEnabled === value) return;
    cached = { ...getSnapshot(), vimEnabled: value };
    writeBool(VIM_KEY, value);
    notifyAll();
  }, []);

  const setGitModeEnabled = React.useCallback(
    (value: boolean) => {
      // No-op without a session — there's nowhere to remember the
      // flag, and the toggle is rendered with `false` in that case
      // anyway. Avoids flipping a "global" gitMode that bleeds across
      // future sessionId values.
      if (!sessionId) return;
      sessionTransient.setGitMode(sessionId, value);
    },
    [sessionId],
  );

  return {
    vimEnabled: snapshot.vimEnabled,
    setVimEnabled,
    gitModeEnabled,
    setGitModeEnabled,
  };
}
