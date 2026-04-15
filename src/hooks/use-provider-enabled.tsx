import * as React from "react";
import type { ProviderKind } from "@/lib/types";
import {
  DEFAULT_ENABLED_PROVIDERS,
  readAllProviderEnabled,
  writeProviderEnabled,
} from "@/lib/defaults-settings";

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

interface ProviderEnabledValue {
  /** Whether a given provider is enabled at the app level. */
  isProviderEnabled: (kind: ProviderKind) => boolean;
  /** Toggle a provider on/off. Persists to SQLite immediately. */
  setProviderEnabled: (kind: ProviderKind, enabled: boolean) => void;
}

// ---------------------------------------------------------------------------
// Defaults
// ---------------------------------------------------------------------------

/** Initial map used before the SQLite read completes. Matches
 *  `DEFAULT_ENABLED_PROVIDERS` so pre-hydration renders are correct. */
function buildDefaults(): Map<ProviderKind, boolean> {
  const ALL: ProviderKind[] = [
    "claude",
    "claude_cli",
    "codex",
    "github_copilot",
    "github_copilot_cli",
  ];
  return new Map(
    ALL.map((k) => [k, DEFAULT_ENABLED_PROVIDERS.has(k)] as const),
  );
}

// ---------------------------------------------------------------------------
// Context
// ---------------------------------------------------------------------------

const Ctx = React.createContext<ProviderEnabledValue | null>(null);

// ---------------------------------------------------------------------------
// Provider
// ---------------------------------------------------------------------------

export function ProviderEnabledProvider({
  children,
}: {
  children: React.ReactNode;
}) {
  const [enabledMap, setEnabledMap] =
    React.useState<Map<ProviderKind, boolean>>(buildDefaults);

  // Hydrate from SQLite on mount.
  React.useEffect(() => {
    let cancelled = false;
    readAllProviderEnabled().then((map) => {
      if (!cancelled) setEnabledMap(map);
    });
    return () => {
      cancelled = true;
    };
  }, []);

  const setProviderEnabled = React.useCallback(
    (kind: ProviderKind, enabled: boolean) => {
      // Optimistic update — instant UI feedback.
      setEnabledMap((prev) => {
        const next = new Map(prev);
        next.set(kind, enabled);
        return next;
      });
      // Fire-and-forget persist to SQLite.
      writeProviderEnabled(kind, enabled);
    },
    [],
  );

  const isProviderEnabled = React.useCallback(
    (kind: ProviderKind): boolean => {
      const stored = enabledMap.get(kind);
      if (stored !== undefined) return stored;
      return DEFAULT_ENABLED_PROVIDERS.has(kind);
    },
    [enabledMap],
  );

  const value = React.useMemo<ProviderEnabledValue>(
    () => ({ isProviderEnabled, setProviderEnabled }),
    [isProviderEnabled, setProviderEnabled],
  );

  return <Ctx.Provider value={value}>{children}</Ctx.Provider>;
}

// ---------------------------------------------------------------------------
// Hook
// ---------------------------------------------------------------------------

export function useProviderEnabled(): ProviderEnabledValue {
  const ctx = React.useContext(Ctx);
  if (!ctx) {
    throw new Error(
      "useProviderEnabled must be used within a <ProviderEnabledProvider>",
    );
  }
  return ctx;
}
