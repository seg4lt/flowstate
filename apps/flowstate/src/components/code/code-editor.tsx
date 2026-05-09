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
  defaultKeymap,
  history,
  historyKeymap,
} from "@codemirror/commands";
import {
  highlightSelectionMatches,
  search,
  searchKeymap,
} from "@codemirror/search";
import { closeBrackets, closeBracketsKeymap } from "@codemirror/autocomplete";
import { Vim, vim } from "@replit/codemirror-vim";
import {
  buildExtraKeymap,
  ensureClipboardSyncRegistered,
  ensureVimWriteRegistered,
  saveHandlers,
} from "./cm-shared";
import { buildEditorTheme } from "./editor-theme";
import type { HighlighterCore, ThemeRegistrationAny } from "shiki/core";
import {
  DARK_THEME,
  LIGHT_THEME,
  ensureLanguageLoaded,
  getHighlighter,
} from "@/lib/shiki-singleton";
import { languageFromPath } from "@/lib/language-from-path";
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
//   * Vim, theme, and read-only flags live behind Compartments —
//     toggling any of them reconfigures in place without re-mounting
//     the EditorView (cursor / scroll / undo all preserved). Soft-wrap
//     used to be Compartment-backed too but is now hardcoded on; long
//     lines were breaking the viewport and there was no UI toggle.

// `saveHandlers`, `ensureVimWriteRegistered`,
// `ensureClipboardSyncRegistered`, and `buildExtraKeymap` live in
// `./cm-shared` so the markdown editor can reuse them without
// duplicating the global one-shot registrations.

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
//
// Imported from ./editor-theme so the markdown editor can reuse it.


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
  /** Optional: fires on every doc change with the current buffer
   *  text. Used by wrapper editors that need a live mirror of the
   *  contents (e.g. the HTML viewer's preview iframe). Off by
   *  default — the doc-change updateListener already runs for the
   *  dirty-bit check, so opting in only adds a string materialize.
   *  Note: the HTML viewer pays this cost on every keystroke; if a
   *  caller cares about typing latency on huge buffers, throttle on
   *  their side before passing the callback in. */
  onChange?: (contents: string) => void;
}

export function CodeEditor({
  path,
  initialContent,
  theme,
  vimEnabled,
  gitModeEnabled = false,
  projectPath = null,
  sessionId = null,
  readOnly = false,
  onSave,
  onDirtyChange,
  onChange,
}: CodeEditorProps): React.ReactElement {
  const containerRef = React.useRef<HTMLDivElement | null>(null);
  const viewRef = React.useRef<EditorView | null>(null);
  const onSaveRef = React.useRef<() => Promise<void>>(() => Promise.resolve());
  const onDirtyChangeRef = React.useRef(onDirtyChange);
  const onChangeRef = React.useRef(onChange);
  const savedContentRef = React.useRef<string>(initialContent);
  const isDirtyRef = React.useRef<boolean>(false);
  const inImeRef = React.useRef<boolean>(false);

  // Compartments are created once per mount and held in refs so
  // their identity stays stable across React re-renders. Each prop
  // change (vim, theme, readOnly) reconfigures the matching
  // compartment without rebuilding the whole extension array.
  // Soft-wrap doesn't get a compartment — it's hardcoded on, so
  // `EditorView.lineWrapping` lands directly in the extension array.
  const vimCompartmentRef = React.useRef(new Compartment());
  const themeCompartmentRef = React.useRef(new Compartment());
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
    onChangeRef.current = onChange;
  }, [onChange]);

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
      const text = u.state.doc.toString();
      const dirty = text !== savedContentRef.current;
      if (dirty !== isDirtyRef.current) {
        isDirtyRef.current = dirty;
        onDirtyChangeRef.current(dirty);
      }
      // Optional live-content callback. Materializing the doc to a
      // string is the only added cost; we skip it entirely when no
      // caller subscribed.
      onChangeRef.current?.(text);
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
      // Soft-wrap is hardcoded on — long lines were breaking the
      // viewport on minified / generated files. No compartment, no
      // toggle. If we ever need an off switch we can reintroduce
      // both, but the user-facing toggle was already gone.
      EditorView.lineWrapping,
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

  // (Soft-wrap reactivity used to live here; now it's hardcoded on
  // in the initial extension array — no Compartment to reconfigure.)

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
      {/* `min-w-0` is required: this div hosts the CM6 EditorView,
          and as a flex child its default `min-width: auto` resolves to
          its content's intrinsic width — long markdown / code lines
          inside `.cm-content` would push this flex item wider than
          the panel, defeating `EditorView.lineWrapping` (the scroller
          would measure the inflated intrinsic width and wrap too far
          right, then the ancestor's `overflow-hidden` clips the
          right edge mid-word). `min-w-0` lets the flex child shrink
          to the panel width so CM measures the correct wrap budget. */}
      <div
        ref={containerRef}
        className="min-h-0 min-w-0 flex-1 overflow-hidden"
      />
    </div>
  );
}

// Default export so `React.lazy(() => import("./code-editor"))`
// picks up the component without the consumer having to unwrap a
// named export.
export default CodeEditor;
