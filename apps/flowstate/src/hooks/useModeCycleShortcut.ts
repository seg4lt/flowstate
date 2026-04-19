import * as React from "react";
import { sendMessage } from "@/lib/api";
import { cycleMode, MODE_LABELS } from "@/lib/mode-cycling";
import { toast } from "@/hooks/use-toast";
import type { PermissionMode, SessionSummary } from "@/lib/types";

// Shift+Tab mode-cycling keybinding, extracted from chat-view.
//
// Only active when `session` is non-null — archived threads with no
// live session don't respond to the shortcut. If a turn is running,
// the hook also pushes `update_permission_mode` so the in-flight
// adapter picks up the change immediately — without this, toggling
// bypass mid-turn would still prompt for every subsequent tool call
// until the turn ends.
export function useModeCycleShortcut(params: {
  session: SessionSummary | undefined;
  sessionId: string;
  permissionMode: PermissionMode;
  excludedModes: PermissionMode[];
  setPermissionMode: (mode: PermissionMode) => void;
}): void {
  const { session, sessionId, permissionMode, excludedModes, setPermissionMode } =
    params;

  React.useEffect(() => {
    if (!session) return; // Only active when session exists

    const handleKeyDown = (event: KeyboardEvent) => {
      // Only respond to Shift+Tab
      if (event.key !== "Tab" || !event.shiftKey) return;

      // Skip when focus is on an INPUT or contenteditable — e.g.
      // title-rename, branch switcher search, diff style toggles —
      // where Shift+Tab should keep its default focus-navigation
      // behavior. The composer <textarea> is the only textarea in
      // the app and is intentionally NOT skipped: users want the
      // mode to cycle while typing without losing their cursor.
      const target = event.target as HTMLElement;
      if (target.tagName === "INPUT" || target.isContentEditable) {
        return;
      }

      // Prevent default Tab behavior (focus navigation)
      event.preventDefault();

      // Cycle to next mode. Always update local state so the toolbar
      // reflects the choice and the next `send_turn` sends it. If a
      // turn is in flight, also push `update_permission_mode` so the
      // in-flight adapter picks up the change immediately — without
      // this, toggling bypass mid-turn would still prompt for every
      // subsequent tool call until the turn ends.
      const newMode = cycleMode(permissionMode, "forward", excludedModes);
      setPermissionMode(newMode);
      if (session?.status === "running") {
        void sendMessage({
          type: "update_permission_mode",
          session_id: sessionId,
          permission_mode: newMode,
        });
      }

      toast({
        description: `Mode: ${MODE_LABELS[newMode]}`,
        duration: 2000,
      });
    };

    window.addEventListener("keydown", handleKeyDown);
    return () => window.removeEventListener("keydown", handleKeyDown);
  }, [session, sessionId, permissionMode, excludedModes, setPermissionMode]);
}
