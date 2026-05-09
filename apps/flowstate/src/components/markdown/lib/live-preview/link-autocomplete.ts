/**
 * Markdown-link path autocomplete — fires when the cursor sits inside
 * the `(…)` of a `[label](query)` (or `![alt](query)` image) and the
 * user is partway through typing the path.
 *
 * Result population: filters the project file index (already loaded
 * via React Query — no extra IPC) for paths whose basename or relative
 * path matches the partial query. flowstate is single-project, so we
 * don't need fff-search's multi-vault ranking machinery — a basic
 * substring rank biased toward basename hits is sufficient.
 *
 * Implementation note: this exports a bare `CompletionSource` — the
 * live-preview `index.ts` merges it under a single `autocompletion()`
 * call.
 */

import {
  type Completion,
  type CompletionContext,
  type CompletionResult,
  type CompletionSource,
} from "@codemirror/autocomplete";
import { basename, posixRelative } from "../tauri";

const LINK_SUGGESTION_LIMIT = 30;

const LINKABLE_RE =
  /\.(md|markdown|mdx|mdown|mkd|png|jpg|jpeg|gif|webp|svg|avif)$/i;

/**
 * Build the link-path completion source.
 *
 * @param getDocDir   Returns the absolute directory of the open
 *                    document — used to compute relative paths.
 * @param getProjectPath Returns the absolute project root, or `""`
 *                    when no project is open.
 * @param getFiles    Returns every project-relative path the picker
 *                    knows about (from `projectFilesQueryOptions`).
 */
export function linkAutocompleteSource(
  getDocDir: () => string,
  getProjectPath: () => string,
  getFiles: () => string[],
): CompletionSource {
  return (ctx: CompletionContext): CompletionResult | null => {
    const line = ctx.state.doc.lineAt(ctx.pos);
    const before = ctx.state.doc.sliceString(line.from, ctx.pos);
    const open = before.lastIndexOf("](");
    if (open === -1) return null;
    const between = before.slice(open + 2);
    if (between.includes(")")) return null;
    if (between.length === 0 && !ctx.explicit) return null;

    const from = line.from + open + 2;
    const projectPath = getProjectPath();
    const docDir = getDocDir();
    const files = getFiles();
    if (files.length === 0) return null;

    const query = between.toLowerCase();

    // Two-pass rank: basename matches first (most common case),
    // then path-segment matches.
    const baseHits: { rel: string; abs: string; score: number }[] = [];
    const pathHits: { rel: string; abs: string; score: number }[] = [];
    for (const projectRel of files) {
      if (!LINKABLE_RE.test(projectRel)) continue;
      const base = basename(projectRel).toLowerCase();
      const lower = projectRel.toLowerCase();
      const baseIdx = base.indexOf(query);
      const pathIdx = lower.indexOf(query);
      if (baseIdx === -1 && pathIdx === -1 && query.length > 0) continue;
      const abs = projectPath ? `${projectPath}/${projectRel}` : projectRel;
      const rel = docDir ? posixRelative(docDir, abs) : projectRel;
      // Prefer prefix matches, then earlier offsets.
      const score =
        baseIdx === 0
          ? 0
          : baseIdx > 0
            ? baseIdx
            : pathIdx >= 0
              ? 1000 + pathIdx
              : 9999;
      (baseIdx >= 0 ? baseHits : pathHits).push({ rel, abs, score });
    }
    const ranked = [
      ...baseHits.sort((a, b) => a.score - b.score),
      ...pathHits.sort((a, b) => a.score - b.score),
    ].slice(0, LINK_SUGGESTION_LIMIT);
    if (ranked.length === 0) return null;

    const options: Completion[] = ranked.map(({ rel, abs }) => {
      const base = basename(abs);
      const type = abs.toLowerCase().endsWith(".svg") ? "interface" : "file";
      return {
        label: base,
        ...(rel !== base ? { detail: rel } : {}),
        apply: `${rel})`,
        type,
      };
    });

    return {
      from,
      to: ctx.pos,
      options,
      validFor: /^[^\n)\]]*$/,
    };
  };
}
