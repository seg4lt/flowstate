// Centralised normalisation for composer settings (effort,
// thinking-mode) against the active model's capabilities, plus a
// per-session cache of the dropdown-alias the user actually picked.
//
// ─── Why the picked-alias cache exists ────────────────────────────
//
// The Claude Agent SDK's `q.supportedModels()` returns *aliases* as
// model `value`s — `"default"`, `"sonnet"`, `"sonnet[1m]"`, `"haiku"`.
// Each alias carries its own `supportedEffortLevels` /
// `supportsAdaptiveThinking` flags. The user picks an alias from the
// toolbar dropdown, and we send that alias to the SDK as the model.
//
// On the first turn the SDK emits a `model_resolved` event with the
// *pinned* id it actually ran on — e.g. `"claude-opus-4-7-20250514"`
// for `"default"`, `"claude-sonnet-4-6-20251015"` for `"sonnet"`.
// runtime-core persists this pinned id onto `session.summary.model`
// so subsequent UI reads "what actually ran this turn".
//
// Problem: the pinned id has no textual relationship to the alias
// (especially `"default"` → `claude-opus-4-7-*`). So a catalog
// lookup against the post-`model_resolved` `session.model` returns
// `undefined`, and every capability-gated control (effort selector,
// Adaptive pill, clamp-on-model-change effect) sees empty flags and
// silently misbehaves.
//
// Fix: stash the alias the user picked (or that the session spawned
// with) in sessionStorage, keyed by sessionId. Capability-consuming
// sites resolve via `pickedModel ?? session.model`, so the right
// dropdown entry is found even after the pinned id has replaced the
// alias on `session.summary.model`. `session.summary.model` remains
// the source of truth for "what ran" — it's what agent-message
// headers, cost attribution, and usage dashboards read.
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

// ─── Picked-alias cache (per sessionId) ────────────────────────────
// Stored in sessionStorage, not the app store, because the right
// lifetime is "one browser tab" — same as `effort`, `thinkingMode`,
// and `permissionMode`. Lost on reload, which is fine: on reload the
// session will re-run through `model_resolved` on its next turn and
// the alias is gone anyway; we fall back to `session.model` and
// display degrades gracefully (entry = undefined). For the in-tab
// session where users actually interact with these controls, the
// cache lives exactly as long as it's useful.

const PICKED_MODEL_STORAGE_PREFIX = "flowstate:pickedModel:";

function pickedModelKey(sessionId: string): string {
  return `${PICKED_MODEL_STORAGE_PREFIX}${sessionId}`;
}

/**
 * Record the dropdown-alias the user picked (or that the session
 * spawned with) so later capability lookups can find the matching
 * catalog entry even after `model_resolved` has replaced
 * `session.model` with a pinned id.
 *
 * Silent on storage failures — sessionStorage can throw in private-
 * browsing mode and we don't want to take the toolbar down for an
 * edge-case persistence blip.
 */
export function rememberPickedModel(sessionId: string, model: string): void {
  try {
    sessionStorage.setItem(pickedModelKey(sessionId), model);
  } catch {
    /* storage may be unavailable */
  }
}

/**
 * Read the previously-remembered alias for a session, or `undefined`
 * if we've never seen the user pick / spawn with a model (fresh tab
 * on an existing session) or if storage access failed.
 */
export function readPickedModel(sessionId: string): string | undefined {
  try {
    return sessionStorage.getItem(pickedModelKey(sessionId)) ?? undefined;
  } catch {
    return undefined;
  }
}
