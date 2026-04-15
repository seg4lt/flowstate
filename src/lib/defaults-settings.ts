// Flowstate-app-level defaults: the user's preferred effort level,
// permission mode, and per-provider model for new sessions/turns.
// Stored in `user_config.sqlite` via the same key/value plumbing
// that pool size and worktree base path use — app-level tunables
// the agent SDK's daemon database has no reason to see.
//
// All readers validate the stored value against the known enum
// members and return `null` when missing or invalid, so callers
// can fall back to their own hardcoded defaults.

import { getUserConfig, setUserConfig } from "./api";
import type { PermissionMode, ProviderKind, ReasoningEffort } from "./types";

// --- Config keys ---

const CONFIG_KEY_EFFORT = "defaults.effort";
const CONFIG_KEY_PERMISSION_MODE = "defaults.permission_mode";
const CONFIG_KEY_MODEL_PREFIX = "defaults.model.";

// --- Validation helpers ---

const VALID_EFFORTS: ReadonlySet<string> = new Set<ReasoningEffort>([
  "high",
  "medium",
  "low",
  "minimal",
]);

const VALID_PERMISSION_MODES: ReadonlySet<string> = new Set<PermissionMode>([
  "default",
  "accept_edits",
  "plan",
  "bypass",
]);

// --- Effort ---

export async function readDefaultEffort(): Promise<ReasoningEffort | null> {
  try {
    const raw = await getUserConfig(CONFIG_KEY_EFFORT);
    if (raw !== null && VALID_EFFORTS.has(raw)) {
      return raw as ReasoningEffort;
    }
    return null;
  } catch {
    return null;
  }
}

export async function writeDefaultEffort(
  value: ReasoningEffort,
): Promise<void> {
  try {
    await setUserConfig(CONFIG_KEY_EFFORT, value);
  } catch {
    /* storage may be unavailable */
  }
}

// --- Permission mode ---

export async function readDefaultPermissionMode(): Promise<PermissionMode | null> {
  try {
    const raw = await getUserConfig(CONFIG_KEY_PERMISSION_MODE);
    if (raw !== null && VALID_PERMISSION_MODES.has(raw)) {
      return raw as PermissionMode;
    }
    return null;
  } catch {
    return null;
  }
}

export async function writeDefaultPermissionMode(
  value: PermissionMode,
): Promise<void> {
  try {
    await setUserConfig(CONFIG_KEY_PERMISSION_MODE, value);
  } catch {
    /* storage may be unavailable */
  }
}

// --- Per-provider default model ---

export async function readDefaultModel(
  provider: ProviderKind,
): Promise<string | null> {
  try {
    const raw = await getUserConfig(CONFIG_KEY_MODEL_PREFIX + provider);
    if (raw === null) return null;
    const trimmed = raw.trim();
    return trimmed.length > 0 ? trimmed : null;
  } catch {
    return null;
  }
}

export async function writeDefaultModel(
  provider: ProviderKind,
  model: string,
): Promise<void> {
  try {
    await setUserConfig(CONFIG_KEY_MODEL_PREFIX + provider, model.trim());
  } catch {
    /* storage may be unavailable */
  }
}
