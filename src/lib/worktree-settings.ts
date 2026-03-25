// Flowzen-app-level setting: the base directory under which new
// git worktrees are created. Stored in `user_config.sqlite` via the
// same key/value plumbing the highlighter pool size uses — this is
// an app-level tunable, not something the agent SDK's daemon
// database has any reason to see. See
// `rs-agent-sdk/crates/core/persistence/CLAUDE.md` for the boundary.
//
// The default derivation when this setting is empty / null:
//   `<dirname(parent-project-path)>/worktrees/<project-name>-worktrees/<project-name>-<sanitized>`
// When a user sets a custom base (e.g. `/Users/me/Code/worktrees`)
// the derivation becomes:
//   `<base>/<project-name>-worktrees/<project-name>-<sanitized>`
// and the default `<dirname>/worktrees` prefix is skipped.

import { getUserConfig, setUserConfig } from "./api";

export const WORKTREE_BASE_PATH_CONFIG_KEY = "worktree.base_path";

export async function readWorktreeBasePath(): Promise<string | null> {
  try {
    const raw = await getUserConfig(WORKTREE_BASE_PATH_CONFIG_KEY);
    if (raw === null) return null;
    const trimmed = raw.trim();
    return trimmed.length > 0 ? trimmed : null;
  } catch {
    return null;
  }
}

export async function writeWorktreeBasePath(path: string): Promise<void> {
  try {
    await setUserConfig(WORKTREE_BASE_PATH_CONFIG_KEY, path.trim());
  } catch {
    /* storage may be unavailable */
  }
}
