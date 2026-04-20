import * as React from "react";
import { useApp } from "@/stores/app-store";
import {
  setCheckpointsGlobalEnabled,
  setProjectCheckpointsOverride,
} from "@/lib/api";
import type { CheckpointSettings } from "@/lib/types";
import { toast } from "@/hooks/use-toast";

/**
 * Reactive view of the daemon-owned checkpoint settings plus mutation
 * helpers. State flows one-way: daemon is source of truth, local
 * cache is refreshed via the `CheckpointEnablementChanged` broadcast
 * handled in the app-store reducer. Mutation helpers await the
 * daemon's response and trust the subsequent broadcast to land the
 * authoritative snapshot; the returned value from the mutation is
 * optimistically applied only if you want to avoid the broadcast
 * round-trip for immediate UI feedback.
 */
export function useCheckpointSettings(): {
  settings: CheckpointSettings;
  setGlobalEnabled: (value: boolean) => Promise<void>;
  /** `null` clears the override so the project inherits the global. */
  setProjectOverride: (
    projectId: string,
    value: boolean | null,
  ) => Promise<void>;
  /** Resolve the effective flag for a given project id — returns the
   *  project override when set, otherwise the global default. */
  effectiveFor: (projectId: string | null | undefined) => boolean;
} {
  const { state } = useApp();
  const settings = state.checkpointSettings;

  const setGlobalEnabled = React.useCallback(async (value: boolean) => {
    try {
      await setCheckpointsGlobalEnabled(value);
    } catch (e) {
      const message = e instanceof Error ? e.message : String(e);
      toast({
        title: "Failed to update checkpoint setting",
        description: message,
      });
      throw e;
    }
  }, []);

  const setProjectOverride = React.useCallback(
    async (projectId: string, value: boolean | null) => {
      try {
        await setProjectCheckpointsOverride(projectId, value);
      } catch (e) {
        const message = e instanceof Error ? e.message : String(e);
        toast({
          title: "Failed to update project setting",
          description: message,
        });
        throw e;
      }
    },
    [],
  );

  // Index project overrides by id for O(1) effective lookup. The list
  // stays tiny (only projects with explicit overrides) so rebuilding
  // the map per render is cheap.
  const overrideById = React.useMemo(() => {
    const map = new Map<string, boolean>();
    for (const row of settings.projectOverrides) {
      map.set(row.projectId, row.enabled);
    }
    return map;
  }, [settings.projectOverrides]);

  const effectiveFor = React.useCallback(
    (projectId: string | null | undefined): boolean => {
      if (projectId) {
        const override = overrideById.get(projectId);
        if (override !== undefined) return override;
      }
      return settings.globalEnabled;
    },
    [overrideById, settings.globalEnabled],
  );

  return { settings, setGlobalEnabled, setProjectOverride, effectiveFor };
}
