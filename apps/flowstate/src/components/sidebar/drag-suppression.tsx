import * as React from "react";

/**
 * Sidebar drag-suppression context.
 *
 * The sidebar's DndContext wires up a KeyboardSensor so Space/Enter on
 * a focused sortable (e.g. a project row) picks it up for reordering.
 * That's the right default for keyboard users — but it fights modals
 * opened _from_ the sidebar: after clicking a dropdown item that pops
 * a dialog, focus can land or persist on a sortable, and a subsequent
 * Space/Enter (intended for a dialog button or input) starts a drag.
 *
 * Rather than case-by-case `stopPropagation` gymnastics, any component
 * that opens a modal-like surface from inside the sidebar can call
 * `useSuppressSidebarDrag(isOpen)` to bump a refcount while open. The
 * provider exposes an aggregated boolean; the sidebar's DndContext
 * swaps in an empty sensor array when suppressed, so neither keyboard
 * nor pointer drag can activate until the modal closes.
 *
 * Refcounted rather than boolean so overlapping modals compose cleanly
 * (dialog opens a nested confirm, etc.).
 */
interface DragSuppressionContextValue {
  suppressed: boolean;
  /** Bumps the refcount; returns a release function. */
  push: () => () => void;
}

const DragSuppressionContext =
  React.createContext<DragSuppressionContextValue | null>(null);

export function SidebarDragSuppressionProvider({
  children,
}: {
  children: React.ReactNode;
}) {
  const [count, setCount] = React.useState(0);
  const push = React.useCallback(() => {
    setCount((c) => c + 1);
    let released = false;
    return () => {
      if (released) return;
      released = true;
      setCount((c) => Math.max(0, c - 1));
    };
  }, []);
  const value = React.useMemo(
    () => ({ suppressed: count > 0, push }),
    [count, push],
  );
  return (
    <DragSuppressionContext.Provider value={value}>
      {children}
    </DragSuppressionContext.Provider>
  );
}

/** Returns whether sidebar drag is currently suppressed by any modal. */
export function useSidebarDragSuppressed(): boolean {
  return React.useContext(DragSuppressionContext)?.suppressed ?? false;
}

/**
 * Convenience hook: while `active` is true, the sidebar's drag sensors
 * are disabled. No-op outside the provider (e.g. if someone renders a
 * dialog outside the sidebar tree) so callers don't need to guard.
 */
export function useSuppressSidebarDrag(active: boolean): void {
  const ctx = React.useContext(DragSuppressionContext);
  React.useEffect(() => {
    if (!active || !ctx) return;
    return ctx.push();
  }, [active, ctx]);
}
