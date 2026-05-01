// Shared utilities for git worktree path derivation. Extracted from
// branch-switcher.tsx so both the BranchSwitcher popover and the
// project-home CreateWorktreeDialog can reuse the same logic.

// Path-separator helpers — Tauri returns Windows paths with `\`
// from the file picker, git's `--porcelain` output uses `\` on
// Windows too, but our codebase historically only handled `/`.
// Treat both as separators everywhere.

/** Index of the last path separator (`/` or `\`) in `p`, or -1. */
function lastSeparatorIndex(p: string): number {
  const fwd = p.lastIndexOf("/");
  const back = p.lastIndexOf("\\");
  return fwd > back ? fwd : back;
}

/** Strip a single trailing separator (`/` or `\`). */
function stripTrailingSeparator(p: string): string {
  if (p.length === 0) return p;
  const last = p.charCodeAt(p.length - 1);
  return last === 47 /* / */ || last === 92 /* \\ */ ? p.slice(0, -1) : p;
}

/** Strip trailing slash so paths coming from git porcelain, the
 *  file picker, and our own state all compare equal. Used by every
 *  "is this worktree the main project?" and "do we already have a
 *  project for this worktree path?" check — without this the sidebar
 *  can double-create projects and fail to group worktree threads
 *  under their parent project. Tolerates both `/` and `\` so Windows
 *  paths from Tauri's file picker compare equal to git's porcelain
 *  output. */
export function normPath(p: string | null | undefined): string {
  if (!p) return "";
  return stripTrailingSeparator(p);
}

/** True when two filesystem paths refer to the same folder, tolerating
 *  trailing-slash differences. */
export function samePath(
  a: string | null | undefined,
  b: string | null | undefined,
): boolean {
  return normPath(a) === normPath(b);
}

// Derive the on-disk folder path for a new worktree. Convention:
// `<base>/<project-name>-worktrees/<project-name>-<sanitized>`
// where `<base>` is either the user's configured worktree base path
// from Settings or — when unset — `<dirname(parent-project-path)>/worktrees`,
// `<project-name>` is the basename of the main project path, and
// `<sanitized>` is the typed branch name lowercased with
// non-alphanumeric characters collapsed to hyphens.
//
// On Windows the inherited separator (`\`) is preserved so the path
// we hand to `git worktree add` doesn't end up half-`/` half-`\`,
// which some shell quoting layers mishandle.
export function deriveWorktreePath(
  parentProjectPath: string,
  name: string,
  configuredBase: string | null,
): string {
  const projectName = basename(parentProjectPath);
  const sanitized = name
    .toLowerCase()
    .replace(/[^a-z0-9._-]/g, "-")
    .replace(/-+/g, "-")
    .replace(/^-|-$/g, "");
  // Pick a separator: if either input contains a backslash, we're
  // on Windows-style paths and should keep using `\`. Otherwise
  // default to `/` (Unix, and also valid as a fallback since git
  // accepts forward slashes on Windows too).
  const sep =
    parentProjectPath.includes("\\") ||
    (configuredBase && configuredBase.includes("\\"))
      ? "\\"
      : "/";
  const base =
    configuredBase && configuredBase.length > 0
      ? configuredBase
      : `${dirname(parentProjectPath)}${sep}worktrees`;
  return `${base}${sep}${projectName}-worktrees${sep}${projectName}-${sanitized}`;
}

export function basename(p: string): string {
  const stripped = stripTrailingSeparator(p);
  const idx = lastSeparatorIndex(stripped);
  return idx >= 0 ? stripped.slice(idx + 1) : stripped;
}

export function dirname(p: string): string {
  const stripped = stripTrailingSeparator(p);
  const idx = lastSeparatorIndex(stripped);
  return idx >= 0 ? stripped.slice(0, idx) : ".";
}
