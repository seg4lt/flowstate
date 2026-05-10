//! Kanban data types — tasks, comments, task↔session links, project
//! memory, and orchestrator settings.
//!
//! These shapes are owned by the **flowstate app layer**, not the
//! agent SDK's persistence crate. Nothing in `runtime-core` or any
//! provider adapter reads them to execute or resume an agent.
//! Per `crates/core/persistence/CLAUDE.md`, that's the litmus test
//! for what lives where — kanban rows are an application concern.
//!
//! All shapes derive `serde::{Serialize, Deserialize}` because they
//! cross both the Tauri-IPC and HTTP boundaries and round-trip into
//! SQLite as JSON columns for the unstructured-array fields.

use serde::{Deserialize, Serialize};

/// Lifecycle states a kanban task moves through.
///
/// The state machine is enforced in `service::validate_transition`,
/// not at the SQLite CHECK level — SQLite only refuses unknown
/// strings; the *legal-transition* graph lives in code where the
/// errors can be precise.
///
/// `Cancelled` is a terminal state distinct from `Done`. The user
/// can cancel a task at any non-terminal state via the UI; the
/// orchestrator session and any active worker are retired but the
/// row is kept for audit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskState {
    /// Just created. No triage agent has looked at it yet.
    Open,
    /// Triage one-shot agent is running. Project unknown; or
    /// suggestions pending user disambiguation.
    Triage,
    /// Triage done. Project tagged, project_memory present.
    /// Orchestrator session about to be spawned.
    Ready,
    /// Coder worker is active (or about to be) in the task's
    /// worktree. The orchestrator session lives across this and
    /// every later state.
    Code,
    /// Reviewer worker is active. Agent-side review only — human
    /// approval still required afterwards.
    AgentReview,
    /// Human approval gate. The kanban UI shows an Approve button
    /// here. Loop does not nudge this state — it waits for the
    /// human action.
    HumanReview,
    /// Auto-merge is running (or about to). `task_request_merge`
    /// flips into this; success → Done, conflict → NeedsHuman.
    Merge,
    /// Terminal success. Worktree gone, branch deleted, sessions
    /// retired, memory updated.
    Done,
    /// Explicit human-gate. Either the orchestrator/worker
    /// flagged a blocker, the orchestrator couldn't decide which
    /// project, a cross-project dep was detected, or merge
    /// conflicted. `needs_human_reason` carries the question.
    NeedsHuman,
    /// Terminal cancel. The user gave up on this task.
    Cancelled,
}

impl TaskState {
    pub fn as_str(self) -> &'static str {
        match self {
            TaskState::Open => "Open",
            TaskState::Triage => "Triage",
            TaskState::Ready => "Ready",
            TaskState::Code => "Code",
            TaskState::AgentReview => "AgentReview",
            TaskState::HumanReview => "HumanReview",
            TaskState::Merge => "Merge",
            TaskState::Done => "Done",
            TaskState::NeedsHuman => "NeedsHuman",
            TaskState::Cancelled => "Cancelled",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        Some(match s {
            "Open" => TaskState::Open,
            "Triage" => TaskState::Triage,
            "Ready" => TaskState::Ready,
            "Code" => TaskState::Code,
            "AgentReview" => TaskState::AgentReview,
            "HumanReview" => TaskState::HumanReview,
            "Merge" => TaskState::Merge,
            "Done" => TaskState::Done,
            "NeedsHuman" => TaskState::NeedsHuman,
            "Cancelled" => TaskState::Cancelled,
            _ => return None,
        })
    }

    /// States the **tick loop** should consider when scanning for
    /// next actions. Excludes terminals and human-gated states.
    ///
    /// `HumanReview` is excluded because progress is human-driven:
    /// the loop sees nothing to do until the Approve button is
    /// clicked, at which point the route handler explicitly kicks
    /// the loop (no need for a polling tick to discover it).
    pub fn is_actionable(self) -> bool {
        matches!(
            self,
            TaskState::Open
                | TaskState::Triage
                | TaskState::Ready
                | TaskState::Code
                | TaskState::AgentReview
                | TaskState::Merge
        )
    }

    pub fn is_terminal(self) -> bool {
        matches!(self, TaskState::Done | TaskState::Cancelled)
    }
}

/// Persona of a session attached to a task.
///
/// Drives two things: (a) the orchestrator-MCP dispatch's audience
/// check (only `triage`, `orchestrator`, `memory_seeder`,
/// `memory_updater` are allowed to call orchestrator tools); and
/// (b) the UI's session-link grouping inside a task drawer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionRole {
    /// Stateless one-shot — figures out the project, asks for
    /// human disambiguation when ambiguous, promotes to Ready.
    Triage,
    /// Long-lived per-task brain. Owns model/permission/effort
    /// choices, spawns coders/reviewers, decides escalations.
    Orchestrator,
    /// Worker — writes code in the task's worktree.
    Coder,
    /// Worker — reviews the coder's diff and posts findings.
    Reviewer,
    /// One-shot — populates `project_memory` on first project use.
    MemorySeeder,
    /// One-shot — refines `project_memory` after a task is Done.
    MemoryUpdater,
}

impl SessionRole {
    pub fn as_str(self) -> &'static str {
        match self {
            SessionRole::Triage => "triage",
            SessionRole::Orchestrator => "orchestrator",
            SessionRole::Coder => "coder",
            SessionRole::Reviewer => "reviewer",
            SessionRole::MemorySeeder => "memory_seeder",
            SessionRole::MemoryUpdater => "memory_updater",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        Some(match s {
            "triage" => SessionRole::Triage,
            "orchestrator" => SessionRole::Orchestrator,
            "coder" => SessionRole::Coder,
            "reviewer" => SessionRole::Reviewer,
            "memory_seeder" => SessionRole::MemorySeeder,
            "memory_updater" => SessionRole::MemoryUpdater,
            _ => return None,
        })
    }

    /// `true` for roles that are allowed to call orchestrator-MCP
    /// tools. Workers (coder, reviewer) are **not** — they only
    /// see the regular flowstate MCP surface.
    pub fn is_orchestrator_audience(self) -> bool {
        matches!(
            self,
            SessionRole::Triage
                | SessionRole::Orchestrator
                | SessionRole::MemorySeeder
                | SessionRole::MemoryUpdater
        )
    }
}

/// Comment authors. `user` is the human; the rest are agent
/// personas matched to `SessionRole`. `system` is for automated
/// notes (state transitions, merge SHAs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CommentAuthor {
    User,
    Triage,
    Orchestrator,
    Reviewer,
    Coder,
    System,
}

impl CommentAuthor {
    pub fn as_str(self) -> &'static str {
        match self {
            CommentAuthor::User => "user",
            CommentAuthor::Triage => "triage",
            CommentAuthor::Orchestrator => "orchestrator",
            CommentAuthor::Reviewer => "reviewer",
            CommentAuthor::Coder => "coder",
            CommentAuthor::System => "system",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        Some(match s {
            "user" => CommentAuthor::User,
            "triage" => CommentAuthor::Triage,
            "orchestrator" => CommentAuthor::Orchestrator,
            "reviewer" => CommentAuthor::Reviewer,
            "coder" => CommentAuthor::Coder,
            "system" => CommentAuthor::System,
            _ => return None,
        })
    }
}

/// A kanban task row.
///
/// `body` carries the raw user text plus any triage-inferred notes
/// stored as a structured blob (free-text-friendly — the orchestrator
/// reads it via `task_get` and decides how to use it).
///
/// `project_id` is `None` until triage tags it. `worktree_project_id`
/// and `branch` are `None` until the orchestrator spawns the first
/// coder via `task_spawn_worker`. `orchestrator_session_id` is `None`
/// until the orchestrator session is spawned at the Ready → Code
/// boundary.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Task {
    pub task_id: String,
    pub title: String,
    pub body: String,
    pub state: TaskState,
    pub project_id: Option<String>,
    pub worktree_project_id: Option<String>,
    pub branch: Option<String>,
    pub orchestrator_session_id: Option<String>,
    /// Only populated when `state == NeedsHuman`. Reset to `None`
    /// when the human resolves and the loop moves the task forward.
    pub needs_human_reason: Option<String>,
    /// Unix seconds.
    pub created_at: i64,
    /// Unix seconds. Bumped on every mutation.
    pub updated_at: i64,
}

/// Comment thread row.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskComment {
    pub comment_id: String,
    pub task_id: String,
    pub author: CommentAuthor,
    pub body: String,
    pub created_at: i64,
}

/// Link between a kanban task and a flowstate session.
///
/// One task may have many sessions: a triage one-shot, an
/// orchestrator (long-lived), several coder/reviewer workers across
/// iterations, and one or both memory agents. `retired_at` is set
/// when the session is no longer active for the task — either it
/// completed (one-shots) or the task hit a terminal state
/// (orchestrator, on Done/Cancelled).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskSession {
    pub session_id: String,
    pub task_id: String,
    pub role: SessionRole,
    pub created_at: i64,
    /// Unix seconds, `None` while active.
    pub retired_at: Option<i64>,
}

/// Per-project memory — concise, structured context that triage
/// and orchestrator agents read when reasoning about a new task.
///
/// Auto-seeded by the `MemorySeeder` agent on first project use;
/// refined by the `MemoryUpdater` agent after each `Done`; freely
/// editable by the user via the UI. All array fields are stored as
/// JSON strings in SQLite — we decode at read time.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectMemory {
    pub project_id: String,
    /// One-paragraph "what this project is for".
    pub purpose: Option<String>,
    /// e.g. `["rust", "typescript"]`.
    pub languages: Vec<String>,
    /// Notable top-level dirs with one-line notes.
    pub key_directories: Vec<KeyDirectory>,
    /// House rules — naming conventions, "always use mise", "no
    /// emoji in code". Free-form strings.
    pub conventions: Vec<String>,
    /// FIFO of recent task themes, capped at 10 by the updater.
    pub recent_task_themes: Vec<String>,
    /// Unix seconds when the seeder first wrote a row, `None` if
    /// the row was hand-created by the user.
    pub seeded_at: Option<i64>,
    /// Unix seconds — bumped on every mutation.
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyDirectory {
    pub path: String,
    pub note: String,
}

/// Settings keyed by string. Used for the feature flag, tick
/// enabled toggle, tick interval, etc. Single table keeps wiring
/// simple — there are only a handful of orchestrator-level knobs.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrchestratorSetting {
    pub key: String,
    pub value: String,
    pub updated_at: i64,
}

/// Known setting keys. Centralized so handlers and the tick task
/// agree on the wire names.
pub mod settings_keys {
    /// `"true"` / `"false"`. Default `"false"` (feature OFF by
    /// default). When false, the orchestrator HTTP routes 404,
    /// and the front-end hides the kanban UI.
    pub const FEATURE_ENABLED: &str = "feature_enabled";
    /// `"true"` / `"false"`. The user-facing toggle in the
    /// orchestrator window. When true and the feature flag is
    /// true, the tick loop ticks at `TICK_INTERVAL_MS`.
    pub const TICK_ENABLED: &str = "tick_enabled";
    /// Stringified integer milliseconds. Default `"10000"`.
    pub const TICK_INTERVAL_MS: &str = "tick_interval_ms";
    /// Maximum number of tasks the orchestrator will hold in
    /// "active spawn" states (Code / AgentReview / Merge) at once.
    /// Tasks past the limit stay in earlier states until a slot
    /// frees up. Default `"3"` — tuned for a single-developer
    /// workstation where parallel agents compete for CPU + provider
    /// rate limits.
    pub const MAX_PARALLEL_TASKS: &str = "max_parallel_tasks";
}
