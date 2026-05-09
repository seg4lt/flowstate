/**
 * Pure POSIX-style path helpers shared across the markdown editor.
 *
 * All functions assume forward-slash separators. The Tauri host on
 * macOS / Linux gives us POSIX paths verbatim; Windows callers should
 * normalise back-slashes before invoking these.
 *
 * Ported from zen-tools' `@zen-tools/types` paths module — every
 * function is a pure string transform with zero IPC dependency.
 */

/** Extract the basename (no extension) of a path. */
export function basenameNoExt(path: string): string {
  const last = path.split("/").pop() ?? path;
  const dot = last.lastIndexOf(".");
  return dot > 0 ? last.slice(0, dot) : last;
}

/** Full basename — used by labels for non-`.md` tabs (e.g.
 *  `Sketch.excalidraw.svg`) where stripping the extension would lose
 *  meaningful information. */
export function basename(path: string): string {
  return path.split("/").pop() ?? path;
}

/** Parent directory of an absolute path. Returns `""` when none. */
export function dirname(path: string): string {
  const idx = path.lastIndexOf("/");
  return idx > 0 ? path.slice(0, idx) : "";
}

/** Join two path segments with a single forward slash. Trims
 *  trailing/leading slashes so `joinPath("/foo/", "/bar")` produces
 *  `/foo/bar` rather than `/foo//bar`. */
export function joinPath(a: string, b: string): string {
  if (!a) return b;
  if (!b) return a;
  const left = a.endsWith("/") ? a.slice(0, -1) : a;
  const right = b.startsWith("/") ? b.slice(1) : b;
  return `${left}/${right}`;
}

/**
 * Compute the POSIX-style path of `target` relative to `from`
 * (a directory).  Used by the markdown link autocomplete so a
 * suggested file gets inserted as `path/to/file.md` rather than its
 * absolute path. Behaviour mirrors `path.posix.relative`.
 */
export function posixRelative(from: string, target: string): string {
  if (!from.startsWith("/") || !target.startsWith("/")) return target;
  const a = normalizePath(from).split("/").filter(Boolean);
  const b = normalizePath(target).split("/").filter(Boolean);
  let i = 0;
  while (i < a.length && i < b.length && a[i] === b[i]) i++;
  const ups = a.length - i;
  const rest = b.slice(i);
  if (ups === 0 && rest.length === 0) return ".";
  const parts: string[] = [];
  for (let k = 0; k < ups; k++) parts.push("..");
  parts.push(...rest);
  return parts.join("/");
}

/**
 * Collapse `.` / `..` segments and double-slashes in a POSIX-style
 * path. Keeps absolute / relative form intact.
 */
export function normalizePath(input: string): string {
  if (!input) return input;
  const isAbs = input.startsWith("/");
  const segs = input.split("/");
  const out: string[] = [];
  for (const seg of segs) {
    if (seg === "" || seg === ".") continue;
    if (seg === "..") {
      if (out.length > 0 && out[out.length - 1] !== "..") {
        out.pop();
      } else if (!isAbs) {
        out.push("..");
      }
      continue;
    }
    out.push(seg);
  }
  const joined = out.join("/");
  if (isAbs) return `/${joined}`;
  return joined || ".";
}

/** A loose slug: lowercase, ascii letters/digits/dash, collapsed. */
export function slugify(input: string): string {
  return input
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-+|-+$/g, "")
    .slice(0, 60);
}
