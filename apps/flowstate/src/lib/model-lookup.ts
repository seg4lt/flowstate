import type { ProviderKind, ProviderModel, ProviderStatus } from "@/lib/types";

export interface ResolvedModelDisplay {
  /** The raw provider-level model id, exactly as sent to the wire. */
  rawId: string;
  /** Human-readable display label for this model. Falls back to the
   *  raw id when the provider hasn't catalogued this specific
   *  pinned variant (common when the SDK resolves an alias to a
   *  date-stamped version we don't list). */
  label: string;
  /** Display label for the provider ("Claude", "Codex", etc.). */
  providerLabel: string;
  /** Full ProviderModel entry when one matches exactly — useful if
   *  callers also want capability fields. Undefined when unmatched. */
  entry?: ProviderModel;
}

/**
 * Strip an Anthropic-style `-YYYYMMDD` date stamp from the end of a
 * model id so aliased and pinned forms compare equal. Examples:
 *
 *   "claude-sonnet-4-5-20250929" → "claude-sonnet-4-5"
 *   "claude-sonnet-4-5"          → "claude-sonnet-4-5" (passthrough)
 *
 * Used only as a fallback for catalog lookups — the raw id is still
 * what we send over the wire.
 */
function stripDateStamp(id: string): string {
  return id.replace(/-\d{8}$/, "");
}

/**
 * Family-heuristic fallback for when neither exact nor date-stripped
 * catalog lookups find an entry. Needed because the Claude Agent SDK's
 * `q.supportedModels()` returns *branded* aliases (`"default"`,
 * `"sonnet"`, `"sonnet[1m]"`, `"haiku"`) — not the full zoo of id
 * forms the SDK will accept and emit on `model_resolved`. When a
 * session's model ends up as something like `"claude-opus-4-7[1m]"`
 * (an accepted alias that isn't in `supportedModels()`), we'd
 * otherwise drop to `entry = undefined` and lose every capability
 * flag — which is how X-High/Max vanish and Adaptive grays out for
 * sessions whose cache didn't survive (reload, pre-cache session,
 * etc.).
 *
 * The catalog's naming convention encodes the family:
 *   - `default`    → Opus family (per SDK description "Opus 4.7 …")
 *   - `sonnet`     → Sonnet family
 *   - `sonnet[1m]` → Sonnet 1M-context variant
 *   - `haiku`      → Haiku family
 *
 * Kept in this file (not `lib/pricing.ts`) because pricing matches by
 * family label for cost math, whereas this needs a specific catalog
 * *entry* so callers can read its capability flags. Ordering in each
 * candidate list matters: we prefer the `[1m]`-context variant when
 * the target id carries `[1m]`, otherwise the non-1M form.
 *
 * If the SDK rebrands `default` to a different family in a future
 * release, update the `opus` list here.
 */
function familyFallback(
  modelId: string,
  models: readonly ProviderModel[],
): ProviderModel | undefined {
  const lower = modelId.toLowerCase();
  const has1m = lower.includes("[1m]");
  // Preference-ordered candidate alias lists per family. The first
  // entry that actually exists in the catalog wins.
  let candidates: string[] | null = null;
  if (lower.includes("opus")) {
    // No `[1m]`-specific opus alias today — `default` is already the
    // 1M-context Opus variant per the SDK description.
    candidates = ["default"];
  } else if (lower.includes("sonnet")) {
    candidates = has1m
      ? ["sonnet[1m]", "sonnet"]
      : ["sonnet", "sonnet[1m]"];
  } else if (lower.includes("haiku")) {
    candidates = ["haiku"];
  }
  if (!candidates) return undefined;
  for (const v of candidates) {
    const match = models.find((m) => m.value === v);
    if (match) return match;
  }
  return undefined;
}

/**
 * Resolve a raw provider model id into the display strings we want to
 * render. Both the model label and provider label degrade gracefully:
 * when a pinned variant isn't in the provider's catalog we still
 * display the raw id rather than an empty string.
 *
 * Centralising this here keeps `agent-message`, `message-model-info`,
 * and the subagent header from each re-implementing the same lookup.
 *
 * Lookup order:
 *   1. Exact id match — fast path when the catalog entry and the
 *      resolved session id line up character-for-character.
 *   2. Date-stamp-normalised match — handles catalog/resolved date
 *      drift (`claude-sonnet-4-5-20250929` vs
 *      `claude-sonnet-4-5-20251015`) and alias/pinned mixing
 *      (`claude-sonnet-4-5` vs `claude-sonnet-4-5-20250929`).
 *   3. Family-heuristic fallback — for id forms the SDK accepts but
 *      doesn't enumerate in `q.supportedModels()` (`"default"`,
 *      `"sonnet"`, `"sonnet[1m]"`, `"haiku"` are the only branded
 *      aliases in the catalog; pinned/expanded forms like
 *      `"claude-opus-4-7[1m]"` fall through the first two lookups).
 *      See `familyFallback` for the heuristic.
 *
 * Without these fallbacks the caller gets `entry = undefined` and
 * loses every capability flag (`supportedEffortLevels`,
 * `supportsAdaptiveThinking`) — which is how X-High/Max vanished
 * from the effort menu and Adaptive grayed out for sessions whose
 * `session.model` drifted from the catalog.
 */
export function resolveModelDisplay(
  modelId: string | undefined,
  providerKind: ProviderKind | undefined,
  providers: ProviderStatus[],
): ResolvedModelDisplay {
  const provider = providerKind
    ? providers.find((p) => p.kind === providerKind)
    : undefined;

  let entry: ProviderModel | undefined;
  if (modelId && provider) {
    entry = provider.models.find((m) => m.value === modelId);
    if (!entry) {
      const normalizedTarget = stripDateStamp(modelId);
      entry = provider.models.find(
        (m) => stripDateStamp(m.value) === normalizedTarget,
      );
    }
    if (!entry) {
      entry = familyFallback(modelId, provider.models);
    }
  }

  return {
    rawId: modelId ?? "",
    // Prefer the catalog label even when matched via fallback — e.g.
    // a session resolved to `claude-sonnet-4-5-20251015` should still
    // render as "Claude Sonnet 4.5" if the catalog's entry for
    // `claude-sonnet-4-5-20250929` carried that label.
    label: entry?.label ?? modelId ?? "",
    providerLabel: provider?.label ?? "",
    entry,
  };
}
