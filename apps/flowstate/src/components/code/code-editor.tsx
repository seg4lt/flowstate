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
  drawSelection,
  highlightActiveLine,
  keymap,
  lineNumbers,
} from "@codemirror/view";
import {
  bracketMatching,
  codeFolding,
  foldGutter,
  foldKeymap,
  indentOnInput,
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
          run();
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
      // vim() must be first so its keymap takes precedence in NORMAL
      // mode. Compartment lets us flip vim on/off in place.
      vimCompartmentRef.current.of(vimEnabled ? vim() : []),
      lineNumbers(),
      foldGutter(),
      codeFolding(),
      highlightActiveLine(),
      highlightSelectionMatches(),
      drawSelection(),
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
      blurAutoSave,
      updateListener,
      readOnlyCompartmentRef.current.of(
        EditorState.readOnly.of(readOnly || overlong),
      ),
      wrapCompartmentRef.current.of(softWrap ? EditorView.lineWrapping : []),
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
      effects: vimCompartmentRef.current.reconfigure(vimEnabled ? vim() : []),
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
