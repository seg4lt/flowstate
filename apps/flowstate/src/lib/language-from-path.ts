// Map a file extension (without the dot) to a Shiki language name.
// Only contains entries where the extension differs from Shiki's
// canonical name; everything else falls through to the raw ext.
//
// Used by both the file-viewer code editor and the chat tool-call
// renderers (e.g. the Write tool's content preview), so a path like
// `src/foo.tsx` lights up the same way in both surfaces.
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
  // `md` / `markdown` / `mdx` are intentionally NOT mapped here —
  // these files render in the rich live-preview MarkdownEditor (see
  // `components/markdown/markdown-editor.tsx`), which has its own
  // `@codemirror/lang-markdown` parser + custom decoration palette.
  // Letting Shiki tokenise on top would fight the live-preview
  // decorations. Markdown paths route around `CodeEditor` entirely
  // via the `getEditorKind`-based switch in `code-view.tsx`.
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

/**
 * Resolve a Shiki language name from a file path. Returns "text" when
 * no extension is present so callers can hand the result straight to
 * Shiki without an extra null check — Shiki's plain-text fallback
 * kicks in for anything it doesn't have a grammar for.
 */
export function languageFromPath(path: string): string {
  const dot = path.lastIndexOf(".");
  if (dot === -1) return "text";
  const ext = path.slice(dot + 1).toLowerCase();
  return EXT_TO_LANG[ext] ?? ext;
}

// ── Editor-kind classification ─────────────────────────────────────
//
// `code-view.tsx` uses `getEditorKind(path)` to decide which lazy-
// loaded component mounts for a given file:
//   * `markdown`   → the rich `MarkdownEditor` (live preview, mermaid,
//                    excalidraw embed, image paste, link follow).
//   * `excalidraw` → the standalone `ExcalidrawEditor` (canvas pane,
//                    binary save for `.excalidraw.png`).
//   * `code`       → the existing `CodeEditor` (Shiki + git diff +
//                    comment overlay) — the default for everything
//                    else.
// Keeping these in one place means file-tree rename / new-file flows
// can ask "what kind of editor will this open in?" without each caller
// re-implementing the extension allow-list.

/** Markdown extensions the live-preview editor opens. */
export const MARKDOWN_EXTS: ReadonlySet<string> = new Set([
  "md",
  "markdown",
  "mdx",
  "mdown",
  "mkd",
]);

/** True when `path` ends with a markdown extension we treat as a
 *  rich-edit document (live preview etc.). */
export function isMarkdownPath(path: string): boolean {
  const dot = path.lastIndexOf(".");
  if (dot === -1) return false;
  return MARKDOWN_EXTS.has(path.slice(dot + 1).toLowerCase());
}

/** True when `path` is an Excalidraw drawing — either the SVG or PNG
 *  flavour (both formats embed the scene in a metadata block excalidraw
 *  can round-trip). Order matters: `.excalidraw.svg` is checked before
 *  the generic image check so the drawing pane (not the static image
 *  viewer) opens for these. */
export function isExcalidrawPath(path: string): boolean {
  const lower = path.toLowerCase();
  return (
    lower.endsWith(".excalidraw.svg") || lower.endsWith(".excalidraw.png")
  );
}

export type EditorKind = "markdown" | "excalidraw" | "code";

/** Pick the editor component for a given file path. Excalidraw beats
 *  markdown beats code; everything else is `code`. */
export function getEditorKind(path: string): EditorKind {
  if (isExcalidrawPath(path)) return "excalidraw";
  if (isMarkdownPath(path)) return "markdown";
  return "code";
}
