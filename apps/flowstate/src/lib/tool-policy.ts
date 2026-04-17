// Shared policy for what counts as a "mutating" tool inside plan
// mode. Used by the Strict Plan Mode toggle: when enabled, the
// frontend auto-denies these tools before the UI surfaces a
// permission dialog, so the user can't accidentally click Allow
// and escape plan mode mid-investigation.
//
// Why these four:
// - Bash runs arbitrary shell — can do anything the shell can.
// - Write / Edit / NotebookEdit mutate source files directly.
//
// Read / Grep / Glob / WebFetch / WebSearch / TodoWrite / Task
// are intentionally NOT here. The claude-sdk bridge already
// auto-allows those in plan mode (see `planModeAllowedTools` in
// crates/core/provider-claude-sdk/bridge/src/index.ts) — they
// don't mutate state and prompting for them every turn is pure
// friction.
//
// This list is intentionally frontend-only policy. The SDK still
// prompts for Bash/Write/Edit/NotebookEdit in plan mode by default
// — that's THE prompt that matters (mutation attempt during
// "just look around"). Strict mode layers an additional
// auto-deny on top for users who want a harder guarantee.
export const PLAN_MODE_MUTATING_TOOLS = new Set<string>([
  "Bash",
  "Write",
  "Edit",
  "NotebookEdit",
]);

/** Human-readable list for tooltips / descriptions. */
export const PLAN_MODE_MUTATING_TOOLS_LABEL = Array.from(
  PLAN_MODE_MUTATING_TOOLS,
).join(", ");
