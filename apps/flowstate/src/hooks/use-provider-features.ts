import * as React from "react";
import type { ProviderFeatures, ProviderKind } from "@/lib/types";
import { useApp } from "@/stores/app-store";

/**
 * Safe fallback for when a session's provider hasn't reported
 * capabilities yet. Every flag is `false`, so UIs that gate on a flag
 * default to the "feature unavailable / hidden" variant — same
 * behaviour as when a flag was `undefined` under the old hand-written
 * `ProviderFeatures` shape.
 */
const EMPTY_PROVIDER_FEATURES: ProviderFeatures = {
  statusLabels: false,
  toolProgress: false,
  apiRetries: false,
  thinkingEffort: false,
  contextBreakdown: false,
  promptSuggestions: false,
  fileCheckpoints: false,
  compactCustomInstructions: false,
  sessionLifecycleEvents: false,
  supportsAutoPermissionMode: false,
};

/**
 * Feature-flag lookup for the provider backing a session. Every
 * cross-provider UI surface that depends on capability support should
 * gate on the flags returned here rather than branching on
 * `ProviderKind` directly — a new provider adding the feature should
 * just populate its own `ProviderFeatures` on the Rust side, with no
 * frontend change required.
 *
 * Returns `EMPTY_PROVIDER_FEATURES` (every flag `false`) when:
 *   - `kind` is `undefined` (caller doesn't have a session yet),
 *   - the app-store hasn't hydrated providers yet,
 *   - or the daemon build is older and doesn't emit `features`.
 *
 * In every case the UI should interpret missing flags as "feature
 * unavailable", which falls through to the safe / hidden variant.
 */
export function useProviderFeatures(
  kind: ProviderKind | undefined,
): ProviderFeatures {
  const { state } = useApp();
  return React.useMemo<ProviderFeatures>(() => {
    if (!kind) return EMPTY_PROVIDER_FEATURES;
    const entry = state.providers.find((p) => p.kind === kind);
    return entry?.features ?? EMPTY_PROVIDER_FEATURES;
  }, [state.providers, kind]);
}
