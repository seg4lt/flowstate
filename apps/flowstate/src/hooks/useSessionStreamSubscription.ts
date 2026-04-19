import * as React from "react";
import { useQueryClient } from "@tanstack/react-query";
import { useNavigate } from "@tanstack/react-router";
import { useApp } from "@/stores/app-store";
import { applyEventToTurns } from "@/lib/session-event-reducer";
import {
  sessionQueryKey,
  type SessionPage,
} from "@/lib/queries";
import { toast } from "@/hooks/use-toast";
import type {
  PermissionMode,
  RetryState,
  TurnPhase,
} from "@/lib/types";

// Per-view reactions triggered by runtime events on the currently-
// visible session. ChatView used to hold these as an inline
// `connectStream` subscription; now it forwards the handlers into
// this hook, which subscribes to the store's shared stream listener
// API and routes per-session cache updates + current-view side
// effects. No second Tauri channel is opened — the backend sees
// exactly one subscriber (the store).
export interface SessionStreamHandlers {
  sessionId: string;
  sessionIdRef: React.MutableRefObject<string>;
  setPendingInput: (value: string | null) => void;
  setLastEventAt: (ts: number) => void;
  setStuckSince: (value: number | null) => void;
  setTurnPhase: (phase: TurnPhase | undefined) => void;
  setRetryState: (state: RetryState | null) => void;
  setPromptSuggestion: (s: string | null) => void;
  setPermissionMode: (mode: PermissionMode) => void;
  activateDiffSubscription: () => void;
  refreshDiffs: (opts?: { force?: boolean }) => void;
}

export function useSessionStreamSubscription(
  handlers: SessionStreamHandlers,
): void {
  const queryClient = useQueryClient();
  const navigate = useNavigate();
  const { addServerMessageListener } = useApp();

  // Stash handlers in a ref so the subscription effect doesn't tear
  // down and re-register on every ChatView render. The effect below
  // depends only on stable identities.
  const handlersRef = React.useRef(handlers);
  handlersRef.current = handlers;

  React.useEffect(() => {
    const unsubscribe = addServerMessageListener((message) => {
      const h = handlersRef.current;

      if (message.type === "session_loaded") {
        // Replace the cache entry for the target session outright —
        // this is the lag-recovery path, where the daemon is telling
        // us "here is the authoritative state of session X right now".
        const detail = message.session;
        const targetId = detail.summary.sessionId;
        const totalTurns = detail.summary.turnCount ?? detail.turns.length;
        queryClient.setQueryData<SessionPage>(sessionQueryKey(targetId), {
          detail,
          loadedTurns: detail.turns.length,
          totalTurns,
          hasMoreOlder: detail.turns.length < totalTurns,
        });
        if (targetId === h.sessionIdRef.current) {
          h.setPendingInput(null);
          h.setLastEventAt(Date.now());
          h.setStuckSince(null);
          // Activate and refresh the diff subscription so the badge
          // reflects the current working-tree state after reconnection.
          h.activateDiffSubscription();
          h.refreshDiffs();
        }
        return;
      }

      if (message.type !== "event") return;
      const event = message.event;
      if (!("session_id" in event)) return;
      const eventSessionId = event.session_id;

      // Route turn mutations to the event's session cache. Events
      // whose session isn't in the cache (the user has never
      // visited) silently no-op — when the user eventually opens
      // that thread, useQuery fetches fresh data from the daemon.
      queryClient.setQueryData<SessionPage>(
        sessionQueryKey(eventSessionId),
        (prev) => {
          if (!prev) return prev;
          const nextTurns = applyEventToTurns(prev.detail.turns, event);
          if (nextTurns === prev.detail.turns) return prev;
          const total = Math.max(prev.totalTurns, nextTurns.length);
          return {
            ...prev,
            detail: { ...prev.detail, turns: nextTurns },
            loadedTurns: nextTurns.length,
            totalTurns: total,
            hasMoreOlder: nextTurns.length < total,
          };
        },
      );

      // Per-view UI state only moves for events on the currently-
      // visible session. Everything below here is "reset pending
      // chrome" / "scroll-to-bottom hints" / "router navigation" —
      // all current-view concerns.
      if (eventSessionId !== h.sessionIdRef.current) return;

      h.setLastEventAt(Date.now());
      h.setStuckSince(null);

      switch (event.type) {
        case "turn_started":
          // Clear the optimistic pending row now that the real turn
          // has been appended to the cache. The store handles
          // pendingPermissions/pendingQuestion clearing globally
          // (turn_completed / session_interrupted reducer paths).
          h.setPendingInput(null);
          h.setTurnPhase(undefined);
          h.setRetryState(null);
          // Clear any stale suggestion from the previous turn —
          // the new turn will emit its own `prompt_suggested`
          // if the SDK has a prediction.
          h.setPromptSuggestion(null);
          break;

        case "turn_completed":
          h.setPendingInput(null);
          h.setTurnPhase(undefined);
          h.setRetryState(null);
          // Every completed turn activates the diff subscription
          // (idempotent after the first call) and restarts it so
          // the badge reflects what this turn left on disk. The
          // git work runs entirely on the Rust side via Tauri IPC
          // — non-blocking for the UI.
          h.activateDiffSubscription();
          h.refreshDiffs();
          break;

        case "files_rewound":
          // Native rewind just changed files on disk outside the
          // turn loop. Force the diff subscription open and refresh
          // so the badge updates to the post-rewind state. Toast
          // the totals so the user has feedback even if the diff
          // panel isn't visible. Cap the path-list preview in the
          // toast so a 200-file rewind doesn't blow it up.
          h.activateDiffSubscription();
          h.refreshDiffs({ force: true });
          {
            const restored = event.paths_restored.length;
            const deleted = event.paths_deleted.length;
            toast({
              description: `Reverted ${restored} restored, ${deleted} deleted.`,
              duration: 4000,
            });
          }
          break;

        case "content_delta":
          // First token of the turn clears any in-flight retry
          // banner — if the provider was retrying and the model
          // started responding, the retry succeeded. Always
          // dispatch: React short-circuits a same-value set, so
          // we don't need to gate on the current retryState.
          h.setRetryState(null);
          break;

        case "turn_status_changed":
          h.setTurnPhase(event.phase);
          break;

        case "turn_retrying":
          h.setRetryState({
            turnId: event.turn_id,
            attempt: event.attempt,
            maxRetries: event.max_retries,
            retryDelayMs: event.retry_delay_ms,
            errorStatus: event.error_status,
            error: event.error,
            startedAt: Date.now(),
          });
          break;

        case "prompt_suggested":
          // Latest prediction wins — the SDK may emit several over
          // the life of a turn and we only show the freshest.
          h.setPromptSuggestion(event.suggestion);
          break;

        case "tool_call_completed": {
          // Detect auto-approved EnterPlanMode completing successfully.
          // When it goes through the permission prompt, PlanEnterPrompt
          // already sets the mode via modeOverride. This catches the
          // bypass/allow-always case where no permission_requested fires.
          if (!event.error) {
            const cached = queryClient.getQueryData<SessionPage>(
              sessionQueryKey(h.sessionIdRef.current),
            );
            if (cached) {
              const turn = cached.detail.turns.find(
                (t) => t.turnId === event.turn_id,
              );
              const tc = turn?.toolCalls?.find(
                (c) => c.callId === event.call_id,
              );
              if (tc?.name === "EnterPlanMode" && !tc.parentCallId) {
                h.setPermissionMode("plan");
                toast({
                  description: "Agent switched to Plan mode",
                  duration: 3000,
                });
              }
            }
          }
          break;
        }

        // permission_requested / user_question_asked are handled in
        // the global store reducer (app-store.tsx). chat-view reads
        // pendingPermissions / pendingQuestion from the store, so a
        // prompt that arrives while the user is on a different
        // thread now lives in the store until they switch over.

        case "session_deleted":
        case "session_archived":
          // Active thread deleted / archived from elsewhere — bail
          // out so the user isn't staring at a title with no data.
          navigate({ to: "/" });
          break;
      }
    });

    return unsubscribe;
  }, [addServerMessageListener, queryClient, navigate]);
}
