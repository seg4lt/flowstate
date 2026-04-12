// Shared utilities for git worktree path derivation. Extracted from
// branch-switcher.tsx so both the BranchSwitcher popover and the
// project-home CreateWorktreeDialog can reuse the same logic.

// Derive the on-disk folder path for a new worktree. Convention:
// `<base>/<project-name>-worktrees/<project-name>-<sanitized>`
// where `<base>` is either the user's configured worktree base path
// from Settings or — when unset — `<dirname(parent-project-path)>/worktrees`,
// `<project-name>` is the basename of the main project path, and
// `<sanitized>` is the typed branch name lowercased with
// non-alphanumeric characters collapsed to hyphens.
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
  const base =
    configuredBase && configuredBase.length > 0
      ? configuredBase
      : `${dirname(parentProjectPath)}/worktrees`;
  return `${base}/${projectName}-worktrees/${projectName}-${sanitized}`;
}

export function basename(p: string): string {
  const stripped = p.endsWith("/") ? p.slice(0, -1) : p;
  const idx = stripped.lastIndexOf("/");
  return idx >= 0 ? stripped.slice(idx + 1) : stripped;
}

export function dirname(p: string): string {
  const stripped = p.endsWith("/") ? p.slice(0, -1) : p;
  const idx = stripped.lastIndexOf("/");
  return idx >= 0 ? stripped.slice(0, idx) : ".";
}
