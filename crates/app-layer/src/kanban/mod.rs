//! Kanban orchestrator feature.
//!
//! A self-driving task system layered on top of the existing
//! flowstate agent runtime. Behind a setting flag and OFF by
//! default; nothing in the existing flowstate experience changes
//! until the user opts in.
//!
//! Architecture (see `/Users/babal/.claude/plans/let-s-think-this-trhgouh-pure-acorn.md`
//! for full design notes):
//!
//! 1. The user drops a free-text task on a kanban board. Stored in
//!    `kanban.sqlite` (this crate, separate file from
//!    `user_config.sqlite`).
//! 2. A stateless **triage** agent figures out which project the
//!    task belongs to, asks for disambiguation when needed.
//! 3. A per-task **orchestrator** session owns the technical
//!    decisions (model/permission/effort, when to spawn workers).
//! 4. **Worker** agents do the actual coding/reviewing in a
//!    per-task git worktree. They don't know about the kanban —
//!    they signal completion via `<<<TASK_DONE: …>>>` markers.
//! 5. A **tick loop** in `runtime-core` periodically nudges the
//!    orchestrator session of each active task, gated by a UI
//!    toggle.
//!
//! This module owns the persistence, models, business rules, HTTP
//! surface, and orchestrator-MCP dispatch. The tick loop and
//! runtime wiring live in `runtime-core` to keep this crate from
//! pulling the SDK runtime as an internal detail.

pub mod agents;
pub mod http;
pub mod merge;
pub mod model;
pub mod prompts;
pub mod service;
pub mod store;
pub mod tick;

pub use agents::{AgentSpawner, SessionPoll};
pub use http::{KanbanApiState, OrchestratorTickKick, router};
pub use merge::{MergeError, MergeOutcome, cleanup_worktree, merge_task};
pub use model::{
    CommentAuthor, KeyDirectory, OrchestratorSetting, ProjectMemory, SessionRole, Task,
    TaskComment, TaskSession, TaskState, settings_keys,
};
pub use service::{TransitionError, WorkerSignal, parse_worker_marker, validate_transition};
pub use store::KanbanStore;
pub use tick::{TickHandle, spawn_tick_task};
