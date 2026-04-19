//! Cross-thread / cross-project agent-orchestration contract.
//!
//! Defines the abstract surface an agent uses to reach other sessions:
//! spawn a new thread (optionally in a different project), deliver a
//! message to an existing thread, poll for replies, and discover peers.
//! The types are deliberately closed enums — every new action is a new
//! variant + a matching tool schema in [`capabilities`], and every
//! provider adapter relays the same wire shape so orchestration never
//! lives inside a specific provider's bridge logic.
//!
//! `RuntimeCallDispatcher` is the trait `runtime-core` implements; this
//! crate stays dependency-thin so the provider-api layer can parse /
//! encode the calls without pulling in the full runtime.

use serde::{Deserialize, Serialize};

use crate::{ProjectRecord, ProviderKind, SessionDetail, SessionSummary};

/// Lean peek at a session — what the agent gets back from
/// `flowstate_list_sessions`. Wraps [`SessionSummary`] with short
/// previews so the model can pick the right thread without a follow-up
/// `flowstate_read_session` call. Previews are truncated to 200 chars
/// and stripped of newlines so they fit in a JSON tool output.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-bindings", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-bindings", ts(optional_fields))]
#[serde(rename_all = "camelCase")]
pub struct SessionDigest {
    pub summary: SessionSummary,
    /// User-set display title (what shows in the sidebar), sourced
    /// from the host app's display layer. `None` when the user hasn't
    /// renamed the session or when the host doesn't expose titles to
    /// the runtime — consumers fall back to `firstInputPreview`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// User-set display name for the session's project. Same shape as
    /// `title` — host-app display metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_name: Option<String>,
    /// Resolved project path (joined from `summary.project_id` →
    /// `ProjectRecord.path`). Gives the agent a human-readable anchor
    /// for "which project is this?" without a second lookup.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_path: Option<String>,
    /// First turn's user-message input, truncated. Serves as a
    /// disambiguator when `title` is unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_input_preview: Option<String>,
    /// Last turn's final assistant reply, truncated. Useful when the
    /// agent wants to catch up on what the peer was last doing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_output_preview: Option<String>,
}

const PREVIEW_CHAR_CAP: usize = 200;

impl SessionDigest {
    /// Build a digest from the parts the dispatcher already has on
    /// hand. `project_path` is looked up by the caller (the runtime
    /// has the projects table); `title` / `project_name` come from
    /// the app-layer metadata resolver when one is installed.
    pub fn from_parts(
        summary: SessionSummary,
        title: Option<String>,
        project_name: Option<String>,
        project_path: Option<String>,
        first_input: Option<&str>,
        last_output: Option<&str>,
    ) -> Self {
        Self {
            summary,
            title,
            project_name,
            project_path,
            first_input_preview: first_input.map(truncate_preview),
            last_output_preview: last_output.map(truncate_preview),
        }
    }
}

fn truncate_preview(raw: &str) -> String {
    let trimmed: String = raw
        .chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if trimmed.chars().count() <= PREVIEW_CHAR_CAP {
        trimmed
    } else {
        let cut: String = trimmed.chars().take(PREVIEW_CHAR_CAP).collect();
        format!("{cut}…")
    }
}

/// Identifies the session that issued a `RuntimeCall`. Stamped by the
/// adapter from the current turn — the agent never has to spell its
/// own id in the tool call args.
#[derive(Debug, Clone)]
pub struct RuntimeCallOrigin {
    pub session_id: String,
    pub turn_id: String,
}

/// The set of cross-session actions an agent can ask the runtime to
/// perform. Closed enum on purpose.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RuntimeCall {
    /// Create a brand-new session (optionally in a different project),
    /// send an initial user message, block until the target session
    /// produces its next final assistant reply. Returns the new session
    /// id and the reply text.
    SpawnAndAwait {
        #[serde(default)]
        project_id: Option<String>,
        #[serde(default)]
        provider: Option<ProviderKind>,
        #[serde(default)]
        model: Option<String>,
        initial_message: String,
        #[serde(default)]
        timeout_secs: Option<u64>,
    },
    /// Fire-and-forget spawn. Returns the new `session_id` as soon as
    /// the first turn is scheduled; the caller can `Poll` later.
    Spawn {
        #[serde(default)]
        project_id: Option<String>,
        #[serde(default)]
        provider: Option<ProviderKind>,
        #[serde(default)]
        model: Option<String>,
        initial_message: String,
    },
    /// Deliver a message to an existing session and block until that
    /// session's next final assistant reply.
    SendAndAwait {
        session_id: String,
        message: String,
        #[serde(default)]
        timeout_secs: Option<u64>,
    },
    /// Fire-and-forget delivery. If the target is idle, runtime
    /// schedules a turn immediately; otherwise the message is queued
    /// for delivery at the next turn boundary.
    Send { session_id: String, message: String },
    /// Peek at the newest completed reply from `session_id` that post-
    /// dates `since_turn_id` (or the most recent, if `None`).
    Poll {
        session_id: String,
        #[serde(default)]
        since_turn_id: Option<String>,
    },
    /// Read-only snapshot — the session summary plus the most recent
    /// `last_turns` turns (all turns if `None`).
    ReadSession {
        session_id: String,
        #[serde(default)]
        last_turns: Option<u32>,
    },
    /// Discovery. The dispatcher decides curation (pinned + same-project
    /// + spawned descendants + spawner, typically).
    ListSessions {
        #[serde(default)]
        project_id: Option<String>,
    },
    /// Discovery for projects. Agents call this when the user says
    /// "use my other project" and the agent doesn't already know the
    /// project_id. Returns every persisted project record.
    ListProjects,
    /// Create a git worktree off an existing project. Returns a
    /// worktree blueprint (new project id + on-disk path + branch).
    /// Host app must have installed a `WorktreeProvisioner`; otherwise
    /// this returns `Internal` with "worktree support not available".
    CreateWorktree {
        base_project_id: String,
        branch: String,
        #[serde(default)]
        base_ref: Option<String>,
        #[serde(default)]
        create_branch: Option<bool>,
    },
    /// List worktrees. When `base_project_id` is set, restrict to
    /// worktrees rooted at that project; otherwise list them all.
    ListWorktrees {
        #[serde(default)]
        base_project_id: Option<String>,
    },
    /// Convenience combo: create a worktree, then spawn a session
    /// inside it with `initial_message`. If `await_reply` is true,
    /// block until the new session produces its first assistant reply
    /// (same contract as `SpawnAndAwait`).
    SpawnInWorktree {
        base_project_id: String,
        branch: String,
        #[serde(default)]
        base_ref: Option<String>,
        #[serde(default)]
        create_branch: Option<bool>,
        initial_message: String,
        #[serde(default)]
        provider: Option<ProviderKind>,
        #[serde(default)]
        model: Option<String>,
        #[serde(default)]
        await_reply: Option<bool>,
        #[serde(default)]
        timeout_secs: Option<u64>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RuntimeCallResult {
    Spawned {
        session_id: String,
        reply: Option<String>,
    },
    SpawnedAsync {
        session_id: String,
    },
    Sent {
        reply: Option<String>,
    },
    SentAsync,
    Poll(PollOutcome),
    Session(SessionDetail),
    // Struct variants below (not newtype). serde's internally-tagged
    // representation (`#[serde(tag = "kind")]`) does not support newtype
    // variants whose inner type serializes as a sequence — a `Vec` has
    // no room for the `kind` discriminator to ride alongside it. Wrapping
    // the list in a field sidesteps the restriction and keeps the wire
    // shape predictable for the bridge.
    Sessions {
        sessions: Vec<SessionDigest>,
    },
    Projects {
        projects: Vec<ProjectRecord>,
    },
    Worktree(WorktreeSummary),
    Worktrees {
        worktrees: Vec<WorktreeSummary>,
    },
    /// Result of `SpawnInWorktree`. Carries both the worktree metadata
    /// and the spawned session's id (+ reply if `await_reply` was true).
    SpawnedInWorktree {
        worktree: WorktreeSummary,
        session_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reply: Option<String>,
    },
}

/// Wire-shape mirror of `runtime_core::WorktreeBlueprint`. Kept in
/// `provider-api` so the orchestration result types don't have to
/// reach into runtime-core.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-bindings", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-bindings", ts(optional_fields))]
#[serde(rename_all = "camelCase")]
pub struct WorktreeSummary {
    pub project_id: String,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_project_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum PollOutcome {
    Pending,
    Ready { reply: String, turn_id: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, thiserror::Error)]
#[serde(tag = "code", rename_all = "snake_case")]
pub enum RuntimeCallError {
    #[error("session `{session_id}` not found")]
    SessionNotFound { session_id: String },
    #[error("project `{project_id}` not found")]
    ProjectNotFound { project_id: String },
    #[error("provider `{provider}` is disabled or unknown")]
    ProviderDisabled { provider: String },
    #[error("cycle detected — session `{session_id}` is already waiting on the caller")]
    Cycle { session_id: String },
    #[error("orchestration budget exceeded for this turn")]
    BudgetExceeded,
    #[error("timed out waiting for session `{session_id}` reply")]
    Timeout { session_id: String },
    #[error("operation cancelled")]
    Cancelled,
    #[error("permission denied")]
    PermissionDenied,
    #[error("internal runtime error: {message}")]
    Internal { message: String },
}

/// Implemented by `runtime-core::RuntimeCore`. Kept here so the provider-
/// api layer can parse / encode calls without depending on the runtime.
/// The async trait is `Send + Sync` because dispatchers are held behind
/// an `Arc` and shared across per-session sink clones.
#[async_trait::async_trait]
pub trait RuntimeCallDispatcher: Send + Sync {
    async fn dispatch(
        &self,
        origin: RuntimeCallOrigin,
        call: RuntimeCall,
    ) -> Result<RuntimeCallResult, RuntimeCallError>;
}

/// Marker describing who created a session. Referenced from the
/// `SessionLinked` runtime event and used by the dispatcher's awaiting-
/// graph root. Not persisted onto `SessionSummary` in this iteration —
/// the UI derives "spawned by" from the live event stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionCreator {
    User,
    Agent { session_id: String },
}

impl Default for SessionCreator {
    fn default() -> Self {
        SessionCreator::User
    }
}
