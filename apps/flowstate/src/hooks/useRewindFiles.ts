import * as React from "react";
import { rewindFiles } from "@/lib/api";
import type { RewindOutcomeWire } from "@/lib/types";
import { toast } from "@/hooks/use-toast";

/**
 * State machine for the rewind-workflow dialog.
 *
 * Flow:
 *   idle
 *     └─ preview(turnId) →  loading
 *                              ├─ outcome.kind == "applied"
 *                              │      └─ previewed(outcome)           [user still has to click Apply]
 *                              ├─ outcome.kind == "needs_confirmation"
 *                              │      └─ conflicts_pending(conflicts) [user confirms or cancels]
 *                              ├─ outcome.kind == "unavailable"
 *                              │      └─ unavailable(reason)
 *                              └─ throw    → error(message)
 *
 *   previewed(outcome)  ─ apply()       →  applying  → applied(outcome) | error
 *   conflicts_pending   ─ confirm()     →  applying  → applied(outcome) | error
 *   any                 ─ cancel()      →  idle
 */
export type RewindState =
  | { kind: "idle" }
  | { kind: "loading"; turnId: string }
  | { kind: "previewed"; turnId: string; outcome: RewindOutcomeWire & { kind: "applied" } }
  | {
      kind: "conflicts_pending";
      turnId: string;
      outcome: RewindOutcomeWire & { kind: "needs_confirmation" };
    }
  | {
      kind: "unavailable";
      turnId: string;
      outcome: RewindOutcomeWire & { kind: "unavailable" };
    }
  | { kind: "applying"; turnId: string }
  | { kind: "applied"; turnId: string; outcome: RewindOutcomeWire & { kind: "applied" } }
  | { kind: "error"; turnId: string; message: string };

export interface UseRewindFiles {
  state: RewindState;
  preview: (turnId: string) => Promise<void>;
  apply: () => Promise<void>;
  cancel: () => void;
}

export function useRewindFiles(sessionId: string): UseRewindFiles {
  const [state, setState] = React.useState<RewindState>({ kind: "idle" });

  const preview = React.useCallback(
    async (turnId: string) => {
      setState({ kind: "loading", turnId });
      try {
        const outcome = await rewindFiles({
          sessionId,
          turnId,
          dryRun: true,
        });
        switch (outcome.kind) {
          case "applied":
            setState({ kind: "previewed", turnId, outcome });
            break;
          case "needs_confirmation":
            setState({ kind: "conflicts_pending", turnId, outcome });
            break;
          case "unavailable":
            setState({ kind: "unavailable", turnId, outcome });
            break;
        }
      } catch (e) {
        const message = e instanceof Error ? e.message : String(e);
        setState({ kind: "error", turnId, message });
        toast({
          title: "Rewind preview failed",
          description: message,
        });
      }
    },
    [sessionId],
  );

  const apply = React.useCallback(async () => {
    // Apply is only meaningful from previewed or conflicts_pending.
    // Any other state is a programmer error; silently no-op rather
    // than throwing since UI buttons are disabled in other states.
    const current = state;
    if (
      current.kind !== "previewed" &&
      current.kind !== "conflicts_pending"
    ) {
      return;
    }
    const { turnId } = current;
    const confirmConflicts = current.kind === "conflicts_pending";
    setState({ kind: "applying", turnId });
    try {
      const outcome = await rewindFiles({
        sessionId,
        turnId,
        dryRun: false,
        confirmConflicts,
      });
      if (outcome.kind === "applied") {
        setState({ kind: "applied", turnId, outcome });
        const totalCount =
          outcome.paths_restored.length +
          outcome.paths_deleted.length;
        toast({
          title: "Rewind applied",
          description:
            totalCount === 0
              ? "No files needed to change."
              : `${outcome.paths_restored.length} restored, ${outcome.paths_deleted.length} deleted.`,
        });
      } else if (outcome.kind === "needs_confirmation") {
        // Shouldn't happen since we just passed confirmConflicts=true,
        // but handle it gracefully — new conflicts may have appeared
        // between preview and apply (another editor touched a file).
        setState({ kind: "conflicts_pending", turnId, outcome });
      } else {
        setState({ kind: "unavailable", turnId, outcome });
      }
    } catch (e) {
      const message = e instanceof Error ? e.message : String(e);
      setState({ kind: "error", turnId, message });
      toast({
        title: "Rewind failed",
        description: message,
      });
    }
  }, [sessionId, state]);

  const cancel = React.useCallback(() => {
    setState({ kind: "idle" });
  }, []);

  return { state, preview, apply, cancel };
}
