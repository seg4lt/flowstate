// Tiny pure-TS glob matcher for the editor view's file picker
// (and as the source of include/exclude string parsing for the
// content-search advanced row). Supports the small subset of
// glob syntax users actually want from a file picker:
//
//   `*`   matches any character except `/`
//   `**`  matches any sequence including `/`
//   `?`   matches a single character (not `/`)
//
// Other regex metacharacters are escaped so paths with literal
// dots, plus signs, parens, etc. work as expected.
//
// We deliberately don't pull in `picomatch` / `minimatch` — those
// are Node-oriented and would cost a few KB for what's effectively
// 30 lines of regex translation.

export interface GlobOptions {
  /** When true (default), the regex is wrapped in `^...$` so it
   *  must match the entire string. Use `false` for "this glob may
   *  appear anywhere in the path" — that's what the file picker's
   *  folder filter wants so users can type `src/**` and have it
   *  match `apps/flowstate/src/components/...` rather than only
   *  paths that begin with `src/`. */
  anchored?: boolean;
}

export function globToRegex(glob: string, options: GlobOptions = {}): RegExp {
  const anchored = options.anchored ?? true;
  let re = "";
  for (let i = 0; i < glob.length; i++) {
    const c = glob[i];
    if (c === "*") {
      if (glob[i + 1] === "*") {
        // `**` matches anything including slashes. Eat the
        // optional `/` immediately after `**/` so the pattern
        // `src/**/foo` matches both `src/foo` and `src/a/b/foo`.
        re += ".*";
        i++;
        if (glob[i + 1] === "/") i++;
      } else {
        // single `*` only crosses one path segment.
        re += "[^/]*";
      }
    } else if (c === "?") {
      re += "[^/]";
    } else if ("\\^$.|?*+(){}[]".includes(c)) {
      re += "\\" + c;
    } else {
      re += c;
    }
  }
  return new RegExp(anchored ? "^" + re + "$" : re);
}

// A pattern is either a glob (compiled to a regex) or a plain
// substring (case-insensitive). We use the substring fallback so
// users can still type half a filename without having to remember
// that file pickers want `**/foo*`.
export type FilePattern =
  | { kind: "glob"; re: RegExp }
  | { kind: "substring"; needle: string };

function hasGlobChars(s: string): boolean {
  return /[*?]/.test(s);
}

/** Parse a single token as a pattern matched against the file's
 *  basename. Globs are anchored — `tabs.*` matches a basename of
 *  `tabs.ts` exactly, not basenames *containing* "tabs.". */
function parseFilePattern(p: string): FilePattern {
  return hasGlobChars(p)
    ? { kind: "glob", re: globToRegex(p) }
    : { kind: "substring", needle: p.toLowerCase() };
}

/** Parse a single token as a pattern matched against a path
 *  fragment (folder filter or full path). Globs are UN-anchored
 *  here — typing `src/**` should match anywhere in the path the
 *  user is filtering, not require the path to start with `src/`. */
function parsePathPattern(p: string): FilePattern {
  return hasGlobChars(p)
    ? { kind: "glob", re: globToRegex(p, { anchored: false }) }
    : { kind: "substring", needle: p.toLowerCase() };
}

function testPattern(text: string, pattern: FilePattern): boolean {
  if (pattern.kind === "glob") return pattern.re.test(text);
  return text.toLowerCase().includes(pattern.needle);
}

// ─── picker query: folder + filename split on space ───────────────
//
// A picker query is a comma-separated list of alternatives (OR).
// Each alternative is either:
//
//   * `<token>` — single pattern, matched against the full path.
//     Same as the original behaviour from before this layer existed
//     so users with muscle memory for "type half the path" still
//     work.
//
//   * `<folder> <space> <filename>` — Zed/IntelliJ-style two-part
//     filter. `<folder>` is matched against the path's directory
//     portion (everything before the last `/`); `<filename>` is
//     matched against the basename. Both must match (AND).
//     Example: `src tabs.ts` finds files named `tabs.ts` whose
//     directory contains `src`. Glob in either part works:
//     `**/code Header.tsx`, `lib/api *.ts`, etc.
//
// Splitting on the FIRST space — extra spaces inside the filename
// would be unusual but the user opts into that intent by typing
// the space, so we treat the rest of the token as the filename
// pattern verbatim.

export interface ScopedQuery {
  /** When set, the path's directory portion must match this. */
  folder?: FilePattern;
  /** Always set when the query is non-empty. Matched against the
   *  basename when `folder` is set, against the full path otherwise. */
  file: FilePattern;
}

export interface PickerQuery {
  /** OR-joined alternatives. Empty when the input was just commas
   *  / whitespace; callers should treat that as "no filter". */
  alternatives: ScopedQuery[];
}

export function parsePickerQuery(query: string): PickerQuery {
  const alternatives: ScopedQuery[] = [];
  for (const raw of query.split(",")) {
    const chunk = raw.trim();
    if (!chunk) continue;
    const spaceIdx = chunk.indexOf(" ");
    if (spaceIdx === -1) {
      alternatives.push({ file: parsePathPattern(chunk) });
      continue;
    }
    const folderPart = chunk.slice(0, spaceIdx).trim();
    const filePart = chunk.slice(spaceIdx + 1).trim();
    if (!filePart) {
      alternatives.push({ file: parsePathPattern(folderPart) });
    } else if (!folderPart) {
      alternatives.push({ file: parsePathPattern(filePart) });
    } else {
      alternatives.push({
        folder: parsePathPattern(folderPart),
        file: parseFilePattern(filePart),
      });
    }
  }
  return { alternatives };
}

export function matchesPickerQuery(
  path: string,
  query: PickerQuery,
): boolean {
  if (query.alternatives.length === 0) return true;
  const slash = path.lastIndexOf("/");
  const basename = slash >= 0 ? path.slice(slash + 1) : path;
  const dir = slash >= 0 ? path.slice(0, slash) : "";
  for (const alt of query.alternatives) {
    if (alt.folder) {
      if (!testPattern(dir, alt.folder)) continue;
      if (!testPattern(basename, alt.file)) continue;
      return true;
    }
    if (testPattern(path, alt.file)) return true;
  }
  return false;
}

// Just split + trim + filter empty — preserves raw glob strings
// for sending over the bridge to the rust side. Use this for the
// content-search include/exclude inputs where the rust
// OverrideBuilder does the actual matching.
export function splitGlobList(value: string): string[] {
  return value
    .split(",")
    .map((p) => p.trim())
    .filter(Boolean);
}
