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
