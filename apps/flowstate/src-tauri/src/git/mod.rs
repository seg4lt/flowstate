//! Git-related Tauri commands split by concern.
//!
//! * `branch`       — repo-root resolution, branch listing, create/delete
//! * `worktree`     — worktree listing / creation / removal, checkout
//! * `diff`         — one-shot diff summary + per-file before/after
//! * `diff_stream`  — streamed diff summary with cancellation

pub mod branch;
pub mod diff;
pub mod diff_stream;
pub mod worktree;

pub use diff_stream::DiffTasks;
