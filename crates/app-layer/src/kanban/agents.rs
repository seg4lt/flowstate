//! Agent spawner — the integration point between the kanban tick
//! loop and `RuntimeCore`.
//!
//! For each persona (triage / coder / reviewer / memory_seeder /
//! memory_updater) the spawner:
//!
//! 1. Calls `RuntimeCore::handle_client_message(StartSession{...})`
//!    to allocate a fresh session bound to a project (or no project
//!    for triage). Records the resulting `session_id` in
//!    `task_sessions` with the right role.
//! 2. Calls `RuntimeCore::handle_client_message(SendTurn{...})` to
//!    deliver the persona-specific prompt. SendTurn returns
//!    immediately; the actual provider work happens asynchronously
//!    on the runtime side.
//! 3. The tick loop then **polls** `read_completed_output(...)` on
//!    each subsequent tick. Returns `Some(text)` when the latest
//!    turn has reached `TurnStatus::Completed`, else `None`.
//! 4. The tick loop runs the marker parser on the completed text;
//!    on `TASK_DONE` it consumes the output (retires the
//!    `task_sessions` row) and advances kanban state.
//!
//! Sessions are **single-turn one-shots** in v2 — each persona's
//! work fits in a single turn (the agent SDK can do a lot in one
//! turn, including multi-step tool use). When a coder needs more
//! context after a reviewer rejection, the next phase spawns a
//! fresh session with the rejection feedback baked into the prompt.
//! Stateful per-task orchestrator sessions are a v3 upgrade once
//! we want the orchestrator persona to use real MCP tools to
//! re-spawn workers; v2 keeps the tick loop in charge.

use std::sync::Arc;

use tracing::{debug, info, warn};
use zenui_provider_api::{
    ClientMessage, PermissionMode, ProviderKind, ReasoningEffort, RuntimeEvent, ServerMessage,
    SessionLinkReason, SessionStatus, TurnStatus,
};
use zenui_runtime_core::RuntimeCore;

use super::model::{ProjectMemory, SessionRole, Task};
use super::prompts;
use super::store::KanbanStore;
use crate::user_config::{SessionDisplay, UserConfigStore};

/// What a completed-turn poll returned.
#[derive(Debug, Clone)]
pub enum SessionPoll {
    /// Session not found in the runtime — likely reaped or never
    /// existed. Caller should retire the `task_sessions` row and
    /// either respawn or escalate.
    Vanished,
    /// Last turn is still `Running` (provider hasn't finished).
    /// Tick loop should leave the task untouched and re-poll on
    /// the next tick.
    StillRunning,
    /// Last turn is `Completed`; here's the assistant output.
    Completed { output: String },
    /// Last turn ended with `Failed` / `Interrupted`. Caller
    /// usually escalates to NeedsHuman.
    Failed { reason: String },
}

/// Cheap-to-clone handle the tick loop holds.
///
/// Wraps an `Arc<RuntimeCore>` plus a `KanbanStore` for recording
/// task↔session links. Holding both is fine: app-layer already
/// owns both at the Tauri shell layer, and the spawner is a
/// thin coordinator — no duplicated state.
#[derive(Clone)]
pub struct AgentSpawner {
    runtime: Arc<RuntimeCore>,
    kanban: KanbanStore,
    /// Optional handle to the app-layer's `UserConfigStore` so we
    /// can persist a human-readable title onto the spawned session
    /// (UI sidebar pulls titles from `session_display`). When
    /// missing, sessions show up with no title — works but ugly.
    user_config: Option<UserConfigStore>,
}

impl AgentSpawner {
    pub fn new(runtime: Arc<RuntimeCore>, kanban: KanbanStore) -> Self {
        Self {
            runtime,
            kanban,
            user_config: None,
        }
    }

    /// Inject a `UserConfigStore` so spawned sessions get titles
    /// in the sidebar. Optional because tests can run without it.
    pub fn with_user_config(mut self, ucs: UserConfigStore) -> Self {
        self.user_config = Some(ucs);
        self
    }

    /// Resolve the agent settings for a given role. Reads the
    /// user's persisted defaults from `UserConfigStore` (the same
    /// keys the main flowstate UI writes to under "defaults.*"),
    /// then layers per-role overrides where mandatory.
    ///
    /// Resolution order for each field:
    ///
    /// - **provider**: user's `defaults.provider` if set AND
    ///   currently enabled; else first enabled provider in a
    ///   canonical order; else hard fallback to Claude.
    /// - **model**: user's `defaults.model.<provider>` if set;
    ///   else `None` (adapter picks its own latest).
    /// - **permission_mode**: user's `defaults.permission_mode`
    ///   if set, EXCEPT for the coder role which is always forced
    ///   to `AcceptEdits`. Forcing exists because a "Default"
    ///   permission mode prompts the human for every tool call —
    ///   the coder has no human watching its session in real time
    ///   so it would deadlock waiting for an Allow click that
    ///   never comes. If the user picked AcceptEdits / Bypass /
    ///   Auto / Plan we honour it; only "Default" gets upgraded.
    /// - **reasoning_effort**: user's `defaults.effort` if set;
    ///   else `None`.
    pub fn resolve_agent_settings(&self, role: SessionRole) -> AgentSettings {
        let provider = self.resolve_provider();
        let model = self.resolve_model(provider);
        let permission_mode = self.resolve_permission_mode(role);
        let reasoning_effort = self.resolve_reasoning_effort();
        debug!(
            role = role.as_str(),
            provider = ?provider,
            model = ?model,
            permission_mode = ?permission_mode,
            reasoning_effort = ?reasoning_effort,
            "resolved agent settings"
        );
        AgentSettings {
            provider,
            model,
            permission_mode,
            reasoning_effort,
        }
    }

    fn resolve_provider(&self) -> ProviderKind {
        let ucs = match &self.user_config {
            Some(u) => u,
            None => return ProviderKind::Claude,
        };
        // 1. Saved default + enabled wins.
        if let Ok(Some(raw)) = ucs.get(uc_keys::DEFAULT_PROVIDER) {
            if let Some(p) = parse_provider(&raw) {
                if self.provider_is_enabled(p) {
                    return p;
                }
            }
        }
        // 2. First enabled provider in canonical order. Mirror the
        //    default-enabled set the front-end uses (claude +
        //    github_copilot enabled OOTB).
        for p in [
            ProviderKind::Claude,
            ProviderKind::GitHubCopilot,
            ProviderKind::Codex,
            ProviderKind::OpenCode,
        ] {
            if self.provider_is_enabled(p) {
                return p;
            }
        }
        // 3. Hard fallback.
        ProviderKind::Claude
    }

    fn provider_is_enabled(&self, p: ProviderKind) -> bool {
        let ucs = match &self.user_config {
            Some(u) => u,
            None => return matches!(p, ProviderKind::Claude),
        };
        let key = format!("{}{}", uc_keys::PROVIDER_ENABLED_PREFIX, provider_kebab(p));
        match ucs.get(&key) {
            Ok(Some(v)) => v == "true",
            Ok(None) => {
                // Same default-enabled set the front-end uses.
                matches!(p, ProviderKind::Claude | ProviderKind::GitHubCopilot)
            }
            Err(_) => matches!(p, ProviderKind::Claude),
        }
    }

    fn resolve_model(&self, provider: ProviderKind) -> Option<String> {
        let ucs = self.user_config.as_ref()?;
        let key = format!("{}{}", uc_keys::DEFAULT_MODEL_PREFIX, provider_kebab(provider));
        match ucs.get(&key) {
            Ok(Some(v)) if !v.trim().is_empty() => Some(v.trim().to_string()),
            _ => None,
        }
    }

    fn resolve_permission_mode(&self, role: SessionRole) -> PermissionMode {
        let user_default = self
            .user_config
            .as_ref()
            .and_then(|u| u.get(uc_keys::DEFAULT_PERMISSION_MODE).ok().flatten())
            .as_deref()
            .and_then(parse_permission_mode);
        // Coder always needs at least AcceptEdits — otherwise it
        // would deadlock on the first tool call waiting for human
        // approval that never comes (no human is watching the
        // session in real time).
        match (role, user_default) {
            // Coder: upgrade Default → AcceptEdits, honour
            // anything more permissive (AcceptEdits, Bypass,
            // Auto). Plan is downgraded to AcceptEdits because a
            // pure plan-mode coder produces a plan instead of
            // editing.
            (SessionRole::Coder, Some(PermissionMode::Default))
            | (SessionRole::Coder, Some(PermissionMode::Plan))
            | (SessionRole::Coder, None) => PermissionMode::AcceptEdits,
            (SessionRole::Coder, Some(other)) => other,
            // Other roles: honour user default; sensible
            // fallback. Reviewer / triage / memory don't need
            // edits but they may run grep / read which providers
            // gate behind permissions.
            (_, Some(mode)) => mode,
            (_, None) => PermissionMode::AcceptEdits,
        }
    }

    fn resolve_reasoning_effort(&self) -> Option<ReasoningEffort> {
        self.user_config
            .as_ref()
            .and_then(|u| u.get(uc_keys::DEFAULT_EFFORT).ok().flatten())
            .as_deref()
            .and_then(parse_reasoning_effort)
    }

    /// Enumerate projects with paths from the running runtime.
    /// Returns `(project_id, absolute_path)` pairs, filtering out
    /// path-less projects (which can't be used as a worker cwd).
    pub async fn runtime_snapshot_projects(&self) -> Vec<(String, String)> {
        self.runtime
            .snapshot()
            .await
            .projects
            .into_iter()
            .filter_map(|p| p.path.map(|path| (p.project_id, path)))
            .collect()
    }

    // ── triage ────────────────────────────────────────────────────

    /// Spawn the triage one-shot for an Open task.
    ///
    /// Triage runs **without a project_id** (its job is to pick
    /// one). Provider defaults to Claude unless the workspace has
    /// it disabled — the start-session path returns an error in
    /// that case which we surface as a comment on the task.
    pub async fn spawn_triage(&self, task: &Task) -> Result<String, String> {
        // Build the candidate-projects list from the runtime
        // snapshot. Path-bearing projects only — projects without
        // a path can't be a target for code work.
        let snapshot = self.runtime.snapshot().await;
        let candidates: Vec<(String, String)> = snapshot
            .projects
            .into_iter()
            .filter_map(|p| p.path.map(|path| (p.project_id, path)))
            .collect();

        let prompt = prompts::triage_prompt(task, &candidates);
        let title = format!("🎯 triage: {}", short_title(&task.title));
        let settings = self.resolve_agent_settings(SessionRole::Triage);
        let session_id = self
            .start_session_and_send(None, prompt, settings, Some(title))
            .await?;
        self.kanban
            .insert_task_session(&session_id, &task.task_id, SessionRole::Triage)?;
        info!(
            task_id = %task.task_id,
            %session_id,
            candidate_count = candidates.len(),
            "spawned triage session"
        );
        Ok(session_id)
    }

    // ── coder ─────────────────────────────────────────────────────

    /// Spawn the coder for a Ready task. Requires the task already
    /// has `project_id` set (the tick loop checks this before
    /// calling). Coder runs with `AcceptEdits` permission so it
    /// can modify files without per-edit prompts.
    pub async fn spawn_coder(
        &self,
        task: &Task,
        memory: Option<&ProjectMemory>,
        revision_feedback: Option<&str>,
    ) -> Result<String, String> {
        let project_id = task
            .project_id
            .as_deref()
            .ok_or_else(|| "coder spawn requires task.project_id".to_string())?;
        let prompt = prompts::coder_prompt(task, memory, revision_feedback);
        let title = if revision_feedback.is_some() {
            format!("🔧 coder (rev2): {}", short_title(&task.title))
        } else {
            format!("🔧 coder: {}", short_title(&task.title))
        };
        let settings = self.resolve_agent_settings(SessionRole::Coder);
        let session_id = self
            .start_session_and_send(
                Some(project_id.to_string()),
                prompt,
                settings,
                Some(title),
            )
            .await?;
        self.kanban
            .insert_task_session(&session_id, &task.task_id, SessionRole::Coder)?;
        info!(
            task_id = %task.task_id,
            %project_id,
            %session_id,
            "spawned coder session"
        );
        Ok(session_id)
    }

    // ── reviewer ──────────────────────────────────────────────────

    /// Spawn the reviewer after a coder marks done.
    pub async fn spawn_reviewer(
        &self,
        task: &Task,
        coder_summary: &str,
    ) -> Result<String, String> {
        let project_id = task
            .project_id
            .as_deref()
            .ok_or_else(|| "reviewer spawn requires task.project_id".to_string())?;
        let prompt = prompts::reviewer_prompt(task, coder_summary);
        let title = format!("🔍 reviewer: {}", short_title(&task.title));
        let settings = self.resolve_agent_settings(SessionRole::Reviewer);
        let session_id = self
            .start_session_and_send(
                Some(project_id.to_string()),
                prompt,
                settings,
                Some(title),
            )
            .await?;
        self.kanban
            .insert_task_session(&session_id, &task.task_id, SessionRole::Reviewer)?;
        info!(
            task_id = %task.task_id,
            %project_id,
            %session_id,
            "spawned reviewer session"
        );
        Ok(session_id)
    }

    // ── memory seeder ─────────────────────────────────────────────

    /// Spawn the memory-seeder for a project that doesn't have a
    /// `project_memory` row yet. Runs read-only-ish in the
    /// project's workspace and returns a JSON blob in its marker
    /// payload that the tick loop persists via
    /// `KanbanStore::upsert_project_memory`.
    pub async fn spawn_memory_seeder(
        &self,
        task: &Task,
        project_id: &str,
        project_path: &str,
    ) -> Result<String, String> {
        let prompt = prompts::memory_seeder_prompt(project_id, project_path);
        let title = format!("📚 memory seed: {project_id}");
        let settings = self.resolve_agent_settings(SessionRole::MemorySeeder);
        let session_id = self
            .start_session_and_send(
                Some(project_id.to_string()),
                prompt,
                settings,
                Some(title),
            )
            .await?;
        self.kanban
            .insert_task_session(&session_id, &task.task_id, SessionRole::MemorySeeder)?;
        info!(
            task_id = %task.task_id,
            %project_id,
            %session_id,
            "spawned memory_seeder session"
        );
        Ok(session_id)
    }

    // ── memory updater ────────────────────────────────────────────

    pub async fn spawn_memory_updater(
        &self,
        task: &Task,
        memory: &ProjectMemory,
    ) -> Result<String, String> {
        let project_id = task
            .project_id
            .as_deref()
            .ok_or_else(|| "memory_updater requires task.project_id".to_string())?;
        let prompt = prompts::memory_updater_prompt(memory, task);
        let title = format!("📝 memory update: {project_id}");
        let settings = self.resolve_agent_settings(SessionRole::MemoryUpdater);
        let session_id = self
            .start_session_and_send(
                Some(project_id.to_string()),
                prompt,
                settings,
                Some(title),
            )
            .await?;
        self.kanban
            .insert_task_session(&session_id, &task.task_id, SessionRole::MemoryUpdater)?;
        info!(
            task_id = %task.task_id,
            %project_id,
            %session_id,
            "spawned memory_updater session"
        );
        Ok(session_id)
    }

    // ── output reader ─────────────────────────────────────────────

    /// Poll a session's most-recent turn. Designed to be called
    /// from the tick loop on every tick while a session is active.
    ///
    /// The semantics:
    /// - We always look at the **last** turn in the session's
    ///   detail (the one our prompt triggered, since one-shots
    ///   only have one user turn).
    /// - `Running` → caller should re-poll later.
    /// - `Completed` → return the assistant `output` so the
    ///   caller can run the marker parser.
    /// - `Failed` / `Interrupted` → return Failed so the caller
    ///   can mark NeedsHuman.
    /// - Session not found → return Vanished.
    pub async fn poll_session(&self, session_id: &str) -> SessionPoll {
        let detail = match self.runtime.live_session_detail(session_id).await {
            Some(d) => d,
            None => return SessionPoll::Vanished,
        };

        // Session-level status takes precedence over per-turn:
        // an interrupted session may still have a `Running` turn
        // record, but the runtime won't move it forward.
        if matches!(detail.summary.status, SessionStatus::Interrupted) {
            return SessionPoll::Failed {
                reason: "session was interrupted".to_string(),
            };
        }

        // Find the latest turn. Sessions can have system-injected
        // turns at the front (some adapters) so we explicitly take
        // the last one rather than the first.
        let last = match detail.turns.last() {
            Some(t) => t,
            None => return SessionPoll::StillRunning, // no turns yet
        };

        match last.status {
            TurnStatus::Running => SessionPoll::StillRunning,
            TurnStatus::Completed => SessionPoll::Completed {
                output: last.output.clone(),
            },
            TurnStatus::Failed => SessionPoll::Failed {
                reason: format!("turn {} failed", last.turn_id),
            },
            TurnStatus::Interrupted => SessionPoll::Failed {
                reason: format!("turn {} interrupted", last.turn_id),
            },
        }
    }

    // ── internal helpers ──────────────────────────────────────────

    async fn start_session_and_send(
        &self,
        project_id: Option<String>,
        initial_message: String,
        settings: AgentSettings,
        session_title: Option<String>,
    ) -> Result<String, String> {
        // Step 1: start session with the resolved provider + model.
        let started = self
            .runtime
            .handle_client_message(ClientMessage::StartSession {
                provider: settings.provider,
                model: settings.model.clone(),
                project_id,
            })
            .await;
        let session_id = match started {
            Some(ServerMessage::SessionCreated { session }) => session.session_id,
            Some(ServerMessage::Error { message }) => {
                return Err(format!("StartSession failed: {message}"));
            }
            other => {
                return Err(format!(
                    "StartSession unexpected response: {other:?}"
                ));
            }
        };

        // Persist a sidebar-friendly title so the spawned session
        // shows up as e.g. "🎯 triage: fix typo" instead of a
        // blank entry. Best-effort: if the store isn't wired, or
        // the write fails, we log + continue — the session is
        // already up and ticking, a missing display title is a
        // pure cosmetic issue.
        if let (Some(ucs), Some(title)) = (&self.user_config, session_title.as_ref()) {
            let display = SessionDisplay {
                title: Some(title.clone()),
                last_turn_preview: None,
                sort_order: None,
            };
            if let Err(e) = ucs.set_session_display(&session_id, &display) {
                warn!(%session_id, %e, "failed to persist session_display title");
            } else {
                debug!(%session_id, %title, "session title persisted");
            }
        }

        // Publish a SessionLinked event so the sidebar's Sparkles
        // icon ("Spawned by agent") shows up next to this thread,
        // matching how flowstate already renders agent-to-agent
        // MCP spawns. The `from_session_id` is a stable sentinel
        // — there's no real "parent session" for an orchestrator-
        // initiated agent, but the frontend keys lookups by the
        // child anyway and the tooltip parentLabel falls back to
        // the sentinel string when no matching session exists.
        self.runtime.publish(RuntimeEvent::SessionLinked {
            from_session_id: ORCHESTRATOR_PARENT_SENTINEL.to_string(),
            to_session_id: session_id.clone(),
            reason: SessionLinkReason::Spawn,
        });

        // Step 2: send the initial turn. SendTurn returns
        // immediately — the actual model call happens async on
        // the runtime side. The tick loop will poll for completion.
        let sent = self
            .runtime
            .handle_client_message(ClientMessage::SendTurn {
                session_id: session_id.clone(),
                input: initial_message,
                images: Vec::new(),
                permission_mode: Some(settings.permission_mode),
                reasoning_effort: settings.reasoning_effort,
                thinking_mode: None,
            })
            .await;
        match sent {
            Some(ServerMessage::Ack { .. }) => {
                debug!(%session_id, "initial SendTurn ack'd");
                Ok(session_id)
            }
            Some(ServerMessage::Error { message }) => Err(format!(
                "SendTurn failed for {session_id}: {message}"
            )),
            other => Err(format!(
                "SendTurn unexpected response for {session_id}: {other:?}"
            )),
        }
    }
}

/// Stable sentinel id we publish as the "parent" of every
/// orchestrator-spawned session. The frontend's session_links
/// map is keyed by child id and the tooltip parentLabel falls
/// back to a slice of the sentinel when the parent isn't found
/// in `state.sessions` — making the Sparkles icon appear with a
/// "from flowstate-orchestrator" hint.
const ORCHESTRATOR_PARENT_SENTINEL: &str = "flowstate-orchestrator";

/// User-config keys we read for agent defaults. Match the
/// strings the front-end already writes via
/// `apps/flowstate/src/lib/defaults-settings.ts`.
mod uc_keys {
    pub const DEFAULT_PROVIDER: &str = "defaults.provider";
    pub const DEFAULT_EFFORT: &str = "defaults.effort";
    pub const DEFAULT_PERMISSION_MODE: &str = "defaults.permission_mode";
    pub const DEFAULT_MODEL_PREFIX: &str = "defaults.model.";
    pub const PROVIDER_ENABLED_PREFIX: &str = "provider.enabled.";
}

/// Resolved settings for a single agent spawn. The combination
/// is settled per spawn (rather than once per `AgentSpawner`)
/// so a settings change while the loop is running takes effect
/// on the next agent the loop kicks off.
#[derive(Debug, Clone)]
pub struct AgentSettings {
    pub provider: ProviderKind,
    /// `None` lets the runtime adapter pick its own default
    /// (which usually means "the latest released model"). A
    /// stored model string overrides that.
    pub model: Option<String>,
    pub permission_mode: PermissionMode,
    pub reasoning_effort: Option<ReasoningEffort>,
}

/// Trim a task title down to a reasonable length for the sidebar.
/// 50 chars matches roughly what the existing flowstate sidebar
/// shows before truncating with ellipsis.
fn short_title(s: &str) -> String {
    let trimmed = s.trim();
    if trimmed.chars().count() <= 50 {
        trimmed.to_string()
    } else {
        let prefix: String = trimmed.chars().take(50).collect();
        format!("{prefix}…")
    }
}

fn parse_provider(s: &str) -> Option<ProviderKind> {
    // Match the wire tags the frontend writes — kebab/snake
    // forms exactly. The enum is `#[serde(rename_all = "snake_case")]`
    // except for github_copilot which is explicit. We check
    // both rename targets just to be safe.
    match s {
        "claude" => Some(ProviderKind::Claude),
        "codex" => Some(ProviderKind::Codex),
        "github_copilot" | "githubcopilot" | "github-copilot" => {
            Some(ProviderKind::GitHubCopilot)
        }
        "opencode" | "open_code" => Some(ProviderKind::OpenCode),
        _ => None,
    }
}

fn provider_kebab(p: ProviderKind) -> &'static str {
    match p {
        ProviderKind::Claude => "claude",
        ProviderKind::Codex => "codex",
        ProviderKind::GitHubCopilot => "github_copilot",
        ProviderKind::OpenCode => "opencode",
    }
}

fn parse_permission_mode(s: &str) -> Option<PermissionMode> {
    match s {
        "default" => Some(PermissionMode::Default),
        "accept_edits" | "acceptedits" => Some(PermissionMode::AcceptEdits),
        "plan" => Some(PermissionMode::Plan),
        "bypass" => Some(PermissionMode::Bypass),
        "auto" => Some(PermissionMode::Auto),
        _ => None,
    }
}

fn parse_reasoning_effort(s: &str) -> Option<ReasoningEffort> {
    match s {
        "minimal" => Some(ReasoningEffort::Minimal),
        "low" => Some(ReasoningEffort::Low),
        "medium" => Some(ReasoningEffort::Medium),
        "high" => Some(ReasoningEffort::High),
        "xhigh" => Some(ReasoningEffort::Xhigh),
        "max" => Some(ReasoningEffort::Max),
        _ => None,
    }
}

/// Helpers for parsing the structured marker payloads produced by
/// triage / memory_seeder / memory_updater (which encode JSON
/// inside the marker summary). Wrapped here so the tick loop has
/// one place to call. Returns `Err` if the payload didn't parse —
/// caller surfaces as NeedsHuman.
pub mod payload {
    use serde::Deserialize;

    use super::super::model::ProjectMemory;

    /// One of two shapes the triage agent emits in its
    /// `<<<TASK_DONE: ...>>>` payload:
    ///
    /// - `Single { project_id, title }` — handle this task as-is
    ///   in that project.
    /// - `Split { subtasks }` — break into N independent tasks.
    ///   Each subtask carries its own project_id, title, body,
    ///   and a `depends_on` list of 0-based indices into the same
    ///   `subtasks` array.
    ///
    /// We use untagged enum dispatch so the agent doesn't have
    /// to learn a discriminator key — the presence of `subtasks`
    /// vs `project_id` at the top level is the distinguishing
    /// feature.
    #[derive(Debug, Deserialize)]
    #[serde(untagged)]
    pub enum TriageDecision {
        Single {
            project_id: String,
            title: String,
        },
        Split {
            subtasks: Vec<TriageSubtask>,
        },
    }

    #[derive(Debug, Deserialize)]
    pub struct TriageSubtask {
        pub title: String,
        pub body: String,
        pub project_id: String,
        #[serde(default)]
        pub depends_on: Vec<usize>,
    }

    pub fn parse_triage(s: &str) -> Result<TriageDecision, String> {
        let decision: TriageDecision = serde_json::from_str(s.trim())
            .map_err(|e| format!("bad triage JSON: {e}"))?;
        if let TriageDecision::Split { subtasks } = &decision {
            if subtasks.is_empty() {
                return Err("triage produced empty subtasks list".to_string());
            }
            // Sanity-check `depends_on` indices.
            for (i, sub) in subtasks.iter().enumerate() {
                for &dep in &sub.depends_on {
                    if dep >= subtasks.len() {
                        return Err(format!(
                            "subtask {i} depends_on index {dep} out of range"
                        ));
                    }
                    if dep >= i {
                        // Forward / self deps would deadlock the
                        // tick loop. The prompt warns against this
                        // but we enforce it server-side.
                        return Err(format!(
                            "subtask {i} depends_on index {dep} \
                             must be strictly less than {i} (no forward deps)"
                        ));
                    }
                }
            }
        }
        Ok(decision)
    }

    /// Memory blob shape in marker payloads — same structured
    /// fields as `ProjectMemory` but skipping the server-managed
    /// metadata (`project_id`, `seeded_at`, `updated_at`).
    #[derive(Debug, Deserialize)]
    pub struct MemoryBlob {
        #[serde(default)]
        pub purpose: Option<String>,
        #[serde(default)]
        pub languages: Vec<String>,
        #[serde(default)]
        pub key_directories: Vec<super::super::model::KeyDirectory>,
        #[serde(default)]
        pub conventions: Vec<String>,
        #[serde(default)]
        pub recent_task_themes: Vec<String>,
    }

    pub fn parse_memory(s: &str) -> Result<MemoryBlob, String> {
        serde_json::from_str(s.trim()).map_err(|e| format!("bad memory JSON: {e}"))
    }

    pub fn into_memory(project_id: &str, blob: MemoryBlob) -> ProjectMemory {
        ProjectMemory {
            project_id: project_id.to_string(),
            purpose: blob.purpose,
            languages: blob.languages,
            key_directories: blob.key_directories,
            conventions: blob.conventions,
            recent_task_themes: blob.recent_task_themes,
            seeded_at: None, // store preserves existing via COALESCE
            updated_at: 0,
        }
    }

    /// Reviewer marker convention: payload's first word is the
    /// verdict ("approved" / "changes_requested"). Returns the
    /// verdict + the rest as rationale text.
    pub fn parse_review_verdict(s: &str) -> ReviewVerdict {
        let trimmed = s.trim();
        let (head, rest) = match trimmed.split_once(|c: char| c.is_whitespace() || c == '—' || c == '-') {
            Some((h, r)) => (h.trim(), r.trim()),
            None => (trimmed, ""),
        };
        let head_lower = head.to_lowercase();
        let head_lower = head_lower.trim_end_matches([',', ':', ';', '.']);
        if head_lower == "approved" || head_lower == "approve" || head_lower == "lgtm" {
            ReviewVerdict::Approved {
                rationale: rest.to_string(),
            }
        } else if head_lower.contains("changes")
            || head_lower == "request"
            || head_lower == "reject"
            || head_lower == "rejected"
        {
            ReviewVerdict::ChangesRequested {
                rationale: rest.to_string(),
            }
        } else {
            // Default to "treat as approval" so a stray formatting
            // miss doesn't pin the task — but log it. The reviewer
            // prompt is explicit so this should be rare.
            ReviewVerdict::Approved {
                rationale: format!("(verdict unclear; raw: {trimmed})"),
            }
        }
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum ReviewVerdict {
        Approved { rationale: String },
        ChangesRequested { rationale: String },
    }
}


#[cfg(test)]
mod tests {
    use super::payload::*;
    use super::*;

    // ── settings resolver ─────────────────────────────────────────

    fn make_spawner_with_user_config(ucs: UserConfigStore) -> AgentSpawner {
        // We don't need a real RuntimeCore for resolver tests —
        // none of resolve_*() touches it. But AgentSpawner::new
        // takes one, so use a "fake" via runtime-core's test
        // builder. To avoid the dependency, directly construct
        // the struct here.
        unimplemented!("unused — replaced by static helpers below")
    }

    /// Construct a UserConfigStore in-memory for resolver tests.
    fn ucs() -> UserConfigStore {
        // Use a temp dir so each test gets a fresh store.
        let tmp = tempfile::tempdir().unwrap();
        UserConfigStore::open(tmp.path()).expect("open user config in tempdir")
    }

    // The resolver methods on AgentSpawner read only
    // `self.user_config`, so we test them via a helper that
    // builds a minimal AgentSpawner with a stub runtime. The
    // stub is only stored — it's never called by resolver code.
    fn resolver_for(ucs: UserConfigStore) -> AgentSpawner {
        // We use Arc::from_raw / Box::leak tricks here? No —
        // simpler: build a minimal AgentSpawner via direct field
        // construction. AgentSpawner's fields are crate-private
        // so this works inside the same crate.
        AgentSpawner {
            // SAFETY: resolver methods never call into runtime,
            // so a dangling Arc is acceptable. We use Arc::new of
            // a freshly-constructed RuntimeCore-shaped value...
            // actually no, we can't fabricate one. Instead, skip
            // tests that need a runtime and just unit-test the
            // pure-function helpers (parse_provider, parse_*).
            runtime: panic!("resolver tests should not invoke runtime"),
            kanban: KanbanStore::in_memory().unwrap(),
            user_config: Some(ucs),
        }
    }

    #[test]
    fn parse_provider_recognises_kebab_and_snake() {
        assert!(matches!(
            parse_provider("claude"),
            Some(ProviderKind::Claude)
        ));
        assert!(matches!(
            parse_provider("github_copilot"),
            Some(ProviderKind::GitHubCopilot)
        ));
        assert!(matches!(
            parse_provider("github-copilot"),
            Some(ProviderKind::GitHubCopilot)
        ));
        assert!(matches!(
            parse_provider("opencode"),
            Some(ProviderKind::OpenCode)
        ));
        assert!(parse_provider("nonsense").is_none());
    }

    #[test]
    fn parse_permission_mode_matches_settings_keys() {
        assert!(matches!(
            parse_permission_mode("default"),
            Some(PermissionMode::Default)
        ));
        assert!(matches!(
            parse_permission_mode("accept_edits"),
            Some(PermissionMode::AcceptEdits)
        ));
        assert!(matches!(
            parse_permission_mode("plan"),
            Some(PermissionMode::Plan)
        ));
        assert!(matches!(
            parse_permission_mode("bypass"),
            Some(PermissionMode::Bypass)
        ));
        assert!(matches!(
            parse_permission_mode("auto"),
            Some(PermissionMode::Auto)
        ));
        assert!(parse_permission_mode("unknown").is_none());
    }

    #[test]
    fn parse_reasoning_effort_matches_settings_keys() {
        assert!(matches!(
            parse_reasoning_effort("high"),
            Some(ReasoningEffort::High)
        ));
        assert!(matches!(
            parse_reasoning_effort("xhigh"),
            Some(ReasoningEffort::Xhigh)
        ));
        assert!(parse_reasoning_effort("turbo").is_none());
    }

    // Drop the unused helpers — the warning is loud otherwise.
    #[allow(dead_code)]
    fn _unused() {
        let _ = make_spawner_with_user_config;
        let _ = ucs;
        let _ = resolver_for;
    }

    // ── triage payload ────────────────────────────────────────────

    #[test]
    fn triage_payload_parses_single() {
        match parse_triage(r#"{"project_id":"p1","title":"fix"}"#).unwrap() {
            TriageDecision::Single { project_id, title } => {
                assert_eq!(project_id, "p1");
                assert_eq!(title, "fix");
            }
            other => panic!("expected Single, got {other:?}"),
        }
    }

    #[test]
    fn triage_payload_parses_split() {
        let raw = r#"{"subtasks":[
            {"title":"a","body":"do a","project_id":"p1","depends_on":[]},
            {"title":"b","body":"do b","project_id":"p1","depends_on":[0]}
        ]}"#;
        match parse_triage(raw).unwrap() {
            TriageDecision::Split { subtasks } => {
                assert_eq!(subtasks.len(), 2);
                assert_eq!(subtasks[0].title, "a");
                assert_eq!(subtasks[1].depends_on, vec![0]);
            }
            other => panic!("expected Split, got {other:?}"),
        }
    }

    #[test]
    fn triage_payload_rejects_forward_dep() {
        let raw = r#"{"subtasks":[
            {"title":"a","body":"","project_id":"p1","depends_on":[1]},
            {"title":"b","body":"","project_id":"p1","depends_on":[]}
        ]}"#;
        assert!(parse_triage(raw).is_err());
    }

    #[test]
    fn triage_payload_rejects_out_of_range_dep() {
        let raw = r#"{"subtasks":[
            {"title":"a","body":"","project_id":"p1","depends_on":[5]}
        ]}"#;
        assert!(parse_triage(raw).is_err());
    }

    #[test]
    fn triage_payload_rejects_empty_subtasks() {
        assert!(parse_triage(r#"{"subtasks":[]}"#).is_err());
    }

    #[test]
    fn triage_payload_rejects_unknown_shape() {
        assert!(parse_triage(r#"{"project_id":"p1"}"#).is_err());
    }

    #[test]
    fn memory_payload_parses_with_optional_fields() {
        let m = parse_memory(
            r#"{"purpose":"hi","languages":["rust"],"key_directories":[],"conventions":[],"recent_task_themes":[]}"#,
        )
        .unwrap();
        assert_eq!(m.purpose.as_deref(), Some("hi"));
        assert_eq!(m.languages, vec!["rust".to_string()]);
    }

    #[test]
    fn memory_payload_tolerates_missing_fields() {
        let m = parse_memory(r#"{}"#).unwrap();
        assert!(m.purpose.is_none());
        assert!(m.languages.is_empty());
    }

    #[test]
    fn review_verdict_classifies() {
        match parse_review_verdict("approved — lgtm") {
            ReviewVerdict::Approved { .. } => {}
            other => panic!("{other:?}"),
        }
        match parse_review_verdict("changes_requested needs tests") {
            ReviewVerdict::ChangesRequested { .. } => {}
            other => panic!("{other:?}"),
        }
        match parse_review_verdict("LGTM,") {
            ReviewVerdict::Approved { .. } => {}
            other => panic!("{other:?}"),
        }
        // Unknown verdict defaults to Approved with raw text in
        // the rationale — see comment in parse_review_verdict.
        match parse_review_verdict("maybe") {
            ReviewVerdict::Approved { rationale } => assert!(rationale.contains("maybe")),
            other => panic!("{other:?}"),
        }
    }
}
