//! Process-wide tunables for the Claude SDK adapter that the host
//! app surfaces to the user via Settings.
//!
//! These live as a `RwLock`-protected static rather than being passed
//! through the runtime-core spawn path because they're "default for
//! every new session" knobs — there's no UI affordance for setting
//! them per-thread, and threading them through every layer
//! (runtime-core ↔ daemon-core ↔ adapter) for a single optional
//! integer would be a lot of boilerplate for very little win.
//!
//! The Tauri shell calls [`set_max_tokens`] in two places:
//!   1. App startup, after reading `user_config["defaults.max_tokens"]`
//!      from sqlite. Seeds the default before any session is created.
//!   2. The `set_claude_max_tokens` Tauri command, fired when the user
//!      saves the value in Settings. Affects sessions created from
//!      that point on; existing sessions keep their original value
//!      since it's baked in at SDK Query open time.

use std::sync::RwLock;

/// Hard floor matching the Anthropic API's minimum `task_budget`.
/// See <https://platform.claude.com/docs/en/build-with-claude/task-budgets>.
pub const MIN_MAX_TOKENS: u64 = 20_000;

/// Soft ceiling matching Opus 4.7's `max_output_tokens` so the UI
/// can validate before the API would. Opus 4.6 / Sonnet 4.x cap
/// lower (64k) but we accept the higher value here and let the API
/// reject if a model can't honour it.
pub const MAX_MAX_TOKENS: u64 = 128_000;

/// Default we install when the user hasn't set anything yet. Sized
/// for the Opus 4.7 tokenizer change (~1.0–1.35× the token count of
/// 4.6 for the same English text) — high enough that a typical agent
/// turn doesn't truncate, low enough that runaway loops still cost
/// the user a bounded amount.
pub const DEFAULT_MAX_TOKENS: u64 = 64_000;

static CURRENT: RwLock<Option<u64>> = RwLock::new(None);

/// Update the active default. `None` clears the override and the
/// adapter falls back to the SDK's own default (no `taskBudget`
/// passed). Values outside `[MIN_MAX_TOKENS, MAX_MAX_TOKENS]` are
/// clamped — the UI is supposed to reject these but we belt-and-
/// braces here so a bad sqlite row can't poison the daemon.
pub fn set_max_tokens(value: Option<u64>) {
    let normalized = value.map(|v| v.clamp(MIN_MAX_TOKENS, MAX_MAX_TOKENS));
    if let Ok(mut g) = CURRENT.write() {
        *g = normalized;
    }
}

/// Read the active default. Called by the adapter at session-create
/// time so a Settings change picks up on the next thread spawn
/// without restart. Returns `None` until `set_max_tokens` has been
/// called at least once (typically by the Tauri shell on boot).
pub fn current_max_tokens() -> Option<u64> {
    CURRENT.read().ok().and_then(|g| *g)
}
