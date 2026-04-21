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

use crate::{PermissionMode, ProjectRecord, ProviderKind, ReasoningEffort, SessionDetail, SessionSummary};

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
        /// Permission mode applied to the spawned session's opening
        /// turn. `None` preserves the historical behavior
        /// (`PermissionMode::Default`); callers opt in when they want
        /// the sub-agent to run with looser/stricter permissions than
        /// the strictest default.
        #[serde(default)]
        permission_mode: Option<PermissionMode>,
        /// Reasoning effort for the opening turn. `None` preserves the
        /// historical behavior (no effort override); only honoured by
        /// providers whose `ProviderFeatures.thinking_effort` is true.
        #[serde(default)]
        reasoning_effort: Option<ReasoningEffort>,
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
        /// See `SpawnAndAwait::permission_mode`.
        #[serde(default)]
        permission_mode: Option<PermissionMode>,
        /// See `SpawnAndAwait::reasoning_effort`.
        #[serde(default)]
        reasoning_effort: Option<ReasoningEffort>,
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
        /// See `SpawnAndAwait::permission_mode`.
        #[serde(default)]
        permission_mode: Option<PermissionMode>,
        /// See `SpawnAndAwait::reasoning_effort`.
        #[serde(default)]
        reasoning_effort: Option<ReasoningEffort>,
        #[serde(default)]
        await_reply: Option<bool>,
        #[serde(default)]
        timeout_secs: Option<u64>,
    },
    /// Introspection: enumerate every provider the runtime knows
    /// about along with its available models, per-model reasoning
    /// effort levels, and the permission modes / efforts the wire
    /// type supports. Agents call this before a `spawn` when they're
    /// uncertain about the right provider/model string — especially
    /// useful for opencode where model ids look like
    /// `opencode/kimi-k2.5` and are easy to typo.
    ///
    /// No arguments — the result is always the full catalog. If an
    /// agent wants only the enabled ones, filter on `enabled` in the
    /// response. Flowstate does not paginate; the catalog is a
    /// handful of providers and typically <200 models total, well
    /// within a single tool response budget.
    ListProviders,
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
    /// Result of `ListProviders`. One [`ProviderCatalogEntry`] per
    /// registered provider, plus the wire-level enum vocabularies
    /// (permission modes, reasoning efforts) an agent can use on
    /// `spawn` / `spawn_and_await` / `spawn_in_worktree`. Keeping
    /// the wire vocabularies in the same payload saves a second
    /// round-trip — the agent always has everything it needs to
    /// construct a well-formed spawn call.
    Providers {
        providers: Vec<ProviderCatalogEntry>,
        /// Every `permission_mode` value the `spawn*` tools accept.
        /// Derived from [`crate::PermissionMode::ALL`] so it can never
        /// drift from the enum.
        permission_modes: Vec<String>,
        /// Every `reasoning_effort` value the `spawn*` tools accept.
        /// Derived from [`crate::ReasoningEffort::ALL`]. Per-model
        /// effort support is reported inside each
        /// `ProviderCatalogEntry`'s models.
        reasoning_efforts: Vec<String>,
    },
}

/// Per-provider snapshot returned by `RuntimeCall::ListProviders`.
/// Enough for an agent to pick a `provider` + `model` + (optional)
/// `reasoning_effort` / `permission_mode` without a separate docs
/// round-trip. Mirrors the shape of [`crate::ProviderStatus`] the
/// frontend already renders in its settings panel, narrowed to what
/// an agent actually needs to make a spawn decision.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-bindings", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-bindings", ts(optional_fields))]
pub struct ProviderCatalogEntry {
    /// Wire-tag for the `spawn*` tools' `provider` field — exactly
    /// what should go into `spawn(provider="…")`.
    pub kind: crate::ProviderKind,
    /// Human-readable display name (e.g. "GitHub Copilot"). For log
    /// lines + tool-output narration, not for passing back to the
    /// runtime.
    pub label: String,
    /// Whether the user has this provider enabled. Disabled providers
    /// are still listed so agents can suggest "enable X in settings
    /// to use model Y" rather than silently omitting them.
    pub enabled: bool,
    /// Whether the provider is installed + authenticated. Pulled
    /// from [`crate::ProviderStatusLevel`]: `Ready` = good to spawn,
    /// `Warning` = usually spawns but may misbehave, `Error` = don't
    /// try. Emitted as a stable lowercase tag so the agent can
    /// exact-match.
    pub status: String,
    /// Optional diagnostic from the health check — reason a provider
    /// isn't `Ready` (e.g. "binary not on PATH", "auth missing"). Use
    /// verbatim in error messages.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_message: Option<String>,
    /// Feature flags. Agents should gate optional spawn args on
    /// these — e.g. omit `reasoning_effort` for providers where
    /// `thinking_effort == false`.
    pub features: crate::ProviderFeatures,
    /// Models the provider advertises. Each entry is a
    /// [`crate::ProviderModel`] carrying: `value` (the wire string
    /// for `spawn(model="…")`), `label` (display name), plus the
    /// per-model effort level list — the model-specific filter over
    /// the provider-level `reasoning_efforts` list.
    pub models: Vec<crate::ProviderModel>,
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
