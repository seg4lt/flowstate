// Project-domain slice. Covers projects, projectDisplay,
// projectWorktrees, and sessionDisplay — the app-side labels,
// ordering, and worktree parent/child links. Read by the sidebar and
// the settings view; never touched by stream events that only affect
// a single session.
//
// Mutations come from `project_created` / `project_deleted` /
// `session_project_assigned` / `session_archived` / `session_unarchived`
// runtime events plus the `hydrate_display` / `set_*_display` /
// `set_project_worktree` actions emitted by the app-side display
// helpers in app-store.

export { useProjectSlice } from "../app-store";
