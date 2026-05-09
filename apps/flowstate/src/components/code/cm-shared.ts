/**
 * Plumbing shared between `CodeEditor` (raw code) and
 * `MarkdownEditor` (rich live-preview).
 *
 * Both editors host CodeMirror 6 `EditorView`s and need:
 *   - A way for `Vim :w` to fire the host's save callback.
 *   - The vim register-controller patched once at module load so
 *     yank / delete / change mirror to `navigator.clipboard`.
 *   - A factory that builds the standard extra-keymap (Cmd+S, Alt
 *     line move, multi-cursor) closed over an `onSaveRef`.
 *
 * Defining these here avoids duplicating ~80 lines between the two
 * editor components, and makes the global one-shot registrations
 * (Vim `:w`, clipboard sync) impossible to call in two different
 * orderings.
 */

import {
  copyLineDown,
  copyLineUp,
  indentWithTab,
  moveLineDown,
  moveLineUp,
  toggleBlockComment,
  toggleLineComment,
} from "@codemirror/commands";
import {
  gotoLine,
  selectMatches,
  selectNextOccurrence,
} from "@codemirror/search";
import type { EditorView } from "@codemirror/view";
import { Vim } from "@replit/codemirror-vim";

/** Per-view save handler keyed by the live `EditorView`. WeakMap so
 *  the entry vanishes when the view is GC'd; no manual cleanup. */
export const saveHandlers = new WeakMap<EditorView, () => Promise<void>>();

let vimWriteRegistered = false;

/** Register the Vim `:w` / `:write` ex-command exactly once per
 *  module load. The handler looks up the focused view's save handler
 *  at fire time so it always targets the editor the user is in. */
export function ensureVimWriteRegistered(): void {
  if (vimWriteRegistered) return;
  vimWriteRegistered = true;
  Vim.defineEx("write", "w", (cm: { cm6: EditorView }) => {
    const view = cm.cm6;
    const handler = saveHandlers.get(view);
    if (handler) void handler();
  });
}

let clipboardSyncRegistered = false;

/** Patch Vim's RegisterController so unnamed-register yank/delete/
 *  change additionally mirror to `navigator.clipboard.writeText`.
 *  Once-per-module; subsequent calls are no-ops. */
export function ensureClipboardSyncRegistered(): void {
  if (clipboardSyncRegistered) return;
  clipboardSyncRegistered = true;
  if (typeof navigator === "undefined" || !navigator.clipboard) return;
  const ctrl = (
    Vim as unknown as { getRegisterController?: () => unknown }
  ).getRegisterController?.();
  if (!ctrl) return;
  type PushText = (
    registerName: string | null | undefined,
    operator: string,
    text: string,
    linewise?: boolean,
    blockwise?: boolean,
  ) => void;
  const c = ctrl as { pushText?: PushText };
  const original = c.pushText;
  if (typeof original !== "function") return;
  c.pushText = function (registerName, operator, text, linewise, blockwise) {
    original.call(this, registerName, operator, text, linewise, blockwise);
    if (
      text &&
      !registerName &&
      (operator === "yank" || operator === "delete" || operator === "change")
    ) {
      void navigator.clipboard.writeText(text).catch(() => {
        /* silent — vim register is still updated */
      });
    }
  };
}

/** Build the extra-keymap binding array. Closes over `onSaveRef` so
 *  every editor can wire its own save callback through. */
export function buildExtraKeymap(
  onSaveRef: { current: () => Promise<void> },
) {
  return [
    indentWithTab,
    { key: "Alt-ArrowUp", run: moveLineUp, shift: copyLineUp },
    { key: "Alt-ArrowDown", run: moveLineDown, shift: copyLineDown },
    { key: "Mod-d", run: selectNextOccurrence },
    { key: "Mod-Shift-l", run: selectMatches },
    { key: "Mod-/", run: toggleLineComment },
    { key: "Shift-Alt-a", run: toggleBlockComment },
    { key: "Mod-g", run: gotoLine },
    {
      key: "Mod-s",
      preventDefault: true,
      run: () => {
        const fn = onSaveRef.current;
        if (fn) void fn();
        return true;
      },
    },
  ];
}
