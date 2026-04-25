import * as React from "react";

// Editor preferences — small set of user-flippable knobs that the
// CodeMirror file viewer respects. Backed by localStorage so the
// choice survives reloads, and broadcast through a module-level
// store so two editors in a split pane stay in sync without a
// React context.
//
// v1 surfaces:
//   * vimEnabled — when true, `@replit/codemirror-vim` is included
//                  in the editor's extension stack via a Compartment.
//                  Default: true (the user explicitly asked for vim).
//   * softWrap   — when true, `EditorView.lineWrapping` is included
//                  via a Compartment. Default: false (most code
//                  reads better with horizontal scrolling).
//
// We deliberately don't use the existing settings store yet — the
// editor only owns these two booleans and there's no value in
// roundtripping through SQLite for a localStorage-class concern.
// If the editor grows more prefs (font size, indent width, ...),
// fold them into a proper store at that point.

const VIM_KEY = "flowstate:editor.vim-enabled";
const WRAP_KEY = "flowstate:editor.soft-wrap";

interface EditorPrefs {
  vimEnabled: boolean;
  softWrap: boolean;
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
let cached: EditorPrefs | null = null;
const subscribers = new Set<() => void>();

function getSnapshot(): EditorPrefs {
  if (cached === null) {
    cached = {
      vimEnabled: readBool(VIM_KEY, true),
      softWrap: readBool(WRAP_KEY, false),
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
  softWrap: boolean;
  setSoftWrap: (value: boolean) => void;
}

export function useEditorPrefs(): EditorPrefsApi {
  const snapshot = React.useSyncExternalStore(subscribe, getSnapshot, getSnapshot);

  const setVimEnabled = React.useCallback((value: boolean) => {
    if (cached?.vimEnabled === value) return;
    cached = { ...getSnapshot(), vimEnabled: value };
    writeBool(VIM_KEY, value);
    notifyAll();
  }, []);

  const setSoftWrap = React.useCallback((value: boolean) => {
    if (cached?.softWrap === value) return;
    cached = { ...getSnapshot(), softWrap: value };
    writeBool(WRAP_KEY, value);
    notifyAll();
  }, []);

  return {
    vimEnabled: snapshot.vimEnabled,
    setVimEnabled,
    softWrap: snapshot.softWrap,
    setSoftWrap,
  };
}
