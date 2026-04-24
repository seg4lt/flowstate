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
import { DEFAULT_ENABLED_PROVIDERS, PROVIDER_KINDS } from "./providers";

// --- Config keys ---

const CONFIG_KEY_EFFORT = "defaults.effort";
const CONFIG_KEY_PERMISSION_MODE = "defaults.permission_mode";
const CONFIG_KEY_MODEL_PREFIX = "defaults.model.";
const CONFIG_KEY_PROVIDER_ENABLED_PREFIX = "provider.enabled.";
const CONFIG_KEY_DEFAULT_PROVIDER = "defaults.provider";
const CONFIG_KEY_STRICT_PLAN_MODE = "defaults.strict_plan_mode";

// --- Provider-enabled defaults ---

/** Providers enabled out of the box. Everything else starts disabled. */
export { DEFAULT_ENABLED_PROVIDERS };

/** Back-compat alias. Prefer `PROVIDER_KINDS` from `@/lib/providers`. */
export const ALL_PROVIDER_KINDS: readonly ProviderKind[] = PROVIDER_KINDS;

// --- Validation helpers ---

const VALID_EFFORTS: ReadonlySet<string> = new Set<ReasoningEffort>([
  "max",
  "xhigh",
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
  "auto",
]);

const VALID_PROVIDER_KINDS: ReadonlySet<string> = new Set<ProviderKind>(
  ALL_PROVIDER_KINDS,
);

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

// --- Default provider ---

/** The provider used by default when starting new threads (e.g.
 *  from worktree creation) without an explicit provider pick. */
export const DEFAULT_PROVIDER: ProviderKind = "claude";

export async function readDefaultProvider(): Promise<ProviderKind | null> {
  try {
    const raw = await getUserConfig(CONFIG_KEY_DEFAULT_PROVIDER);
    if (raw !== null && VALID_PROVIDER_KINDS.has(raw)) {
      return raw as ProviderKind;
    }
    return null;
  } catch {
    return null;
  }
}

export async function writeDefaultProvider(
  value: ProviderKind,
): Promise<void> {
  try {
    await setUserConfig(CONFIG_KEY_DEFAULT_PROVIDER, value);
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

// --- Provider enabled/disabled (app-level) ---

export async function readProviderEnabled(
  provider: ProviderKind,
): Promise<boolean | null> {
  try {
    const raw = await getUserConfig(
      CONFIG_KEY_PROVIDER_ENABLED_PREFIX + provider,
    );
    if (raw === "true") return true;
    if (raw === "false") return false;
    return null;
  } catch {
    return null;
  }
}

export async function writeProviderEnabled(
  provider: ProviderKind,
  enabled: boolean,
): Promise<void> {
  try {
    await setUserConfig(
      CONFIG_KEY_PROVIDER_ENABLED_PREFIX + provider,
      String(enabled),
    );
  } catch {
    /* storage may be unavailable */
  }
}

// --- Strict plan mode ---

/**
 * When `true`, the chat view auto-denies any permission request for
 * a mutating tool (see `PLAN_MODE_MUTATING_TOOLS`) while the session
 * is in plan mode, preventing the user from accidentally clicking
 * Allow on an Edit/Write/Bash prompt that would exit plan mode
 * early. Defaults to `false` — the SDK's own plan-mode gate is the
 * baseline; this is an opt-in hardening.
 */
export async function readStrictPlanMode(): Promise<boolean> {
  try {
    const raw = await getUserConfig(CONFIG_KEY_STRICT_PLAN_MODE);
    if (raw === "true") return true;
    return false;
  } catch {
    return false;
  }
}

export async function writeStrictPlanMode(enabled: boolean): Promise<void> {
  try {
    await setUserConfig(CONFIG_KEY_STRICT_PLAN_MODE, String(enabled));
  } catch {
    /* storage may be unavailable */
  }
}

/** Read enabled state for every provider. Unset keys fall back to
 *  `DEFAULT_ENABLED_PROVIDERS` (claude + github_copilot on, rest off). */
export async function readAllProviderEnabled(): Promise<
  Map<ProviderKind, boolean>
> {
  const results = await Promise.all(
    ALL_PROVIDER_KINDS.map(async (kind) => {
      const stored = await readProviderEnabled(kind);
      const value = stored ?? DEFAULT_ENABLED_PROVIDERS.has(kind);
      return [kind, value] as const;
    }),
  );
  return new Map(results);
}
