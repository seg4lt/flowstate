//! Flowstate app-layer crate.
//!
//! Owns all state the flowstate *app* cares about that neither the
//! runtime SDK nor the provider adapters should know about:
//!
//! - **`user_config`** — key/value tunables, session/project display
//!   rows, project-worktree links. SQLite-backed. Read/written by
//!   Tauri commands today; Phase 6 moves ownership into the daemon.
//! - **`usage`** — turn-level token/cost analytics. SQLite-backed,
//!   event-driven (writes on `RuntimeEvent::TurnCompleted`). Feeds the
//!   dashboard.
//! - **`orchestration_adapters`** — concrete impls of the runtime's
//!   `AppMetadataProvider` and `WorktreeProvisioner` traits.
//! - **`git_worktree`** — pure `std::process::Command` shells for
//!   `git worktree list` / `git worktree add`. Shared between the
//!   Tauri command wrappers and the `WorktreeProvisioner` impl.
//!
//! See `PLAN.md` (Phase 3) for the extraction rationale; the short
//! version is that the future daemon bin needs this state without
//! pulling Tauri.

// Sleep-prevention controller. macOS implements it via the
// `caffeinate` subprocess; Windows via `SetThreadExecutionState`.
// Other platforms have no backing OS hook so the module isn't
// compiled there.
#[cfg(any(target_os = "macos", windows))]
pub mod caffeinate;
pub mod git_worktree;
pub mod http;
pub mod orchestration_adapters;
pub mod provision;
pub mod usage;
pub mod user_config;
