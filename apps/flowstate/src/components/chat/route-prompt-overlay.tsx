import * as React from "react";
import { useMatches } from "@tanstack/react-router";
import { sendMessage } from "@/lib/api";
import { useApp } from "@/stores/app-store";
import { readStrictPlanMode } from "@/lib/defaults-settings";
import { PLAN_MODE_MUTATING_TOOLS } from "@/lib/tool-policy";
import type {
  PermissionDecision,
  PermissionMode,
  UserInputAnswer,
} from "@/lib/types";
import { PermissionPrompt } from "./permission-prompt";
import { QuestionPrompt } from "./question-prompt";

/**
 * Route-independent surface for the per-session permission and
 * user-question prompts.
 *
 * Background: when an agent turn pauses for permission or a clarifying
 * question, the daemon waits for an `answer_permission` /
 * `answer_question` reply before unblocking. The chat view used to be
 * the only thing that rendered those prompts (inline above the
 * composer), but `/chat/$sessionId` and `/code/$sessionId` are sibling
 * routes that fully replace `<Outlet />`. Switching to the code view
 * unmounted `ChatView` and with it the prompt UI — the daemon stayed
 * blocked, but there was nothing on screen the user could click. This
 * component fills that gap.
 *
 * Rendering policy:
 *   * On `/chat/$sessionId` we yield to ChatView's inline prompt
 *     (positioned correctly above the composer) and render nothing.
 *   * On any other route that carries a `sessionId` param
 *     (`/code/$sessionId`, future `/diff`, etc.) we render the same
 *     PermissionPrompt / QuestionPrompt at the bottom of the route
 *     viewport so the user can answer without a navigation detour.
 *
 * The strict-plan-mode auto-deny effect runs here unconditionally
 * (route-independent) so a mutating-tool request that arrives while
 * the user is on `/code` is still denied even though ChatView is
 * unmounted.
 */
export function RoutePromptOverlay() {
  const { state, dispatch } = useApp();
  const matches = useMatches();

  // Extract sessionId from the current route, plus a flag for whether
  // we're on the chat route (where ChatView renders its own inline
  // prompt and we should yield).
  let sessionId: string | null = null;
  let onChatRoute = false;
  for (const m of matches) {
    const params = m.params as Record<string, string> | undefined;
    if (params?.sessionId) {
      sessionId = params.sessionId;
      onChatRoute = m.routeId === "/chat/$sessionId";
      break;
    }
  }

  const pendingPermissions = sessionId
    ? state.pendingPermissionsBySession.get(sessionId) ?? []
    : [];
  const pendingQuestion = sessionId
    ? state.pendingQuestionBySession.get(sessionId) ?? null
    : null;
  const permissionMode = sessionId
    ? state.permissionModeBySession.get(sessionId) ?? "accept_edits"
    : "accept_edits";

  // Strict plan mode opt-in. Mirrors ChatView's previous local state:
  // refresh on focus so flipping the toggle in Settings takes effect
  // without a reload.
  const [strictPlanMode, setStrictPlanMode] = React.useState(false);
  React.useEffect(() => {
    let cancelled = false;
    const refresh = () => {
      readStrictPlanMode().then((saved) => {
        if (!cancelled) setStrictPlanMode(saved);
      });
    };
    refresh();
    window.addEventListener("focus", refresh);
    return () => {
      cancelled = true;
      window.removeEventListener("focus", refresh);
    };
  }, []);

  // Auto-deny mutating tools while in plan mode + strict policy. Lives
  // here (not in ChatView) so the guard fires regardless of which
  // route the user is on — otherwise navigating to /code with a
  // mutating-tool prompt pending would leave the prompt undenied
  // until the user navigated back. `autoDeniedRef` prevents double-
  // answering the same request_id during the dispatch round-trip.
  const autoDeniedRef = React.useRef<Set<string>>(new Set());
  const handlePermissionDecisionRef = React.useRef<
    | ((
        decision: PermissionDecision,
        modeOverride?: PermissionMode,
        feedback?: string,
      ) => Promise<void>)
    | null
  >(null);

  const handlePermissionDecision = React.useCallback(
    async (
      decision: PermissionDecision,
      modeOverride?: PermissionMode,
      feedback?: string,
    ) => {
      if (!sessionId) return;
      const head = pendingPermissions[0];
      if (!head) return;
      // Pop before await so a rapid double-click can't answer twice
      // and the next queued prompt slides in immediately.
      dispatch({
        type: "consume_pending_permission",
        sessionId,
        requestId: head.requestId,
      });
      // Apply mode override locally first: write to sessionStorage so a
      // ChatView remount picks it up via its initializer, and dispatch
      // to the app-store so the toolbar / sidebar (and ChatView while
      // mounted) see the change immediately. This mirrors the wrapper
      // ChatView's `setPermissionMode` does.
      if (modeOverride) {
        try {
          window.sessionStorage.setItem(
            `flowstate:permissionMode:${sessionId}`,
            modeOverride,
          );
        } catch {
          // sessionStorage can throw in private mode; the dispatch is
          // still authoritative for live UI.
        }
        dispatch({
          type: "set_session_permission_mode",
          sessionId,
          mode: modeOverride,
        });
      }
      await sendMessage({
        type: "answer_permission",
        session_id: sessionId,
        request_id: head.requestId,
        decision,
        ...(modeOverride ? { permission_mode_override: modeOverride } : {}),
        ...(feedback ? { reason: feedback } : {}),
      });
    },
    [dispatch, pendingPermissions, sessionId],
  );

  // Stash the latest handler in a ref so the auto-deny effect can call
  // it without listing it as a dep (which would re-fire the effect on
  // every render that recreates the callback).
  React.useEffect(() => {
    handlePermissionDecisionRef.current = handlePermissionDecision;
  }, [handlePermissionDecision]);

  React.useEffect(() => {
    if (!strictPlanMode) return;
    if (permissionMode !== "plan") return;
    const head = pendingPermissions[0];
    if (!head) return;
    if (!PLAN_MODE_MUTATING_TOOLS.has(head.toolName)) return;
    if (autoDeniedRef.current.has(head.requestId)) return;
    autoDeniedRef.current.add(head.requestId);
    void handlePermissionDecisionRef.current?.("deny");
  }, [strictPlanMode, permissionMode, pendingPermissions]);

  // Prune the autoDenied set so it can't grow unbounded across a long
  // session (same logic as the previous in-ChatView effect).
  React.useEffect(() => {
    const live = new Set(pendingPermissions.map((p) => p.requestId));
    for (const id of autoDeniedRef.current) {
      if (!live.has(id)) autoDeniedRef.current.delete(id);
    }
  }, [pendingPermissions]);

  const handleQuestionSubmit = React.useCallback(
    async (answers: UserInputAnswer[]) => {
      if (!sessionId || !pendingQuestion) return;
      const requestId = pendingQuestion.requestId;
      dispatch({ type: "consume_pending_question", sessionId, requestId });
      await sendMessage({
        type: "answer_question",
        session_id: sessionId,
        request_id: requestId,
        answers,
      });
    },
    [dispatch, pendingQuestion, sessionId],
  );

  const handleQuestionCancel = React.useCallback(async () => {
    if (!sessionId || !pendingQuestion) return;
    const requestId = pendingQuestion.requestId;
    dispatch({ type: "consume_pending_question", sessionId, requestId });
    await sendMessage({
      type: "cancel_question",
      session_id: sessionId,
      request_id: requestId,
    });
  }, [dispatch, pendingQuestion, sessionId]);

  // On /chat/$sessionId, ChatView renders its own inline prompt above
  // the composer (the natural spot). We render nothing there to avoid
  // a duplicate banner. The hooks above still run unconditionally so
  // the auto-deny effect keeps firing regardless of route.
  if (!sessionId || onChatRoute) return null;
  if (pendingPermissions.length === 0 && !pendingQuestion) return null;

  return (
    <div className="absolute inset-x-0 bottom-0 z-10 flex flex-col">
      {pendingQuestion && (
        <QuestionPrompt
          questions={pendingQuestion.questions}
          onSubmit={handleQuestionSubmit}
          onCancel={handleQuestionCancel}
        />
      )}
      {pendingPermissions.length > 0 && (
        <PermissionPrompt
          key={pendingPermissions[0].requestId}
          toolName={pendingPermissions[0].toolName}
          input={pendingPermissions[0].input}
          onDecision={handlePermissionDecision}
          queueDepth={pendingPermissions.length}
        />
      )}
    </div>
  );
}
