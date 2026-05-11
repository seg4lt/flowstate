import * as React from "react";

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

interface EditStandaloneSettingValue {
  /** When true, Edit / MultiEdit tool calls break out of the inline
   *  tool-call group: each one lands in its own single-call group at
   *  the position it occurred, and any sibling tool calls before/after
   *  form a fresh group on either side. The user gets a message-style
   *  visual separator around every code edit instead of a tightly
   *  packed list of bullets. Off = the original behavior, where every
   *  consecutive same-scope tool call merges into one group. */
  editStandalone: boolean;
  /** Toggle the setting. Persists to localStorage immediately. */
  setEditStandalone: (next: boolean) => void;
}

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const STORAGE_KEY = "flowstate:edit-standalone";
// Off by default — the original tightly-packed tool-call grouping is
// the calmer baseline. Users who prefer per-edit message-style blocks
// can flip the toggle on in Settings; `readStored()` honors their
// explicit "true" in localStorage so they keep that preference across
// the default-flip. Only never-touched installs see the new default.
const DEFAULT_VALUE = false;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

function readStored(): boolean {
  if (typeof window === "undefined") return DEFAULT_VALUE;
  const raw = window.localStorage.getItem(STORAGE_KEY);
  if (raw === "true") return true;
  if (raw === "false") return false;
  return DEFAULT_VALUE;
}

// ---------------------------------------------------------------------------
// Context
// ---------------------------------------------------------------------------

const Ctx = React.createContext<EditStandaloneSettingValue | null>(null);

// ---------------------------------------------------------------------------
// Provider
// ---------------------------------------------------------------------------

export function EditStandaloneSettingProvider({
  children,
}: {
  children: React.ReactNode;
}) {
  const [enabled, setEnabled] = React.useState<boolean>(readStored);

  const setEditStandalone = React.useCallback((next: boolean) => {
    setEnabled(next);
    window.localStorage.setItem(STORAGE_KEY, String(next));
  }, []);

  const value = React.useMemo<EditStandaloneSettingValue>(
    () => ({ editStandalone: enabled, setEditStandalone }),
    [enabled, setEditStandalone],
  );

  return <Ctx.Provider value={value}>{children}</Ctx.Provider>;
}

// ---------------------------------------------------------------------------
// Hook
// ---------------------------------------------------------------------------

export function useEditStandaloneSetting(): EditStandaloneSettingValue {
  const ctx = React.useContext(Ctx);
  if (!ctx) {
    throw new Error(
      "useEditStandaloneSetting must be used within a <EditStandaloneSettingProvider>",
    );
  }
  return ctx;
}
