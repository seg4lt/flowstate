import * as React from "react";
import { useApp } from "@/stores/app-store";
import { setCheckpointsGlobalEnabled } from "@/lib/api";
import type { CheckpointSettings } from "@/lib/types";
import { toast } from "@/hooks/use-toast";

/**
 * Reactive view of the daemon-owned checkpoint-enablement setting.
 * State flows one-way: daemon is source of truth, local cache is
 * refreshed via the `CheckpointEnablementChanged` broadcast handled
 * in the app-store reducer. The `setEnabled` helper awaits the
 * daemon's response and trusts the subsequent broadcast to land the
 * authoritative snapshot.
 */
export function useCheckpointSettings(): {
  settings: CheckpointSettings;
  setEnabled: (value: boolean) => Promise<void>;
} {
  const { state } = useApp();
  const settings = state.checkpointSettings;

  const setEnabled = React.useCallback(async (value: boolean) => {
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

  return { settings, setEnabled };
}
