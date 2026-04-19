import * as React from "react";
import { sendMessage } from "@/lib/api";
import { toast } from "@/hooks/use-toast";
import type { SessionSummary } from "@/lib/types";

// Escape interrupts the in-flight turn — but requires a *double* press
// within 2s to actually fire. A single Esc only "arms" the gesture and
// shows a toast hint; the second press inside the window does the
// interrupt. This guards against accidental presses (reaching for Esc
// to dismiss something else, OS habit, etc.) silently killing a long
// agent run. Mouse clicks on the working-indicator button and the
// composer stop button stay single-click — clicking a target is
// already deliberate. The title-rename Escape handler is scoped to
// its own input element, so this window-level listener doesn't
// clobber it when a rename is in progress.
export function useDoubleEscInterrupt(params: {
  session: SessionSummary | undefined;
  sessionId: string;
}): void {
  const { session, sessionId } = params;
  const escArmedRef = React.useRef(false);
  const escResetTimerRef = React.useRef<number | null>(null);
  const escToastDismissRef = React.useRef<(() => void) | null>(null);

  React.useEffect(() => {
    if (!session) return;

    const handleKeyDown = (event: KeyboardEvent) => {
      if (event.key !== "Escape") return;
      if (session.status !== "running") return;
      event.preventDefault();

      if (escArmedRef.current) {
        // Second press within the window — actually interrupt. Disarm
        // *before* sendMessage so a third press in the same tick can't
        // double-fire interrupt_turn.
        escArmedRef.current = false;
        if (escResetTimerRef.current != null) {
          clearTimeout(escResetTimerRef.current);
          escResetTimerRef.current = null;
        }
        escToastDismissRef.current?.();
        escToastDismissRef.current = null;
        sendMessage({ type: "interrupt_turn", session_id: sessionId }).catch(
          (err) => {
            // Surface the failure — previously this catch swallowed
            // errors silently, so an interrupt that failed to dispatch
            // would leave the user waiting with no feedback.
            console.error("Failed to interrupt turn", err);
            toast({
              title: "Interrupt failed",
              description:
                err instanceof Error ? err.message : "Unknown error",
              duration: 4000,
            });
          },
        );
        return;
      }

      // First press — arm and show the hint. Toast duration matches the
      // arming window so the visible cue and the keyboard handler's
      // internal state expire at the same instant.
      escArmedRef.current = true;
      escToastDismissRef.current = toast({
        description: "Press Esc again to interrupt",
        duration: 2000,
      }).dismiss;
      escResetTimerRef.current = window.setTimeout(() => {
        escArmedRef.current = false;
        escToastDismissRef.current = null;
        escResetTimerRef.current = null;
      }, 2000);
    };

    window.addEventListener("keydown", handleKeyDown);
    return () => {
      window.removeEventListener("keydown", handleKeyDown);
      if (escResetTimerRef.current != null) {
        clearTimeout(escResetTimerRef.current);
        escResetTimerRef.current = null;
      }
      escToastDismissRef.current?.();
      escToastDismissRef.current = null;
      escArmedRef.current = false;
    };
  }, [sessionId, session]);

  // If the turn finishes naturally between the first and second Esc
  // press, the arming state and its toast become misleading ("Press Esc
  // again to interrupt" when there's nothing to interrupt anymore).
  // Reactively disarm whenever status leaves "running".
  const sessionStatus = session?.status;
  React.useEffect(() => {
    if (sessionStatus === "running") return;
    if (escResetTimerRef.current != null) {
      clearTimeout(escResetTimerRef.current);
      escResetTimerRef.current = null;
    }
    escToastDismissRef.current?.();
    escToastDismissRef.current = null;
    escArmedRef.current = false;
  }, [sessionStatus]);
}
