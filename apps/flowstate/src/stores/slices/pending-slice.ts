// Pending-prompt slice. Covers pendingPermissionsBySession,
// pendingQuestionBySession, and permissionModeBySession — the state
// that drives chat-view's permission + question UI and the sidebar
// "awaiting input" badge.
//
// The actions that mutate this slice are `consume_pending_permission`,
// `consume_pending_question`, and `set_session_permission_mode`, plus
// the `permission_requested` / `user_question_asked` /
// `plan_proposed` / `turn_completed` / `session_interrupted` event
// cases. All still live in the single `appReducer`; this file
// publishes the narrow selector hook.

export { usePendingSlice } from "../app-store";
