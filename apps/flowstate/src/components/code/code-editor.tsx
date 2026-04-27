import * as React from "react";
import {
  Compartment,
  EditorState,
  Range,
  StateEffect,
  StateField,
  type Extension,
} from "@codemirror/state";
import {
  Decoration,
  type DecorationSet,
  EditorView,
  ViewPlugin,
  type ViewUpdate,
  crosshairCursor,
  drawSelection,
  dropCursor,
  highlightActiveLine,
  highlightActiveLineGutter,
  highlightSpecialChars,
  keymap,
  lineNumbers,
  rectangularSelection,
  scrollPastEnd,
} from "@codemirror/view";
import {
  bracketMatching,
  codeFolding,
  foldGutter,
  foldKeymap,
  indentOnInput,
  indentUnit,
} from "@codemirror/language";
import {
  copyLineDown,
  copyLineUp,
  defaultKeymap,
  history,
  historyKeymap,
  indentWithTab,
  moveLineDown,
  moveLineUp,
  toggleBlockComment,
  toggleLineComment,
} from "@codemirror/commands";
import {
  gotoLine,
  highlightSelectionMatches,
  search,
  searchKeymap,
  selectMatches,
  selectNextOccurrence,
} from "@codemirror/search";
import { closeBrackets, closeBracketsKeymap } from "@codemirror/autocomplete";
import { Vim, vim } from "@replit/codemirror-vim";
import type { HighlighterCore, ThemeRegistrationAny } from "shiki/core";
import {
  DARK_THEME,
  LIGHT_THEME,
  ensureLanguageLoaded,
  getHighlighter,
} from "@/lib/shiki-singleton";
import { getGitDiffFile } from "@/lib/api";
import {
  diffLines,
  gitDiffExtension,
  setGitDiffEffect,
  clearGitDiffEffect,
} from "./git-diff-extension";
import { commentExtension } from "./comment-extension";

// Editable code editor — CodeMirror 6 with vim, find/replace,
// multi-cursor, folding, and Shiki-driven syntax highlighting.
//
// Used by `<CodeView>` for the file-viewer pane. Replaces the
// previous read-only `<PierreFile>` integration. Diff rendering
// (`diff-code-block.tsx`, `multibuffer.tsx`) keeps using @pierre/diffs.
//
// Performance notes:
//   * The whole module is pulled in via `React.lazy()` from
//     `code-view.tsx` so CM6 only lands in the bundle on first
//     file open.
//   * Shiki tokenization runs main-thread but is debounced via
//     requestIdleCallback so typing latency never includes a
//     re-tokenize pass. Decorations from the previous tokenize
//     are mapped through the change set so they stay visually
//     anchored across the brief re-tokenize gap.
//   * Vim, theme, soft-wrap, and read-only flags live behind
//     Compartments — toggling any of them reconfigures in place
//     without re-mounting the EditorView (cursor / scroll / undo
//     all preserved).

// ─── shared module-level state ───────────────────────────────────

// Per-view onSave handler, looked up by Vim `:w` and any other
// command that needs the active editor's save path. WeakMap so the
// entry vanishes when the view is GC'd; no manual cleanup needed.
const saveHandlers = new WeakMap<EditorView, () => Promise<void>>();

// Vim `:w` registered exactly once per module load. Looks up the
// focused view's save handler at fire time so it always targets the
// editor the user is actually in.
let vimWriteRegistered = false;
function ensureVimWriteRegistered(): void {
  if (vimWriteRegistered) return;
  vimWriteRegistered = true;
  Vim.defineEx("write", "w", (cm: { cm6: EditorView }) => {
    const view = cm.cm6;
    const handler = saveHandlers.get(view);
    if (handler) void handler();
  });
}

// Mirror vim yank / delete / change to the OS clipboard. The plugin
// only writes to `navigator.clipboard` when the user explicitly
// names the `+` register (`"+y`, `"+d`); plain `y`, `dd`, `cw`
// stay inside vim's own registers. That matches stock vim, but
// most people running vim in a GUI editor expect `set clipboard=
// unnamed` semantics — yank in here, paste with Cmd+V over there.
//
// We patch `RegisterController.pushText` once at module load. Every
// operation that targets the unnamed register (no explicit `"x`
// prefix) for yank / delete / change additionally fires
// `navigator.clipboard.writeText`. Failures are swallowed: the
// vim register is still updated, the user just won't get the OS
// clipboard sync (e.g., insecure context, permission denied).
let clipboardSyncRegistered = false;
function ensureClipboardSyncRegistered(): void {
  if (clipboardSyncRegistered) return;
  clipboardSyncRegistered = true;
  if (typeof navigator === "undefined" || !navigator.clipboard) return;
  // The Vim runtime API isn't strictly typed for the controller's
  // `pushText` method — pull it through `unknown` so TS doesn't
  // complain about the mutation while preserving runtime safety.
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
      // Fire and forget — the clipboard call is async but we
      // don't gate the vim register write on it.
      void navigator.clipboard.writeText(text).catch(() => {
        /* silent — vim register is still updated */
      });
    }
  };
}

// ─── language resolution ─────────────────────────────────────────

// Map a file extension (without the dot) to a Shiki language name.
// Only contains entries where the extension differs from Shiki's
// canonical name; everything else falls through to the raw ext.
const EXT_TO_LANG: Record<string, string> = {
  ts: "typescript",
  mts: "typescript",
  cts: "typescript",
  js: "javascript",
  mjs: "javascript",
  cjs: "javascript",
  py: "python",
  rs: "rust",
  rb: "ruby",
  yml: "yaml",
  md: "markdown",
  sh: "bash",
  zsh: "bash",
  htm: "html",
  "c++": "cpp",
  "h++": "cpp",
  cc: "cpp",
  hh: "cpp",
  hpp: "cpp",
  cs: "csharp",
};

function languageFromPath(path: string): string {
  const dot = path.lastIndexOf(".");
  if (dot === -1) return "text";
  const ext = path.slice(dot + 1).toLowerCase();
  return EXT_TO_LANG[ext] ?? ext;
}

// ─── long-line / large-file detection ────────────────────────────

const MAX_HIGHLIGHTABLE_LINE_LEN = 5_000;

function hasOverlongLine(text: string): boolean {
  // Single linear scan; bails on the first overlong line. Linear
  // and cache-friendly even for multi-MB files.
  let lineStart = 0;
  for (let i = 0; i < text.length; i++) {
    if (text.charCodeAt(i) === 10 /* \n */) {
      if (i - lineStart > MAX_HIGHLIGHTABLE_LINE_LEN) return true;
      lineStart = i + 1;
    }
  }
  return text.length - lineStart > MAX_HIGHLIGHTABLE_LINE_LEN;
}

// ─── indent detection ────────────────────────────────────────────
//
// Inspect leading whitespace to figure out how the file is
// indented. The result drives:
//   * `EditorState.tabSize`  — display width of `\t` in the gutter
//   * `indentUnit`           — the string `Tab` / `indentMore`
//                              inserts at the cursor
//
// Algorithm: walk up to 5 000 lines, count tab-indented vs space-
// indented lines, and (for spaces) collect the unique indent
// lengths. The GCD of those lengths is mathematically the indent
// unit — every level is a multiple of it. Snap to {2, 4, 8} to
// reject 1-space and other off-by-one outliers caused by stray
// alignment spaces. Falls back to 2-space for files with no
// usable signal (empty, single-line, all-flush-left).

export interface DetectedIndent {
  /** String inserted on Tab / indentMore. `"\t"` or `" ".repeat(n)`. */
  unit: string;
  /** Tab display width and visual indent step. */
  size: number;
}

const DEFAULT_INDENT: DetectedIndent = { unit: "  ", size: 2 };

function gcd(a: number, b: number): number {
  while (b !== 0) {
    const t = b;
    b = a % b;
    a = t;
  }
  return a;
}

function detectIndent(text: string): DetectedIndent {
  // Sample size cap — reading the whole file is fine but 5 k lines
  // is enough signal even for monorepo-sized files, and bounds the
  // worst-case scan time on multi-megabyte buffers.
  const SAMPLE_LINES = 5_000;
  let tabLines = 0;
  let spaceLines = 0;
  // Set, not Map — we only care which indent lengths exist, the
  // GCD doesn't need counts.
  const spaceLengths = new Set<number>();

  let lineStart = 0;
  let scannedLines = 0;
  for (let i = 0; i <= text.length; i++) {
    const isEnd = i === text.length || text.charCodeAt(i) === 10;
    if (!isEnd) continue;
    if (i > lineStart) {
      const c = text.charCodeAt(lineStart);
      if (c === 9 /* \t */) {
        tabLines++;
      } else if (c === 32 /* space */) {
        let n = 0;
        while (lineStart + n < i && text.charCodeAt(lineStart + n) === 32) {
          n++;
        }
        // Skip lines that are all whitespace (no leading-indent
        // signal — they'd inject odd lengths into the GCD).
        if (lineStart + n < i) {
          spaceLengths.add(n);
          spaceLines++;
        }
      }
    }
    lineStart = i + 1;
    scannedLines++;
    if (scannedLines >= SAMPLE_LINES) break;
  }

  if (tabLines === 0 && spaceLines === 0) return DEFAULT_INDENT;
  if (tabLines >= spaceLines) {
    // Tab-indented. 4 is the universal display default; we don't
    // try to infer a tab "size" from content — that's a vibes
    // setting per repo, not something the file content encodes.
    return { unit: "\t", size: 4 };
  }

  let g = 0;
  for (const len of spaceLengths) {
    g = g === 0 ? len : gcd(g, len);
  }
  // Snap to the realistic indent sizes. A GCD of 1, 3, 5, 6, 7
  // almost always means a stray alignment space crept in — fall
  // back rather than insert weird amounts of whitespace on Tab.
  if (g === 2 || g === 4 || g === 8) {
    return { unit: " ".repeat(g), size: g };
  }
  return DEFAULT_INDENT;
}

// ─── shiki decoration extension ──────────────────────────────────

const shikiTokensEffect = StateEffect.define<DecorationSet>();

function buildDecorations(
  highlighter: HighlighterCore,
  text: string,
  lang: string,
  theme: ThemeRegistrationAny,
): DecorationSet {
  if (lang === "text" || text.length === 0) return Decoration.none;
  let result;
  try {
    result = highlighter.codeToTokens(text, { lang, theme });
  } catch (err) {
    // Most likely cause: language not loaded yet. The async loader
    // (in createEditorState) re-invokes us once the grammar lands.
    if (import.meta.env.DEV) {
      console.warn(`[code-editor] codeToTokens failed for "${lang}":`, err);
    }
    return Decoration.none;
  }
  const ranges: Range<Decoration>[] = [];
  let pos = 0;
  for (const line of result.tokens) {
    for (const tok of line) {
      const start = pos;
      const end = pos + tok.content.length;
      if (tok.color && start < end) {
        ranges.push(
          Decoration.mark({
            attributes: { style: `color:${tok.color}` },
          }).range(start, end),
        );
      }
      pos = end;
    }
    pos += 1; // newline
  }
  return Decoration.set(ranges, /* sort */ true);
}

const shikiField = StateField.define<DecorationSet>({
  create() {
    return Decoration.none;
  },
  update(deco, tr) {
    // Map existing decorations across the change set so they stay
    // anchored to their tokens until the next async re-tokenize
    // lands. Keeps colors stable while typing.
    let next = deco.map(tr.changes);
    for (const e of tr.effects) {
      if (e.is(shikiTokensEffect)) next = e.value;
    }
    return next;
  },
  provide(f) {
    return EditorView.decorations.from(f);
  },
});

interface ShikiPluginConfig {
  highlighter: HighlighterCore;
  lang: string;
  theme: ThemeRegistrationAny;
  // When true (overlong line detected), the plugin holds back from
  // tokenizing — the field stays empty and the editor renders as
  // plain text.
  disabled: boolean;
}

// requestIdleCallback shim — WebKit/JSC only landed it in Safari
// 17.4 (March 2024). Tauri ships against the host WebKit, so older
// macOS could still hit a Safari without it. Fall back to a 60ms
// setTimeout, which is in the same neighborhood as the typical
// idle window and keeps tokenization off the keystroke path.
type IdleHandle = number;
const idleSchedule: (cb: () => void) => IdleHandle =
  typeof requestIdleCallback === "function"
    ? (cb) => requestIdleCallback(cb, { timeout: 100 }) as unknown as IdleHandle
    : (cb) => window.setTimeout(cb, 60) as IdleHandle;
const idleCancel: (h: IdleHandle) => void =
  typeof cancelIdleCallback === "function"
    ? (h) => cancelIdleCallback(h as unknown as number)
    : (h) => window.clearTimeout(h);

function shikiPlugin(cfg: ShikiPluginConfig): Extension {
  return ViewPlugin.fromClass(
    class {
      pending: IdleHandle | null = null;
      destroyed = false;

      constructor(view: EditorView) {
        if (!cfg.disabled) this.schedule(view, /* immediate */ true);
      }

      update(u: ViewUpdate): void {
        if (cfg.disabled) return;
        if (u.docChanged) this.schedule(u.view, /* immediate */ false);
      }

      schedule(view: EditorView, immediate: boolean): void {
        if (this.pending !== null) idleCancel(this.pending);
        const run = () => {
          this.pending = null;
          if (this.destroyed) return;
          if (!view.dom.isConnected) return;
          const text = view.state.doc.toString();
          const deco = buildDecorations(
            cfg.highlighter,
            text,
            cfg.lang,
            cfg.theme,
          );
          view.dispatch({ effects: shikiTokensEffect.of(deco) });
        };
        if (immediate) {
          // Even "immediate" has to yield. CM6 silently drops any
          // `view.dispatch` call made from inside a ViewPlugin
          // constructor or update(), and our constructor runs as
          // part of the compartment reconfigure that mounts this
          // plugin — so calling `run` synchronously here would
          // post the tokens effect into the void and the field
          // would stay empty (no highlighting). queueMicrotask
          // defers to the very next microtask, after the current
          // update cycle completes, with effectively zero delay.
          queueMicrotask(run);
          return;
        }
        // ~60-100ms delay — long enough for a typing burst to settle,
        // short enough that highlights catch up before the user
        // notices uncolored text. Idle scheduling also yields to
        // layout/paint so we never block frames.
        this.pending = idleSchedule(run);
      }

      destroy(): void {
        this.destroyed = true;
        if (this.pending !== null) idleCancel(this.pending);
      }
    },
  );
}

// ─── base editor theme ───────────────────────────────────────────

// CM6 theme that just sets the chrome (background, gutter, caret,
// selection) to match the rest of the app. Token colors come from
// Shiki via `style="color:..."` attributes on Decoration.mark, so
// this theme stays minimal.
function buildEditorTheme(theme: "light" | "dark"): Extension {
  // Source-of-truth palette is the pierre theme; we read it
  // dynamically so any palette tweak there flows through. The
  // explicit fallbacks keep us rendering even if the theme JSON
  // omits a key.
  const t = theme === "dark" ? DARK_THEME : LIGHT_THEME;
  // Shiki theme JSON shape: `bg`, `fg`, `colors`, `tokenColors`.
  // We only need bg/fg/cursor/selection here.
  const bg = (t as { bg?: string }).bg ?? (theme === "dark" ? "#0b0b0c" : "#ffffff");
  const fg = (t as { fg?: string }).fg ?? (theme === "dark" ? "#e5e5e5" : "#1f1f1f");
  const colors = (t as { colors?: Record<string, string> }).colors ?? {};
  const cursor = colors["editorCursor.foreground"] ?? fg;
  const selectionBg =
    colors["editor.selectionBackground"] ??
    (theme === "dark" ? "#264f7855" : "#add6ff88");
  const lineHighlightBg =
    colors["editor.lineHighlightBackground"] ??
    (theme === "dark" ? "#ffffff0a" : "#0000000a");
  const gutterBg = colors["editorGutter.background"] ?? bg;
  const gutterFg =
    colors["editorLineNumber.foreground"] ??
    (theme === "dark" ? "#6b7280" : "#9ca3af");

  // Vim mode badge palette. The `.cm-vim-panel` text content is
  // `--NORMAL--`, `--INSERT--`, or `--VISUAL--` — we don't have a
  // mode-specific class to colour against, so we just make the
  // whole panel bold/prominent regardless of mode. The fat-cursor
  // colour is what really differentiates modes at-a-glance:
  // block in NORMAL, line in INSERT, the plugin handles the shape;
  // we just pick a colour with enough contrast on either theme.
  const fatCursorBg = theme === "dark" ? "#7dd3fc" : "#0284c7";
  const fatCursorFg = theme === "dark" ? "#0b0b0c" : "#ffffff";
  const panelBg = theme === "dark" ? "#1f1f23" : "#f4f4f5";
  const panelBorder = theme === "dark" ? "#2e2e35" : "#d4d4d8";
  const panelAccent = theme === "dark" ? "#7dd3fc" : "#0369a1";

  return EditorView.theme(
    {
      "&": {
        backgroundColor: bg,
        color: fg,
        height: "100%",
        fontSize: "13px",
      },
      ".cm-scroller": {
        fontFamily:
          'ui-monospace, SFMono-Regular, "SF Mono", Menlo, Consolas, monospace',
        lineHeight: "1.55",
      },
      ".cm-content": { caretColor: cursor },
      ".cm-cursor, .cm-dropCursor": { borderLeftColor: cursor },
      "&.cm-focused > .cm-scroller > .cm-selectionLayer .cm-selectionBackground, .cm-selectionBackground, ::selection":
        { backgroundColor: selectionBg },
      ".cm-activeLine": { backgroundColor: lineHighlightBg },
      ".cm-gutters": {
        backgroundColor: gutterBg,
        color: gutterFg,
        border: "none",
      },
      ".cm-activeLineGutter": { backgroundColor: lineHighlightBg },
      ".cm-foldGutter .cm-gutterElement": {
        cursor: "pointer",
        opacity: "0.6",
      },
      ".cm-foldGutter .cm-gutterElement:hover": { opacity: "1" },

      // ── Vim panel (bottom status bar) ──
      // The plugin renders `<div class="cm-vim-panel"><span>--MODE--</span>
      // <span flex:1></span><span>{partial command}</span></div>`.
      // We override the default ~13px monospace strip with a more
      // prominent bar so the mode is impossible to miss. The first
      // child span is the mode badge — bold + accent colour. The
      // last child span is the partial command (e.g., `2dd` while
      // the user is mid-keystroke); kept dimmer so it doesn't
      // compete visually.
      ".cm-vim-panel": {
        padding: "4px 10px",
        fontFamily:
          'ui-monospace, SFMono-Regular, "SF Mono", Menlo, Consolas, monospace',
        fontSize: "11px",
        minHeight: "1.8em",
        backgroundColor: panelBg,
        borderTop: `1px solid ${panelBorder}`,
        display: "flex",
        alignItems: "center",
        gap: "8px",
      },
      ".cm-vim-panel > span:first-child": {
        fontWeight: "700",
        letterSpacing: "0.04em",
        color: panelAccent,
        cursor: "pointer",
      },
      ".cm-vim-panel > span:last-child": {
        opacity: "0.7",
      },
      ".cm-vim-panel input": {
        border: "none",
        outline: "none",
        backgroundColor: "transparent",
        color: fg,
        flex: "1",
        fontFamily: "inherit",
        fontSize: "inherit",
      },

      // ── Vim block cursor (NORMAL / VISUAL mode) ──
      // The plugin's default green block (`#77ee77`) clashes with
      // every theme. Override with theme-tuned colours so the block
      // cursor is obvious without being garish, and so characters
      // sitting under the block stay legible.
      ".cm-fat-cursor": {
        backgroundColor: fatCursorBg + " !important",
        color: fatCursorFg + " !important",
        border: "none !important",
      },
      "&:not(.cm-focused) .cm-fat-cursor": {
        backgroundColor: "transparent !important",
        outline: `1px solid ${fatCursorBg}`,
        color: "inherit !important",
      },

      // ── Search / replace panel ──
      // CM6 ships the search panel with raw browser defaults
      // (Helvetica, square buttons, native checkboxes). Re-skin
      // to match the rest of the editor: monospace 11 px, padded
      // flex row, theme-tuned inputs and buttons, accent-coloured
      // focus ring, and a less garish search-match highlight than
      // the default flat yellow / cyan.
      ".cm-panels": {
        backgroundColor: panelBg,
        color: fg,
      },
      ".cm-panels-top": {
        borderBottom: `1px solid ${panelBorder}`,
      },
      ".cm-panel.cm-search": {
        padding: "8px 36px 8px 10px",
        display: "flex",
        alignItems: "center",
        gap: "6px",
        flexWrap: "wrap",
        fontFamily:
          'ui-monospace, SFMono-Regular, "SF Mono", Menlo, Consolas, monospace',
        fontSize: "11px",
        position: "relative",
      },
      ".cm-panel.cm-search input": {
        margin: "0",
        padding: "4px 8px",
        border: `1px solid ${panelBorder}`,
        borderRadius: "4px",
        backgroundColor: bg,
        color: fg,
        fontFamily: "inherit",
        fontSize: "11px",
        outline: "none",
        minWidth: "200px",
      },
      ".cm-panel.cm-search input:focus": {
        borderColor: panelAccent,
        boxShadow: `0 0 0 1px ${panelAccent}`,
      },
      ".cm-panel.cm-search input[type=checkbox]": {
        minWidth: "auto",
        margin: "0 4px 0 0",
        accentColor: panelAccent,
        cursor: "pointer",
      },
      ".cm-panel.cm-search button": {
        margin: "0",
        padding: "4px 10px",
        border: `1px solid ${panelBorder}`,
        borderRadius: "4px",
        backgroundColor: bg,
        color: fg,
        fontFamily: "inherit",
        fontSize: "11px",
        cursor: "pointer",
        transition: "background-color 80ms, border-color 80ms",
      },
      ".cm-panel.cm-search button:hover": {
        backgroundColor: lineHighlightBg,
        borderColor: panelAccent,
      },
      ".cm-panel.cm-search label": {
        display: "inline-flex",
        alignItems: "center",
        margin: "0",
        fontSize: "11px",
        color: gutterFg,
        cursor: "pointer",
        whiteSpace: "nowrap",
      },
      ".cm-panel.cm-search br": { display: "none" },
      ".cm-panel.cm-search [name=close]": {
        position: "absolute",
        top: "6px",
        right: "8px",
        background: "transparent",
        border: "none",
        color: gutterFg,
        cursor: "pointer",
        fontSize: "16px",
        lineHeight: "1",
        padding: "4px 6px",
        borderRadius: "4px",
        minWidth: "auto",
      },
      ".cm-panel.cm-search [name=close]:hover": {
        color: fg,
        backgroundColor: lineHighlightBg,
      },

      // Search-match highlights. Defaults are flat yellow (light)
      // and cyan (dark), both of which are unreadable on top of
      // Shiki tokens. Amber/orange give clear contrast in either
      // theme without competing with the selection's blue.
      ".cm-searchMatch": {
        backgroundColor:
          theme === "dark" ? "rgba(250, 204, 21, 0.22)" : "rgba(250, 204, 21, 0.4)",
        outline:
          theme === "dark"
            ? "1px solid rgba(250, 204, 21, 0.55)"
            : "1px solid rgba(202, 138, 4, 0.55)",
      },
      ".cm-searchMatch-selected": {
        backgroundColor:
          theme === "dark" ? "rgba(251, 146, 60, 0.45)" : "rgba(251, 146, 60, 0.55)",
        outline:
          theme === "dark"
            ? "1px solid rgba(251, 146, 60, 1)"
            : "1px solid rgba(194, 65, 12, 1)",
      },
    },
    { dark: theme === "dark" },
  );
}

// ─── extra keymap ────────────────────────────────────────────────

// Returns a key binding array that wires up modern-editor commands
// not covered by `defaultKeymap`. Built per-mount so the Mod-s
// binding can close over `onSaveRef`.
function buildExtraKeymap(onSaveRef: React.RefObject<() => Promise<void>>) {
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

// ─── component ───────────────────────────────────────────────────

export interface CodeEditorProps {
  /** Project-relative path. Used as React key — switching paths
   *  remounts the editor with the new file's content. */
  path: string;
  /** File contents at mount. After mount, the EditorState owns the
   *  text — `initialContent` changes are ignored unless `path`
   *  also changes (which forces a remount). */
  initialContent: string;
  /** Resolved app theme. Switching reconfigures the theme & shiki
   *  Compartments without re-mounting. */
  theme: "light" | "dark";
  /** Whether vim mode is active. Reconfigures via Compartment. */
  vimEnabled: boolean;
  /** Whether soft-wrap is on. Reconfigures via Compartment. */
  softWrap: boolean;
  /** Whether git mode is on. When true and `projectPath` is set,
   *  the editor fetches the {before, after} pair for the open file
   *  via `getGitDiffFile` once on mount and paints gutter +
   *  line-bg markers for added / modified lines. Recomputes only
   *  on file open — not per keystroke. Reconfigures via Compartment
   *  so flipping it off has zero overhead. */
  gitModeEnabled?: boolean;
  /** Project root for `getGitDiffFile`. Required when
   *  `gitModeEnabled` is true; ignored otherwise. */
  projectPath?: string | null;
  /** Active chat session. When set, the line-comment affordance
   *  (hover "+", `Mod-Alt-c` keymap, popup → composer) is mounted
   *  via the comment extension. Null disables it entirely. */
  sessionId?: string | null;
  /** When true, edits are blocked. The save flow still works (a
   *  no-op since nothing changed) but typing is rejected. */
  readOnly?: boolean;
  /** Called when the user triggers a save (Cmd+S, Vim `:w`, or
   *  auto-save on focus-out). Receives the current buffer text.
   *  Should resolve when the disk write completes; rejection
   *  surfaces as a toast in the parent. */
  onSave: (contents: string) => Promise<void>;
  /** Called whenever the editor's dirty state flips. Drives the
   *  unsaved-dot in the tab bar. Always called with the latest
   *  boolean value (deduped — won't fire twice in a row). */
  onDirtyChange: (dirty: boolean) => void;
}

export function CodeEditor({
  path,
  initialContent,
  theme,
  vimEnabled,
  softWrap,
  gitModeEnabled = false,
  projectPath = null,
  sessionId = null,
  readOnly = false,
  onSave,
  onDirtyChange,
}: CodeEditorProps): React.ReactElement {
  const containerRef = React.useRef<HTMLDivElement | null>(null);
  const viewRef = React.useRef<EditorView | null>(null);
  const onSaveRef = React.useRef<() => Promise<void>>(() => Promise.resolve());
  const onDirtyChangeRef = React.useRef(onDirtyChange);
  const savedContentRef = React.useRef<string>(initialContent);
  const isDirtyRef = React.useRef<boolean>(false);
  const inImeRef = React.useRef<boolean>(false);

  // Compartments are created once per mount and held in refs so
  // their identity stays stable across React re-renders. Each prop
  // change (vim, theme, wrap, readOnly) reconfigures the matching
  // compartment without rebuilding the whole extension array.
  const vimCompartmentRef = React.useRef(new Compartment());
  const themeCompartmentRef = React.useRef(new Compartment());
  const wrapCompartmentRef = React.useRef(new Compartment());
  const readOnlyCompartmentRef = React.useRef(new Compartment());
  const gitDiffCompartmentRef = React.useRef(new Compartment());
  const commentCompartmentRef = React.useRef(new Compartment());

  // Track the highlighter resolution state via refs so we can re-
  // tokenize when the grammar lazy-loads after the initial mount.
  const highlighterRef = React.useRef<HighlighterCore | null>(null);

  // Long-line guard: scan once at mount. Memoized against `path` —
  // remount happens on path change so this is the right key.
  const overlong = React.useMemo(
    () => hasOverlongLine(initialContent),
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [path],
  );

  // Indent detection runs once per file open. Like `overlong`, this
  // is keyed only on `path` — once we've decided "this file is
  // 4-space indented", we don't keep re-detecting as the user
  // types. Switching tabs (different `path`) re-detects.
  const indent = React.useMemo<DetectedIndent>(
    () => detectIndent(initialContent),
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [path],
  );

  // Keep the latest onSave/onDirtyChange callbacks accessible to
  // the (stable) view extensions through a ref, so the React
  // closure doesn't go stale across re-renders.
  React.useEffect(() => {
    onDirtyChangeRef.current = onDirtyChange;
  }, [onDirtyChange]);

  React.useEffect(() => {
    onSaveRef.current = async () => {
      const view = viewRef.current;
      if (!view) return;
      const current = view.state.doc.toString();
      // Short-circuit: nothing changed since last save (e.g., blur
      // fired but the user hadn't actually edited). No need to
      // round-trip Tauri.
      if (current === savedContentRef.current) return;
      try {
        await onSave(current);
        // Re-baseline. Doing this after the await means a save that
        // races with new typing won't falsely clear the dirty bit
        // for the new edits — the next updateListener tick will
        // flip dirty back on if the doc has moved past `current`.
        savedContentRef.current = current;
        const stillDirty = view.state.doc.toString() !== current;
        if (isDirtyRef.current !== stillDirty) {
          isDirtyRef.current = stillDirty;
          onDirtyChangeRef.current(stillDirty);
        }
      } catch (err) {
        // Bubble — `CodeView` shows the toast. Dirty bit stays on.
        throw err;
      }
    };
  }, [onSave]);

  // Mount the EditorView. `path` change tears down + remounts,
  // which is the right boundary: a different file means different
  // doc, language, dirty baseline, and undo history.
  React.useEffect(() => {
    if (!containerRef.current) return;
    let cancelled = false;
    savedContentRef.current = initialContent;
    isDirtyRef.current = false;
    onDirtyChangeRef.current(false);
    inImeRef.current = false;

    const lang = languageFromPath(path);

    // Build the initial extension array. The Shiki extension starts
    // empty (no highlighter yet); we swap it in via the theme
    // compartment as soon as the grammar lazy-loads.
    const updateListener = EditorView.updateListener.of((u) => {
      if (!u.docChanged) return;
      const dirty = u.state.doc.toString() !== savedContentRef.current;
      if (dirty !== isDirtyRef.current) {
        isDirtyRef.current = dirty;
        onDirtyChangeRef.current(dirty);
      }
    });

    const blurAutoSave = EditorView.domEventHandlers({
      blur: (_event, _view) => {
        if (!isDirtyRef.current || inImeRef.current) return false;
        // Fire-and-forget; errors land in the sonner toast via the
        // parent's onSave wrapper.
        void onSaveRef.current().catch(() => {
          /* swallow — toast already shown */
        });
        return false;
      },
      compositionstart: () => {
        inImeRef.current = true;
        return false;
      },
      compositionend: () => {
        inImeRef.current = false;
        return false;
      },
    });

    const extensions: Extension[] = [
      // Required opt-in for any multi-range selection. Without
      // this facet, CM6 silently collapses every operation that
      // produces more than one selection range to a single one —
      // breaking Cmd+D / Cmd+Shift+L (multi-cursor by word match),
      // Alt-click / Cmd-click (add cursor at click), Alt-drag
      // (rectangular selection), and Vim's `Ctrl+V` visual block
      // mode. Single-range visual modes (`v`, `V`) work without
      // it, but block mode does not.
      EditorState.allowMultipleSelections.of(true),
      // Indent detected from the file's own leading whitespace.
      // `tabSize` controls how wide a literal `\t` glyph renders;
      // `indentUnit` controls what Tab / `indentMore` inserts at
      // the cursor. Detection is one-shot per file open — the
      // editor doesn't try to track schema drift mid-session.
      EditorState.tabSize.of(indent.size),
      indentUnit.of(indent.unit),
      // vim() must be first so its keymap takes precedence in NORMAL
      // mode. Compartment lets us flip vim on/off in place.
      // `status: true` mounts the `--NORMAL--` / `--INSERT--` /
      // `--VISUAL--` panel at the bottom; we style it prominently
      // in the editor theme below so the mode is easy to read.
      vimCompartmentRef.current.of(vimEnabled ? vim({ status: true }) : []),
      lineNumbers(),
      highlightActiveLineGutter(),
      foldGutter(),
      codeFolding(),
      highlightActiveLine(),
      highlightSelectionMatches(),
      // Visualises invisible characters (zero-width, control
      // chars, BOM, replacement chars). Useful when files have
      // funky pasted whitespace or invisible Unicode.
      highlightSpecialChars(),
      drawSelection(),
      // Drop cursor renders a caret at the drop point while text
      // is being dragged inside the editor — without this, dragging
      // a selection has no visual indication of where it'll land.
      dropCursor(),
      // Alt-drag for rectangular (column) selection. Pairs with
      // crosshairCursor below for the visual hint that the mode
      // is active. Both rely on `allowMultipleSelections` above.
      rectangularSelection(),
      crosshairCursor(),
      bracketMatching(),
      closeBrackets(),
      indentOnInput(),
      history(),
      shikiField,
      keymap.of([
        ...closeBracketsKeymap,
        ...defaultKeymap,
        ...historyKeymap,
        ...searchKeymap,
        ...foldKeymap,
        ...buildExtraKeymap(onSaveRef),
      ]),
      search({ top: true }),
      // Adds bottom padding so the last line of the file can
      // scroll up to the top of the viewport — matches the
      // "scroll past EOF" behaviour every modern editor has.
      scrollPastEnd(),
      blurAutoSave,
      updateListener,
      readOnlyCompartmentRef.current.of(
        EditorState.readOnly.of(readOnly || overlong),
      ),
      wrapCompartmentRef.current.of(softWrap ? EditorView.lineWrapping : []),
      // Git diff markers compartment. Off by default; the effect
      // below reconfigures it to `gitDiffExtension()` when the
      // user flips git mode on, and clears decorations when off.
      gitDiffCompartmentRef.current.of(
        gitModeEnabled ? gitDiffExtension() : [],
      ),
      // Comment-to-composer compartment. Wires the hover gutter
      // "+", the Mod-Alt-c keymap, and the popup tooltip when a
      // chat session is attached. Empty when sessionId is null
      // (e.g. /browse route) — same disable semantics as the
      // diff-comment overlay.
      commentCompartmentRef.current.of(
        sessionId ? commentExtension({ path, sessionId }) : [],
      ),
      // Theme compartment hosts both the editor theme and the Shiki
      // plugin so reconfiguring on theme change retokenizes against
      // the new theme without remounting. Starts with no Shiki —
      // the async highlighter init below swaps it in once the
      // grammar is ready.
      themeCompartmentRef.current.of([buildEditorTheme(theme)]),
    ];

    const state = EditorState.create({
      doc: initialContent,
      extensions,
    });
    const view = new EditorView({
      state,
      parent: containerRef.current,
    });
    viewRef.current = view;
    saveHandlers.set(view, () => onSaveRef.current());
    ensureVimWriteRegistered();
    ensureClipboardSyncRegistered();

    // Resolve the highlighter + grammar asynchronously. Once ready,
    // reconfigure the theme compartment to include the Shiki plugin
    // (which kicks off the first tokenize).
    (async () => {
      let highlighter: HighlighterCore;
      try {
        highlighter = await getHighlighter();
      } catch (err) {
        if (import.meta.env.DEV) {
          console.warn("[code-editor] highlighter init failed:", err);
        }
        return;
      }
      if (cancelled || viewRef.current !== view) return;
      const supported = lang !== "text" && (await ensureLanguageLoaded(highlighter, lang));
      if (cancelled || viewRef.current !== view) return;
      highlighterRef.current = highlighter;
      const effectiveLang = supported ? lang : "text";
      const themeRegistration = theme === "dark" ? DARK_THEME : LIGHT_THEME;
      view.dispatch({
        effects: themeCompartmentRef.current.reconfigure([
          buildEditorTheme(theme),
          shikiPlugin({
            highlighter,
            lang: effectiveLang,
            theme: themeRegistration,
            disabled: overlong || effectiveLang === "text",
          }),
        ]),
      });
    })();

    return () => {
      cancelled = true;
      saveHandlers.delete(view);
      view.destroy();
      if (viewRef.current === view) viewRef.current = null;
      // Marks deps used by the closure — any prop change that
      // requires a remount is gated by `path` here. theme/vim/wrap
      // changes go through their own effects below.
    };
    // We intentionally only mount on path change. theme/vim/wrap/
    // readOnly are reconfigured via the effects below.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [path]);

  // Vim toggle reactivity. Exit insert mode first so toggling OFF
  // mid-INSERT doesn't leave the user in an inconsistent state.
  React.useEffect(() => {
    const view = viewRef.current;
    if (!view) return;
    if (!vimEnabled) {
      try {
        Vim.exitInsertMode(view as unknown as Parameters<typeof Vim.exitInsertMode>[0]);
      } catch {
        /* not in insert mode — ignore */
      }
    }
    view.dispatch({
      effects: vimCompartmentRef.current.reconfigure(
        vimEnabled ? vim({ status: true }) : [],
      ),
    });
  }, [vimEnabled]);

  // Theme reactivity. We rebuild the Shiki plugin from scratch with
  // the new theme so existing decorations re-tokenize against the
  // new palette.
  React.useEffect(() => {
    const view = viewRef.current;
    const highlighter = highlighterRef.current;
    if (!view) return;
    const themeRegistration = theme === "dark" ? DARK_THEME : LIGHT_THEME;
    const lang = languageFromPath(path);
    const baseTheme = buildEditorTheme(theme);
    const extensions: Extension[] = [baseTheme];
    if (highlighter && !overlong && lang !== "text") {
      extensions.push(
        shikiPlugin({
          highlighter,
          lang,
          theme: themeRegistration,
          disabled: false,
        }),
      );
    }
    view.dispatch({
      effects: themeCompartmentRef.current.reconfigure(extensions),
    });
    // path is intentionally a dep so a theme change that lands
    // mid-mount picks up the right language.
  }, [theme, path, overlong]);

  // Soft-wrap reactivity.
  React.useEffect(() => {
    const view = viewRef.current;
    if (!view) return;
    view.dispatch({
      effects: wrapCompartmentRef.current.reconfigure(
        softWrap ? EditorView.lineWrapping : [],
      ),
    });
  }, [softWrap]);

  // Read-only reactivity. Long-line files are forced read-only at
  // mount; this effect handles the explicit `readOnly` prop only.
  React.useEffect(() => {
    const view = viewRef.current;
    if (!view) return;
    view.dispatch({
      effects: readOnlyCompartmentRef.current.reconfigure(
        EditorState.readOnly.of(readOnly || overlong),
      ),
    });
  }, [readOnly, overlong]);

  // Git mode reactivity. Two phases:
  //   1. Reconfigure the compartment so the field/gutter/theme are
  //      either mounted or `[]` (zero overhead off-mode).
  //   2. When ON, fetch the {before, after} pair from Rust once,
  //      compute added / modified line numbers via the LCS in
  //      `diffLines()`, and dispatch `setGitDiffEffect`. We do NOT
  //      retry on keystrokes — the diff snapshot is taken at file
  //      open / mode-flip time, then mapped through edits by the
  //      StateField until the user reopens. This is the deliberate
  //      perf line we drew (no per-keystroke recompute).
  //
  //   When the file isn't in the repo's diff (untracked-and-unchanged,
  //   or the rust call returns matching before/after), `diffLines`
  //   returns empty arrays and the editor renders normally — same
  //   visual result as having git mode off, no error noise.
  React.useEffect(() => {
    const view = viewRef.current;
    if (!view) return;
    let cancelled = false;
    view.dispatch({
      effects: gitDiffCompartmentRef.current.reconfigure(
        gitModeEnabled ? gitDiffExtension() : [],
      ),
    });
    if (!gitModeEnabled || !projectPath) {
      // Make doubly sure stale decorations don't linger when we
      // flip OFF — the compartment swap drops the field, but
      // belt-and-braces.
      view.dispatch({ effects: clearGitDiffEffect.of() });
      return;
    }
    (async () => {
      try {
        const { before, after } = await getGitDiffFile(projectPath, path);
        if (cancelled || viewRef.current !== view) return;
        // Edge case: rust returned the same string on both sides
        // (file at HEAD matches working tree). Skip the LCS work.
        if (before === after) {
          view.dispatch({
            effects: setGitDiffEffect.of({ added: [], modified: [] }),
          });
          return;
        }
        const lines = diffLines(before, after);
        if (cancelled || viewRef.current !== view) return;
        view.dispatch({ effects: setGitDiffEffect.of(lines) });
      } catch (err) {
        if (cancelled || viewRef.current !== view) return;
        if (import.meta.env.DEV) {
          // Most common cause: file isn't tracked / project isn't a
          // git repo. Silently leave decorations empty rather than
          // surface a toast on every untracked file open.
          console.debug("[code-editor] getGitDiffFile failed:", err);
        }
        view.dispatch({ effects: clearGitDiffEffect.of() });
      }
    })();
    return () => {
      cancelled = true;
    };
    // `path` is a dep so re-fetch when the file changes (the parent
    // remounts the editor on path change anyway via the React.lazy
    // key, so this only fires for the initial mount + git-mode flip).
  }, [gitModeEnabled, projectPath, path]);

  // Comment-extension reactivity. Reconfigure when sessionId
  // appears / disappears / changes — the extension closes over
  // sessionId at construction time, so we need a fresh extension
  // any time it shifts. Path is also a dep because the extension's
  // anchor uses it; in practice path changes remount the editor
  // (see the `[path]` mount effect above), so this fires only on
  // sessionId transitions during the editor's lifetime.
  React.useEffect(() => {
    const view = viewRef.current;
    if (!view) return;
    view.dispatch({
      effects: commentCompartmentRef.current.reconfigure(
        sessionId ? commentExtension({ path, sessionId }) : [],
      ),
    });
  }, [sessionId, path]);

  return (
    <div className="flex h-full min-h-0 min-w-0 flex-1 flex-col">
      {overlong ? (
        <div className="border-b border-border bg-muted px-3 py-1.5 text-[11px] text-muted-foreground">
          Syntax highlighting disabled for very long lines (&gt;5000 chars).
          File is read-only.
        </div>
      ) : null}
      <div ref={containerRef} className="min-h-0 flex-1 overflow-hidden" />
    </div>
  );
}

// Default export so `React.lazy(() => import("./code-editor"))`
// picks up the component without the consumer having to unwrap a
// named export.
export default CodeEditor;
