import * as React from "react";

import { useApp } from "@/stores/app-store";
import {
  DEFAULT_PROVIDER,
  readDefaultProvider,
} from "@/lib/defaults-settings";
import { PROVIDER_KINDS } from "@/lib/providers";
import type { ProviderKind } from "@/lib/types";
import { useProviderEnabled } from "@/hooks/use-provider-enabled";

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

interface DefaultProviderValue {
  /** The provider to use when starting a new thread without an explicit
   *  pick (e.g. after creating a worktree from the project home page
   *  or the sidebar's worktree-aware new-thread dropdown). */
  defaultProvider: ProviderKind;
  /** `false` until the async read of `defaults.provider` from SQLite
   *  completes. Callers that react to user input (button clicks) must
   *  gate the action on `loaded` so a click that lands during the
   *  async window doesn't silently fall back to a non-preferred
   *  provider. */
  loaded: boolean;
}

// ---------------------------------------------------------------------------
// Hook
// ---------------------------------------------------------------------------

/**
 * Resolve the provider to use for a new thread when the user hasn't
 * picked one explicitly. Fallback chain:
 *
 *   1. If the user's saved `defaults.provider` is set AND enabled →
 *      return it. No `status === "ready"` check: if the user chose
 *      Claude, we honor that even if Claude is still bootstrapping.
 *      The session-start path surfaces a clear error if the provider
 *      truly can't serve the request — silently picking a different
 *      provider is worse UX.
 *
 *   2. Else (saved provider is disabled, or none is saved) → return
 *      the first ready enabled provider in the canonical
 *      `PROVIDER_KINDS` order (so ties resolve to Claude before
 *      GitHub Copilot, matching `DEFAULT_PROVIDER`). Iterating
 *      `state.providers` directly would be wrong — the backend
 *      builds that list from a HashMap, so its order is
 *      nondeterministic.
 *
 *   3. Else → return the hardcoded `DEFAULT_PROVIDER` constant.
 *
 * The returned `loaded` flag distinguishes "still reading from SQLite"
 * from "loaded, but no saved preference". Consumers should disable the
 * action (create-worktree button, etc.) while `!loaded`, otherwise a
 * fast click can fire before the preference arrives and resolve via
 * step 2 or 3 even when the user has a saved default.
 */
export function useDefaultProvider(): DefaultProviderValue {
  const { state } = useApp();
  const { isProviderEnabled } = useProviderEnabled();

  const [savedProvider, setSavedProvider] = React.useState<ProviderKind | null>(
    null,
  );
  const [loaded, setLoaded] = React.useState(false);

  React.useEffect(() => {
    let cancelled = false;
    readDefaultProvider().then((saved) => {
      if (cancelled) return;
      setSavedProvider(saved);
      setLoaded(true);
    });
    return () => {
      cancelled = true;
    };
  }, []);

  const defaultProvider = React.useMemo<ProviderKind>(() => {
    // 1. Saved + enabled wins unconditionally.
    if (savedProvider && isProviderEnabled(savedProvider)) {
      return savedProvider;
    }
    // 2. First ready enabled provider in canonical PROVIDER_KINDS
    //    order. NOTE: iterating `state.providers` directly is WRONG
    //    here — the backend builds that list from a HashMap
    //    (`RuntimeCore::adapters`), so its order is nondeterministic.
    //    A user with Claude set as their mental default but nothing
    //    saved in SQLite (e.g. never touched the Settings dropdown
    //    because "Claude" is already the visible useState initial
    //    value) would land on whichever provider the HashMap happens
    //    to yield first — often GitHub Copilot. Iterating in the
    //    canonical order pins ties to Claude as documented by
    //    `DEFAULT_PROVIDER`.
    const providerMap = new Map(state.providers.map((p) => [p.kind, p]));
    for (const kind of PROVIDER_KINDS) {
      const info = providerMap.get(kind);
      if (info && isProviderEnabled(kind) && info.status === "ready") {
        return kind;
      }
    }
    // 3. Hardcoded fallback (or saved choice if nothing is ready yet —
    //    preserves the user's intent over the constant).
    return savedProvider ?? DEFAULT_PROVIDER;
  }, [state.providers, isProviderEnabled, savedProvider]);

  return { defaultProvider, loaded };
}
