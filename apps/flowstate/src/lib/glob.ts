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

export function globToRegex(glob: string): RegExp {
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
  return new RegExp("^" + re + "$");
}

// A pattern is either a glob (compiled to a regex anchored to
// the full path) or a plain substring (case-insensitive). We use
// the substring fallback so users can still type half a filename
// without having to remember that file pickers want `**/foo*`.
export type FilePattern =
  | { kind: "glob"; re: RegExp }
  | { kind: "substring"; needle: string };

export function parsePatterns(query: string): FilePattern[] {
  return query
    .split(",")
    .map((p) => p.trim())
    .filter(Boolean)
    .map((p) =>
      /[*?]/.test(p)
        ? ({ kind: "glob" as const, re: globToRegex(p) })
        : ({ kind: "substring" as const, needle: p.toLowerCase() }),
    );
}

// True if `path` matches ANY of the supplied patterns. With an
// empty pattern list everything passes — callers should bail out
// before calling this when the filter shouldn't apply.
export function matchesAnyPattern(
  path: string,
  patterns: FilePattern[],
): boolean {
  if (patterns.length === 0) return true;
  const lower = path.toLowerCase();
  for (const p of patterns) {
    if (p.kind === "glob") {
      if (p.re.test(path)) return true;
    } else if (lower.includes(p.needle)) {
      return true;
    }
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
