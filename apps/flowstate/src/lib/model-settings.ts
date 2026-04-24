// Centralised normalisation for composer settings (effort,
// thinking-mode) against the active model's capabilities.
//
// The Claude Agent SDK populates `ProviderModel.supportedEffortLevels`
// and `ProviderModel.supportsAdaptiveThinking` per-model — but nothing
// in the UI layer used to re-check these when the user switched
// models. Result: pick Opus (supports `xhigh`/`max`), set effort to
// X-High, switch to Sonnet, and the stored effort silently stayed at
// `xhigh` even though Sonnet doesn't advertise it. Same story for
// `adaptive` thinking: some models don't accept it.
//
// These two pure functions are the one place we clamp dependent
// settings to what the selected model will actually honour. They are
// called from a `useEffect` in `chat-view.tsx` keyed on
// `session?.model`, so they fire on:
//   1. explicit model switches (ModelSelector),
//   2. the Claude SDK's `model_resolved` event on turn 1 (which can
//      replace an alias with a pinned date-stamped id), and
//   3. session hydration, where the effort persisted in
//      sessionStorage may be incompatible with the session's current
//      model after an app restart.

import type { ProviderModel, ReasoningEffort, ThinkingMode } from "./types";

/**
 * Effort levels in descending order of capability. Used both to drive
 * the UI selector's render order and to decide the fallback target
 * when an unsupported level is clamped — we walk *down* this list
 * from the current level until we find one the active model accepts.
 *
 * Kept here (not in `effort-selector.tsx`) so the clamping logic
 * doesn't depend on a component file, and the ordering is one shared
 * source of truth.
 */
export const EFFORT_ORDER: readonly ReasoningEffort[] = [
  "max",
  "xhigh",
  "high",
  "medium",
  "low",
  "minimal",
] as const;

/**
 * Effort levels that are model-gated — i.e. only visible / accepted
 * when the active model explicitly advertises them via
 * `ProviderModel.supportedEffortLevels`. The non-gated levels
 * (`high` / `medium` / `low` / `minimal`) are the universal baseline
 * and are always accepted.
 *
 * Mirrors the gating used by the UI's `visibleEffortOptions` in
 * `effort-selector.tsx`. If you add a new gated level, update both.
 */
export const MODEL_GATED_EFFORT_LEVELS: ReadonlySet<ReasoningEffort> = new Set([
  "xhigh",
  "max",
]);

function effortIsAccepted(
  level: ReasoningEffort,
  supported: ReadonlySet<string>,
): boolean {
  // Non-gated levels are always accepted — they're the universal
  // baseline every Claude model honours.
  if (!MODEL_GATED_EFFORT_LEVELS.has(level)) return true;
  return supported.has(level);
}

/**
 * Clamp a reasoning effort value to what the active model will
 * actually honour. Returns the input unchanged when:
 *   - `modelEntry` is undefined (catalog not loaded yet — avoid
 *     premature clamping during bootstrap, we'll rerun when it is),
 *   - the current level isn't model-gated, or
 *   - the current level *is* gated but the model advertises it.
 *
 * When the current level is unsupported, walks *down* `EFFORT_ORDER`
 * starting from the current level's position and returns the first
 * accepted level — i.e. the highest supported level ≤ current. This
 * preserves the user's "turn the dial up" intent as closely as the
 * model allows.
 *
 * Example: current = `max`, model accepts `["xhigh", "high"]` → clamps
 * to `xhigh` (the highest accepted level below `max`). Model accepts
 * `[]` (unknown) → clamps to `high` (first non-gated level below
 * `max` / `xhigh`).
 */
export function clampEffortToModel(
  effort: ReasoningEffort,
  modelEntry: ProviderModel | undefined,
): ReasoningEffort {
  if (!modelEntry) return effort;
  const supported = new Set(modelEntry.supportedEffortLevels ?? []);
  if (effortIsAccepted(effort, supported)) return effort;
  const idx = EFFORT_ORDER.indexOf(effort);
  // Unknown level (shouldn't happen — type system guards this — but
  // be defensive against sessionStorage carrying a stale value from
  // a future version).
  if (idx === -1) return "high";
  for (let i = idx + 1; i < EFFORT_ORDER.length; i++) {
    const candidate = EFFORT_ORDER[i];
    if (effortIsAccepted(candidate, supported)) return candidate;
  }
  // Unreachable: `high` is non-gated so always accepted. Belt-and-
  // braces fallback so the function is total.
  return "high";
}

/**
 * Clamp a thinking-mode value to what the active model will honour.
 * Today the only gated mode is `adaptive`: on models where
 * `supportsAdaptiveThinking` is false, we fall back to `always`
 * (which every Claude model accepts — it just forces a concrete
 * `budgetTokens`).
 *
 * Returns the input unchanged when `modelEntry` is undefined so we
 * don't flip the user's preferred mode during catalog bootstrap.
 */
export function clampThinkingModeToModel(
  mode: ThinkingMode,
  modelEntry: ProviderModel | undefined,
): ThinkingMode {
  if (!modelEntry) return mode;
  if (mode === "adaptive" && !modelEntry.supportsAdaptiveThinking) {
    return "always";
  }
  return mode;
}
