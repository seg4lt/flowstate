/**
 * Chrome-only CodeMirror 6 theme used by both `CodeEditor` (raw code)
 * and `MarkdownEditor` (rich live-preview).
 *
 * Sets background, gutter, caret, selection, vim status panel,
 * search panel — everything except token colours. Token colouring
 * is editor-specific: `CodeEditor` layers Shiki on top via decoration
 * styles; `MarkdownEditor` uses its own `markdownHighlightStyle`.
 *
 * Extracted from `code-editor.tsx` so the markdown editor doesn't
 * need to import code-editor's heavyweight bundle (Shiki + git diff
 * + comment overlay) just to share theme chrome.
 */

import type { Extension } from "@codemirror/state";
import { EditorView } from "@codemirror/view";
import { DARK_THEME, LIGHT_THEME } from "@/lib/shiki-singleton";

export function buildEditorTheme(theme: "light" | "dark"): Extension {
  const t = theme === "dark" ? DARK_THEME : LIGHT_THEME;
  const bg =
    (t as { bg?: string }).bg ?? (theme === "dark" ? "#0b0b0c" : "#ffffff");
  const fg =
    (t as { fg?: string }).fg ?? (theme === "dark" ? "#e5e5e5" : "#1f1f1f");
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
      ".cm-vim-panel > span:last-child": { opacity: "0.7" },
      ".cm-vim-panel input": {
        border: "none",
        outline: "none",
        backgroundColor: "transparent",
        color: fg,
        flex: "1",
        fontFamily: "inherit",
        fontSize: "inherit",
      },
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
      ".cm-panels": { backgroundColor: panelBg, color: fg },
      ".cm-panels-top": { borderBottom: `1px solid ${panelBorder}` },
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
      ".cm-searchMatch": {
        backgroundColor:
          theme === "dark"
            ? "rgba(250, 204, 21, 0.22)"
            : "rgba(250, 204, 21, 0.4)",
        outline:
          theme === "dark"
            ? "1px solid rgba(250, 204, 21, 0.55)"
            : "1px solid rgba(202, 138, 4, 0.55)",
      },
      ".cm-searchMatch-selected": {
        backgroundColor:
          theme === "dark"
            ? "rgba(251, 146, 60, 0.45)"
            : "rgba(251, 146, 60, 0.55)",
        outline:
          theme === "dark"
            ? "1px solid rgba(251, 146, 60, 1)"
            : "1px solid rgba(194, 65, 12, 1)",
      },
    },
    { dark: theme === "dark" },
  );
}
