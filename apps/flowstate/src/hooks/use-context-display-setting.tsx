import * as React from "react";

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

interface ContextDisplaySettingValue {
  /** Whether the context-window token counter is visible in the chat toolbar. */
  showContextDisplay: boolean;
  /** Toggle the setting. Persists to localStorage immediately. */
  setShowContextDisplay: (show: boolean) => void;
}

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const STORAGE_KEY = "flowstate:show-context-display";
const DEFAULT_VALUE = true;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

function readStored(): boolean {
  if (typeof window === "undefined") return DEFAULT_VALUE;
  const raw = window.localStorage.getItem(STORAGE_KEY);
  if (raw === "false") return false;
  if (raw === "true") return true;
  return DEFAULT_VALUE;
}

// ---------------------------------------------------------------------------
// Context
// ---------------------------------------------------------------------------

const Ctx = React.createContext<ContextDisplaySettingValue | null>(null);

// ---------------------------------------------------------------------------
// Provider
// ---------------------------------------------------------------------------

export function ContextDisplaySettingProvider({
  children,
}: {
  children: React.ReactNode;
}) {
  const [show, setShow] = React.useState<boolean>(readStored);

  const setShowContextDisplay = React.useCallback((next: boolean) => {
    setShow(next);
    window.localStorage.setItem(STORAGE_KEY, String(next));
  }, []);

  const value = React.useMemo<ContextDisplaySettingValue>(
    () => ({ showContextDisplay: show, setShowContextDisplay }),
    [show, setShowContextDisplay],
  );

  return <Ctx.Provider value={value}>{children}</Ctx.Provider>;
}

// ---------------------------------------------------------------------------
// Hook
// ---------------------------------------------------------------------------

export function useContextDisplaySetting(): ContextDisplaySettingValue {
  const ctx = React.useContext(Ctx);
  if (!ctx) {
    throw new Error(
      "useContextDisplaySetting must be used within a <ContextDisplaySettingProvider>",
    );
  }
  return ctx;
}
