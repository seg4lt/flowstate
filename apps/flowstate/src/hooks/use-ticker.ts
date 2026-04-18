import * as React from "react";

/**
 * Re-render the caller every `intervalMs` milliseconds. Returns the
 * current epoch-ms timestamp at each tick so consumers can compute
 * elapsed values without calling `Date.now()` themselves (handy in
 * tests where a stable reference is useful).
 *
 * Designed for lightweight "live counter" UIs — a single setInterval
 * per component, no dependency on external state, nothing to clean
 * up beyond the interval handle. Consumers mount it unconditionally
 * but gate the DOM output so a completed tool call doesn't keep
 * re-rendering just because the ticker is still ticking.
 */
export function useTicker(intervalMs: number = 1000): number {
  const [now, setNow] = React.useState<number>(() => Date.now());
  React.useEffect(() => {
    const id = window.setInterval(() => setNow(Date.now()), intervalMs);
    return () => window.clearInterval(id);
  }, [intervalMs]);
  return now;
}
