import * as React from "react";
import type { ProviderFeatures, ProviderKind } from "@/lib/types";
import { useApp } from "@/stores/app-store";

/**
 * Feature-flag lookup for the provider backing a session. Every
 * cross-provider UI surface that depends on capability support should
 * gate on the flags returned here rather than branching on
 * `ProviderKind` directly — a new provider adding the feature should
 * just populate its own `ProviderFeatures` on the Rust side, with no
 * frontend change required.
 *
 * Returns an empty object (every flag `undefined`, i.e. falsy) when:
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
    if (!kind) return {};
    const entry = state.providers.find((p) => p.kind === kind);
    return entry?.features ?? {};
  }, [state.providers, kind]);
}
