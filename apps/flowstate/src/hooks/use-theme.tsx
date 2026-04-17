import * as React from "react";

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

export type ThemePreference = "light" | "dark" | "system";
export type ResolvedTheme = "light" | "dark";

interface ThemeContextValue {
  /** What the user explicitly chose (persisted to localStorage). */
  preference: ThemePreference;
  /** The theme actually in effect — "system" is resolved to light or dark. */
  resolvedTheme: ResolvedTheme;
  /** Update the preference. Persists to localStorage and syncs the DOM. */
  setTheme: (pref: ThemePreference) => void;
}

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const THEME_KEY = "flowstate:theme";
const DEFAULT_THEME: ThemePreference = "dark";
const MQL = "(prefers-color-scheme: dark)";

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

function readStoredPreference(): ThemePreference {
  if (typeof window === "undefined") return DEFAULT_THEME;
  const raw = window.localStorage.getItem(THEME_KEY);
  if (raw === "light" || raw === "dark" || raw === "system") return raw;
  return DEFAULT_THEME;
}

function getSystemTheme(): ResolvedTheme {
  if (typeof window === "undefined") return "dark";
  return window.matchMedia(MQL).matches ? "dark" : "light";
}

function resolve(pref: ThemePreference): ResolvedTheme {
  if (pref === "system") return getSystemTheme();
  return pref;
}

/** Apply or remove the `.dark` class on <html>. */
function syncDOM(resolved: ResolvedTheme) {
  const root = document.documentElement;
  if (resolved === "dark") {
    root.classList.add("dark");
  } else {
    root.classList.remove("dark");
  }
}

// ---------------------------------------------------------------------------
// Context
// ---------------------------------------------------------------------------

const ThemeContext = React.createContext<ThemeContextValue | null>(null);

// ---------------------------------------------------------------------------
// Provider
// ---------------------------------------------------------------------------

export function ThemeProvider({ children }: { children: React.ReactNode }) {
  const [preference, setPreference] = React.useState<ThemePreference>(readStoredPreference);
  const [resolvedTheme, setResolvedTheme] = React.useState<ResolvedTheme>(() => resolve(preference));

  // Sync the DOM class whenever the resolved theme changes.
  React.useEffect(() => {
    syncDOM(resolvedTheme);
  }, [resolvedTheme]);

  // When preference is "system", listen for OS changes.
  React.useEffect(() => {
    if (preference !== "system") return;

    const mql = window.matchMedia(MQL);
    const onChange = () => {
      setResolvedTheme(mql.matches ? "dark" : "light");
    };
    // Re-sync in case OS changed between render and effect mount.
    onChange();
    mql.addEventListener("change", onChange);
    return () => mql.removeEventListener("change", onChange);
  }, [preference]);

  const setTheme = React.useCallback((pref: ThemePreference) => {
    setPreference(pref);
    setResolvedTheme(resolve(pref));
    window.localStorage.setItem(THEME_KEY, pref);
  }, []);

  const value = React.useMemo<ThemeContextValue>(
    () => ({ preference, resolvedTheme, setTheme }),
    [preference, resolvedTheme, setTheme],
  );

  return (
    <ThemeContext.Provider value={value}>{children}</ThemeContext.Provider>
  );
}

// ---------------------------------------------------------------------------
// Hook
// ---------------------------------------------------------------------------

export function useTheme(): ThemeContextValue {
  const ctx = React.useContext(ThemeContext);
  if (!ctx) {
    throw new Error("useTheme must be used within a <ThemeProvider>");
  }
  return ctx;
}
