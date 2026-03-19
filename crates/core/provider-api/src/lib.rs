mod binary_resolver;

pub use binary_resolver::find_cli_binary;

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::{Mutex, oneshot};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    Codex,
    Claude,
    #[serde(rename = "github_copilot")]
    GitHubCopilot,
    #[serde(rename = "claude_cli")]
    ClaudeCli,
    #[serde(rename = "github_copilot_cli")]
    GitHubCopilotCli,
}

impl ProviderKind {
    /// Every known provider variant. Keep in sync with the enum definition.
    pub const ALL: &[ProviderKind] = &[
        ProviderKind::Codex,
        ProviderKind::Claude,
        ProviderKind::GitHubCopilot,
        ProviderKind::ClaudeCli,
        ProviderKind::GitHubCopilotCli,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Codex => "Codex",
            Self::Claude => "Claude",
            Self::GitHubCopilot => "GitHub Copilot",
            Self::ClaudeCli => "Claude (CLI)",
            Self::GitHubCopilotCli => "GitHub Copilot (CLI)",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderStatusLevel {
    Ready,
    Warning,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Ready,
    Running,
    Interrupted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnStatus {
    Running,
    Completed,
    Interrupted,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallStatus {
    Pending,
    Completed,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionDecision {
    Allow,
    AllowAlways,
    Deny,
    DenyAlways,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserInputOption {
    pub id: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserInputQuestion {
    pub id: String,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header: Option<String>,
    #[serde(default)]
    pub options: Vec<UserInputOption>,
    #[serde(default)]
    pub multi_select: bool,
    #[serde(default)]
    pub allow_freeform: bool,
    #[serde(default)]
    pub is_secret: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserInputAnswer {
    pub question_id: String,
    #[serde(default)]
    pub option_ids: Vec<String>,
    pub answer: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionMode {
    Default,
    AcceptEdits,
    Plan,
    Bypass,
}

impl Default for PermissionMode {
    fn default() -> Self {
        PermissionMode::AcceptEdits
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningEffort {
    Minimal,
    Low,
    Medium,
    High,
}

impl Default for ReasoningEffort {
    fn default() -> Self {
        ReasoningEffort::Medium
    }
}

impl ReasoningEffort {
    pub fn as_str(self) -> &'static str {
        match self {
            ReasoningEffort::Minimal => "minimal",
            ReasoningEffort::Low => "low",
            ReasoningEffort::Medium => "medium",
            ReasoningEffort::High => "high",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileOperation {
    Write,
    Edit,
    Delete,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubagentStatus {
    Running,
    Completed,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanStatus {
    Proposed,
    Accepted,
    Rejected,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanStep {
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanRecord {
    pub plan_id: String,
    pub title: String,
    pub steps: Vec<PlanStep>,
    pub raw: String,
    pub status: PlanStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileChangeRecord {
    pub call_id: String,
    pub path: String,
    pub operation: FileOperation,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubagentRecord {
    pub agent_id: String,
    pub parent_call_id: String,
    pub agent_type: String,
    pub prompt: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub events: Vec<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub status: SubagentStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCall {
    pub call_id: String,
    pub name: String,
    pub args: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub status: ToolCallStatus,
    /// When this tool call was issued from inside a sub-agent (the SDK's
    /// Task/Agent dispatch), this is the `call_id` of the parent Task
    /// tool_use that spawned the sub-agent. `None` means the call was
    /// issued directly by the main agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_call_id: Option<String>,
}

/// One element of an assistant turn's ordered content stream.
///
/// Models a turn the way Anthropic does — as a sequence of blocks in
/// the order they arrived from the provider — so that interleaved
/// "text, tool, text, tool" responses render in stream order rather
/// than getting flattened into "all text first, then all tools at the
/// end". The legacy `output`, `reasoning`, and `tool_calls` fields on
/// `TurnRecord` remain populated for backwards compatibility.
///
/// `Text` and `Reasoning` carry their own text segment so a single
/// turn can hold multiple separate runs interrupted by tool calls.
/// `ToolCall` references the matching entry in `TurnRecord::tool_calls`
/// by `call_id` — that's where mutable status/output live; the block
/// itself only carries position.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    Reasoning {
        text: String,
    },
    ToolCall {
        #[serde(rename = "callId")]
        call_id: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderModel {
    pub value: String,
    pub label: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderStatus {
    pub kind: ProviderKind,
    pub label: String,
    pub installed: bool,
    pub authenticated: bool,
    pub version: Option<String>,
    pub status: ProviderStatusLevel,
    pub message: Option<String>,
    #[serde(default)]
    pub models: Vec<ProviderModel>,
    /// Runtime toggle — when `false`, the daemon refuses new turns
    /// for this provider and the frontend greys it out in Settings and
    /// hides it from the new-session picker. Adapters themselves
    /// always emit `true`; runtime-core overwrites with the persisted
    /// value from the `provider_enablement` table before broadcasting.
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

/// Multimodal user input for a single turn. Adapters that only
/// support text use `input.text` and silently drop `images` after
/// logging a one-line `warn!`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UserInput {
    pub text: String,
    #[serde(default)]
    pub images: Vec<ImageAttachment>,
}

impl UserInput {
    pub fn from_text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            images: Vec::new(),
        }
    }
}

/// Raw image payload carried across the trait boundary while a turn
/// is in flight. The Claude SDK bridge needs the bytes to build
/// multimodal content blocks; the runtime also persists them to disk
/// before calling the adapter so they survive across restarts.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ImageAttachment {
    /// MIME type, e.g. `"image/png"`.
    pub media_type: String,
    /// Standard base64 (no `data:` prefix).
    pub data_base64: String,
    /// Display name, e.g. `"image.png"`. Not forwarded to the model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// Lightweight reference to a persisted image. Sent to the frontend
/// on session load in place of the raw bytes, so opening a thread
/// with lots of attachments stays cheap.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AttachmentRef {
    /// UUID — also the filename (sans extension) on disk.
    pub id: String,
    pub media_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub size_bytes: u64,
}

/// On-demand attachment payload, returned by the `get_attachment`
/// client message. Carries the full bytes; fetched lazily when the
/// user clicks a persisted chip.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AttachmentData {
    pub media_type: String,
    pub data_base64: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnRecord {
    pub turn_id: String,
    pub input: String,
    pub output: String,
    pub status: TurnStatus,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub file_changes: Vec<FileChangeRecord>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub subagents: Vec<SubagentRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<PlanRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<PermissionMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<ReasoningEffort>,
    /// Ordered content stream — text, reasoning, and tool calls in the
    /// exact order the provider emitted them. Canonical view for UIs
    /// that want to render interleaved content correctly.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocks: Vec<ContentBlock>,
    /// References to images the user pasted on this turn. Lightweight
    /// metadata only — the full bytes live on disk and are fetched
    /// lazily via `get_attachment` when the user clicks a chip.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub input_attachments: Vec<AttachmentRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectRecord {
    pub project_id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub sort_order: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSummary {
    pub session_id: String,
    pub provider: ProviderKind,
    pub title: String,
    pub status: SessionStatus,
    pub created_at: String,
    pub updated_at: String,
    pub last_turn_preview: Option<String>,
    pub turn_count: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderSessionState {
    pub native_thread_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionDetail {
    pub summary: SessionSummary,
    pub turns: Vec<TurnRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_state: Option<ProviderSessionState>,
    /// Transient working directory resolved by RuntimeCore before adapter calls.
    /// Not persisted in the database.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

impl SessionDetail {
    pub fn format_turn_context(&self, latest_input: &str) -> String {
        let mut prompt = String::from(
            "You are operating inside ZenUI, a native coding-agent shell. Respond directly to the user's latest request using the conversation history below when useful.\n\n",
        );

        if self.turns.is_empty() {
            prompt.push_str("No prior turns exist for this session yet.\n\n");
        } else {
            prompt.push_str("Conversation history:\n");
            for (index, turn) in self.turns.iter().enumerate() {
                prompt.push_str(&format!("Turn {} user: {}\n", index + 1, turn.input));
                if !turn.output.trim().is_empty() {
                    prompt.push_str(&format!("Turn {} assistant: {}\n", index + 1, turn.output));
                }
                prompt.push('\n');
            }
        }

        prompt.push_str("Latest user request:\n");
        prompt.push_str(latest_input.trim());
        prompt.push_str("\n");

        prompt
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppSnapshot {
    pub generated_at: String,
    pub sessions: Vec<SessionDetail>,
    #[serde(default)]
    pub projects: Vec<ProjectRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BootstrapPayload {
    pub app_name: String,
    pub generated_at: String,
    pub ws_url: String,
    pub providers: Vec<ProviderStatus>,
    pub snapshot: AppSnapshot,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HealthPayload {
    pub status: String,
    pub generated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderTurnOutput {
    pub output: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_state: Option<ProviderSessionState>,
}

/// Events that a provider adapter can push during a turn for streaming display.
#[derive(Debug, Clone)]
pub enum ProviderTurnEvent {
    AssistantTextDelta {
        delta: String,
    },
    ReasoningDelta {
        delta: String,
    },
    ToolCallStarted {
        call_id: String,
        name: String,
        args: Value,
        /// Parent Task/Agent call_id when this tool call originates
        /// from a sub-agent; `None` for main-agent calls.
        parent_call_id: Option<String>,
    },
    ToolCallCompleted {
        call_id: String,
        output: String,
        error: Option<String>,
    },
    Info {
        message: String,
    },
    PermissionRequest {
        request_id: String,
        tool_name: String,
        input: Value,
        suggested_decision: PermissionDecision,
    },
    UserQuestion {
        request_id: String,
        questions: Vec<UserInputQuestion>,
    },
    FileChange {
        call_id: String,
        path: String,
        operation: FileOperation,
        before: Option<String>,
        after: Option<String>,
    },
    SubagentStarted {
        parent_call_id: String,
        agent_id: String,
        agent_type: String,
        prompt: String,
    },
    SubagentEvent {
        agent_id: String,
        event: Value,
    },
    SubagentCompleted {
        agent_id: String,
        output: String,
        error: Option<String>,
    },
    PlanProposed {
        plan_id: String,
        title: String,
        steps: Vec<PlanStep>,
        raw: String,
    },
}

/// Per-session "always allow / always deny" memory keyed by tool name.
/// Lives in `RuntimeCore` and is shared across every `TurnEventSink` for
/// a given session, so a user's "Always" decision in turn N short-circuits
/// every subsequent prompt for the same tool in turn N+1, N+2, ...
///
/// Stored decisions are normalized to `Allow` or `Deny` (never `AllowAlways`
/// or `DenyAlways`) — providers don't care about the "always" suffix, only
/// about the binary outcome.
pub type PermissionPolicy = Arc<Mutex<HashMap<String, PermissionDecision>>>;

/// A handle for adapters to push streaming events during `execute_turn`.
/// Dropping the sink closes the channel, signalling to the runtime that the turn is complete.
#[derive(Clone)]
pub struct TurnEventSink {
    tx: tokio::sync::mpsc::Sender<ProviderTurnEvent>,
    /// Oneshot per outstanding permission request, keyed by the
    /// internal `perm-N` id. The payload is `(decision, mode_override)`
    /// so "approve and switch mode" is delivered atomically with the
    /// decision itself — no side channel, no id-lookup mistakes.
    permission_pending: Arc<
        Mutex<HashMap<String, oneshot::Sender<(PermissionDecision, Option<PermissionMode>)>>>,
    >,
    question_pending: Arc<Mutex<HashMap<String, oneshot::Sender<Vec<UserInputAnswer>>>>>,
    /// Session-scoped persistent permission decisions. Shared across turns.
    policy: PermissionPolicy,
}

impl TurnEventSink {
    /// Create a sink with a fresh, isolated permission policy. Mostly for
    /// tests and one-off uses where there's no shared session state.
    pub fn new(tx: tokio::sync::mpsc::Sender<ProviderTurnEvent>) -> Self {
        Self::with_policy(tx, Arc::new(Mutex::new(HashMap::new())))
    }

    /// Create a sink wired to an existing per-session permission policy. The
    /// runtime calls this once per turn, passing in the policy that lives on
    /// `RuntimeCore` keyed by `session_id`, so always-allow/always-deny
    /// decisions persist for the rest of the session.
    pub fn with_policy(
        tx: tokio::sync::mpsc::Sender<ProviderTurnEvent>,
        policy: PermissionPolicy,
    ) -> Self {
        Self {
            tx,
            permission_pending: Arc::new(Mutex::new(HashMap::new())),
            question_pending: Arc::new(Mutex::new(HashMap::new())),
            policy,
        }
    }

    /// Send a streaming event. Silently drops if the channel is closed.
    pub async fn send(&self, event: ProviderTurnEvent) {
        let _ = self.tx.send(event).await;
    }

    /// Ask the host to decide on a tool invocation. Emits a PermissionRequest event
    /// and awaits the host's answer. Returns `(Deny, None)` if the channel closes.
    ///
    /// The second tuple element is an optional `PermissionMode` the host
    /// wants applied alongside the approval — used by the plan-exit
    /// approve-and-switch flow. Adapters that don't care about mode
    /// switches can simply ignore it; there is no separate side channel
    /// to forget to read.
    ///
    /// Short-circuits the host prompt entirely if a previous turn in this
    /// session already answered `AllowAlways` or `DenyAlways` for the same
    /// tool name. The cached fast path always returns `None` for the mode
    /// override — "always" decisions are tool-scoped, not mode-scoped.
    pub async fn request_permission(
        &self,
        tool_name: String,
        input: Value,
        suggested: PermissionDecision,
    ) -> (PermissionDecision, Option<PermissionMode>) {
        // Fast path: prior "always" decision in this session.
        {
            let policy = self.policy.lock().await;
            if let Some(decision) = policy.get(&tool_name).copied() {
                return (decision, None);
            }
        }

        let request_id = next_request_id("perm");
        let (sender, receiver) = oneshot::channel();
        {
            let mut guard = self.permission_pending.lock().await;
            guard.insert(request_id.clone(), sender);
            tracing::info!(
                request_id = %request_id,
                tool_name = %tool_name,
                pending_count = guard.len(),
                "permission request registered in pending map"
            );
        }
        self.send(ProviderTurnEvent::PermissionRequest {
            request_id: request_id.clone(),
            tool_name: tool_name.clone(),
            input,
            suggested_decision: suggested,
        })
        .await;
        let (decision, mode_override) = match receiver.await {
            Ok(payload) => payload,
            Err(_) => {
                tracing::warn!(
                    request_id = %request_id,
                    "permission oneshot receiver got Err — sender dropped before answer (turn probably ended mid-prompt)"
                );
                let mut guard = self.permission_pending.lock().await;
                guard.remove(&request_id);
                return (PermissionDecision::Deny, None);
            }
        };

        // Persist "always" decisions before normalizing the return value.
        // The provider only sees Allow/Deny — the "always" suffix is purely
        // a host-side hint that we should remember the answer.
        let normalized = match decision {
            PermissionDecision::AllowAlways => {
                self.policy
                    .lock()
                    .await
                    .insert(tool_name, PermissionDecision::Allow);
                PermissionDecision::Allow
            }
            PermissionDecision::DenyAlways => {
                self.policy
                    .lock()
                    .await
                    .insert(tool_name, PermissionDecision::Deny);
                PermissionDecision::Deny
            }
            other => other,
        };
        (normalized, mode_override)
    }

    /// Ask the user one or more structured clarifying questions and await their
    /// answers. Returns None if the channel closes before the user answers.
    pub async fn ask_user(
        &self,
        questions: Vec<UserInputQuestion>,
    ) -> Option<Vec<UserInputAnswer>> {
        let request_id = next_request_id("q");
        let (sender, receiver) = oneshot::channel();
        {
            let mut guard = self.question_pending.lock().await;
            guard.insert(request_id.clone(), sender);
        }
        self.send(ProviderTurnEvent::UserQuestion {
            request_id: request_id.clone(),
            questions,
        })
        .await;
        match receiver.await {
            Ok(answers) => Some(answers),
            Err(_) => {
                let mut guard = self.question_pending.lock().await;
                guard.remove(&request_id);
                None
            }
        }
    }

    /// Host-side: called by the runtime when the user answers a permission request.
    pub async fn resolve_permission(&self, request_id: &str, decision: PermissionDecision) {
        self.resolve_permission_with_mode(request_id, decision, None)
            .await;
    }

    /// Like `resolve_permission`, but also attaches an optional
    /// permission-mode override that travels atomically with the
    /// decision through the oneshot. Used by the plan-exit
    /// approve-and-switch flow so the Claude SDK adapter can include
    /// `updatedPermissions` in the bridge's `PermissionResult` within
    /// the same wake-up that delivers the decision. Logs a warning if
    /// there is no pending sender for the id — that almost always
    /// means a stale or mis-routed answer.
    pub async fn resolve_permission_with_mode(
        &self,
        request_id: &str,
        decision: PermissionDecision,
        mode_override: Option<PermissionMode>,
    ) {
        let mut guard = self.permission_pending.lock().await;
        match guard.remove(request_id) {
            Some(sender) => {
                tracing::info!(
                    request_id,
                    ?decision,
                    has_mode_override = mode_override.is_some(),
                    "resolve_permission firing oneshot"
                );
                let _ = sender.send((decision, mode_override));
            }
            None => {
                tracing::warn!(
                    request_id,
                    "resolve_permission found no pending sender — stale or mis-routed answer"
                );
            }
        }
    }

    /// Unblock every permission and question oneshot that is still
    /// sitting in this sink's pending maps. Dropping the sender side
    /// causes the receiver inside `request_permission` / `ask_user`
    /// to wake with Err and return a synthetic Deny / None, which
    /// unwinds any spawned task still waiting on a user answer.
    ///
    /// The adapter must call this at the very end of `run_turn` so
    /// orphaned permission tasks (e.g. from an interrupt that tore
    /// down the bridge before the user answered every prompt) don't
    /// live forever holding the sink's `permission_pending` map and
    /// leaking memory.
    pub async fn drain_pending(&self) {
        let drained_perms = {
            let mut guard = self.permission_pending.lock().await;
            let n = guard.len();
            guard.clear();
            n
        };
        let drained_qs = {
            let mut guard = self.question_pending.lock().await;
            let n = guard.len();
            guard.clear();
            n
        };
        if drained_perms > 0 || drained_qs > 0 {
            tracing::info!(
                drained_permissions = drained_perms,
                drained_questions = drained_qs,
                "sink drain_pending: released orphaned oneshots"
            );
        }
    }

    /// Host-side: called by the runtime when the user answers a question.
    pub async fn resolve_question(&self, request_id: &str, answers: Vec<UserInputAnswer>) {
        let mut guard = self.question_pending.lock().await;
        if let Some(sender) = guard.remove(request_id) {
            let _ = sender.send(answers);
        }
    }

    /// Host-side: called by the runtime when the user dismisses a question
    /// without answering. Dropping the sender causes the awaiting `ask_user`
    /// to return `None`, which each adapter translates into a provider-specific
    /// cancellation signal (JSON-RPC error, deny permission result, sentinel
    /// answer) so the model can proceed instead of hanging forever.
    pub async fn cancel_question(&self, request_id: &str) {
        let mut guard = self.question_pending.lock().await;
        guard.remove(request_id);
    }
}

fn next_request_id(prefix: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or_default();
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}-{nanos:x}-{n:x}")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RuntimeEvent {
    RuntimeReady {
        message: String,
    },
    /// The daemon has entered graceful shutdown. Clients should show a
    /// banner, finish any in-progress UI interactions, and stop issuing
    /// new turn-starting commands.
    DaemonShuttingDown {
        reason: String,
    },
    SessionStarted {
        session: SessionSummary,
    },
    TurnStarted {
        session_id: String,
        turn: TurnRecord,
    },
    ContentDelta {
        session_id: String,
        turn_id: String,
        delta: String,
        accumulated_output: String,
    },
    ReasoningDelta {
        session_id: String,
        turn_id: String,
        delta: String,
    },
    ToolCallStarted {
        session_id: String,
        turn_id: String,
        call_id: String,
        name: String,
        args: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_call_id: Option<String>,
    },
    ToolCallCompleted {
        session_id: String,
        turn_id: String,
        call_id: String,
        output: String,
        error: Option<String>,
    },
    TurnCompleted {
        session_id: String,
        session: SessionSummary,
        turn: TurnRecord,
    },
    SessionInterrupted {
        session: SessionSummary,
        message: String,
    },
    SessionDeleted {
        session_id: String,
    },
    PermissionRequested {
        session_id: String,
        turn_id: String,
        request_id: String,
        tool_name: String,
        input: Value,
        suggested: PermissionDecision,
    },
    UserQuestionAsked {
        session_id: String,
        turn_id: String,
        request_id: String,
        questions: Vec<UserInputQuestion>,
    },
    FileChanged {
        session_id: String,
        turn_id: String,
        call_id: String,
        path: String,
        operation: FileOperation,
        before: Option<String>,
        after: Option<String>,
    },
    SubagentStarted {
        session_id: String,
        turn_id: String,
        parent_call_id: String,
        agent_id: String,
        agent_type: String,
        prompt: String,
    },
    SubagentEvent {
        session_id: String,
        turn_id: String,
        agent_id: String,
        event: Value,
    },
    SubagentCompleted {
        session_id: String,
        turn_id: String,
        agent_id: String,
        output: String,
        error: Option<String>,
    },
    PlanProposed {
        session_id: String,
        turn_id: String,
        plan_id: String,
        title: String,
        steps: Vec<PlanStep>,
        raw: String,
    },
    Error {
        message: String,
    },
    Info {
        message: String,
    },
    ProviderModelsUpdated {
        provider: ProviderKind,
        models: Vec<ProviderModel>,
    },
    ProviderHealthUpdated {
        status: ProviderStatus,
    },
    ProjectCreated {
        project: ProjectRecord,
    },
    ProjectRenamed {
        project_id: String,
        name: String,
        updated_at: String,
    },
    ProjectDeleted {
        project_id: String,
        reassigned_session_ids: Vec<String>,
    },
    SessionProjectAssigned {
        session_id: String,
        project_id: Option<String>,
    },
    SessionRenamed {
        session_id: String,
        title: String,
    },
    SessionModelUpdated {
        session_id: String,
        model: String,
    },
    SessionArchived {
        session_id: String,
    },
    SessionUnarchived {
        session: SessionSummary,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    Ping,
    LoadSnapshot,
    LoadSession {
        session_id: String,
        /// Cap the number of turns returned to the most recent `limit`.
        /// Absent (the default) means "return every turn" — callers that
        /// don't care about long-thread payload size can keep using the
        /// original shape. Transports and UIs that want perceived-fast
        /// thread opens should pass a small positive value (e.g. 50)
        /// and lazy-load older turns on demand.
        #[serde(default)]
        limit: Option<usize>,
    },
    StartSession {
        provider: ProviderKind,
        title: Option<String>,
        #[serde(default)]
        model: Option<String>,
        #[serde(default)]
        project_id: Option<String>,
    },
    SendTurn {
        session_id: String,
        input: String,
        /// Pasted images attached to this turn. Each carries the raw
        /// base64 bytes so the runtime can persist them to disk and
        /// forward them to providers that support multimodal input.
        #[serde(default)]
        images: Vec<ImageAttachment>,
        #[serde(default)]
        permission_mode: Option<PermissionMode>,
        #[serde(default)]
        reasoning_effort: Option<ReasoningEffort>,
    },
    /// Fetch the full bytes of a persisted image attachment. The
    /// frontend calls this lazily when the user clicks a chip on a
    /// replayed turn; runtime-core reads the file from
    /// `<data_dir>/attachments/<uuid>.<ext>` and responds with
    /// `ServerMessage::Attachment`.
    GetAttachment {
        attachment_id: String,
    },
    InterruptTurn {
        session_id: String,
    },
    /// Switch the active session's permission mode mid-turn. The runtime
    /// forwards this to the session's adapter; for Claude Agent SDK
    /// sessions the bridge calls `query.setPermissionMode` on the live
    /// SDK Query, so the rest of the in-flight turn runs under the new
    /// mode. Adapters whose backend doesn't support mid-turn switching
    /// silently no-op and the new mode applies to the next turn.
    UpdatePermissionMode {
        session_id: String,
        permission_mode: PermissionMode,
    },
    DeleteSession {
        session_id: String,
    },
    AnswerPermission {
        session_id: String,
        request_id: String,
        decision: PermissionDecision,
        /// Optional permission-mode change to apply alongside the
        /// approval. The Claude SDK adapter forwards this to the
        /// bridge, which sets it on the `PermissionResult`'s
        /// `updatedPermissions` so the SDK applies the mode change
        /// AS PART OF accepting the tool call. This is the canonical
        /// way to swap modes when approving an `ExitPlanMode` —
        /// calling `setPermissionMode` separately doesn't make the
        /// model continue executing within the same turn.
        #[serde(default)]
        permission_mode_override: Option<PermissionMode>,
    },
    AnswerQuestion {
        session_id: String,
        request_id: String,
        answers: Vec<UserInputAnswer>,
    },
    CancelQuestion {
        session_id: String,
        request_id: String,
    },
    AcceptPlan {
        session_id: String,
        plan_id: String,
    },
    RejectPlan {
        session_id: String,
        plan_id: String,
    },
    RefreshModels {
        provider: ProviderKind,
    },
    /// Flip a provider's runtime enabled flag. Persisted to the
    /// `provider_enablement` table and broadcast via
    /// `ProviderHealthUpdated` so every connected client sees the
    /// new state. Disabled providers skip health checks and reject
    /// `SendTurn` — see `runtime-core::handle_client_message`.
    SetProviderEnabled {
        provider: ProviderKind,
        enabled: bool,
    },
    CreateProject {
        name: String,
        #[serde(default)]
        path: Option<String>,
    },
    RenameProject {
        project_id: String,
        name: String,
    },
    DeleteProject {
        project_id: String,
    },
    AssignSessionToProject {
        session_id: String,
        #[serde(default)]
        project_id: Option<String>,
    },
    RenameSession {
        session_id: String,
        title: String,
    },
    UpdateSessionModel {
        session_id: String,
        model: String,
    },
    ArchiveSession {
        session_id: String,
    },
    UnarchiveSession {
        session_id: String,
    },
    ListArchivedSessions,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    Welcome { bootstrap: BootstrapPayload },
    Snapshot { snapshot: AppSnapshot },
    SessionLoaded { session: SessionDetail },
    SessionCreated { session: SessionSummary },
    Pong,
    Ack { message: String },
    Event { event: RuntimeEvent },
    Error { message: String },
    ArchivedSessionsList { sessions: Vec<SessionSummary> },
    /// Response to `ClientMessage::GetAttachment`. Carries the full
    /// bytes of a persisted image.
    Attachment { data: AttachmentData },
}

#[async_trait]
pub trait ProviderAdapter: Send + Sync {
    fn kind(&self) -> ProviderKind;

    async fn health(&self) -> ProviderStatus;

    /// Fetch the live model catalog from the upstream CLI / SDK.
    /// Adapters override this; the default returns an empty list (which the
    /// runtime treats as "use the cached or hardcoded fallback").
    async fn fetch_models(&self) -> Result<Vec<ProviderModel>, String> {
        Ok(Vec::new())
    }

    async fn start_session(
        &self,
        _session: &SessionDetail,
    ) -> Result<Option<ProviderSessionState>, String> {
        Ok(None)
    }

    /// Execute a turn. Push streaming events on `events`; return the canonical final output.
    /// The runtime reconciles: if the returned `output` is non-empty it wins; otherwise the
    /// accumulated text from `AssistantTextDelta` events is used.
    ///
    /// # Compaction and conversation history
    ///
    /// Adapters receive `session: &SessionDetail` for convenience — it carries the turn
    /// history, provider state, and session summary. **Adapters must not build or replay
    /// that turn list to their underlying provider.** Each provider manages its own
    /// conversation state natively:
    ///
    /// - **Codex** uses server-side thread state keyed by `provider_state.native_thread_id`,
    ///   reached via `thread/start` / `thread/resume`.
    /// - **Claude Agent SDK** uses the SDK's `resume:` option; zenui persists the SDK
    ///   session id through `provider_state.native_thread_id` so restarts resume cleanly.
    /// - **GitHub Copilot SDK** keeps an in-memory `session` object alive for the bridge's
    ///   lifetime and manages history internally.
    ///
    /// In all three cases, zenui sends **only the new `input` string** plus a resume
    /// reference each turn, and delegates compaction / context-window management to the
    /// provider. `SessionDetail.turns` is consumed by the frontend for chat-history
    /// display but is never replayed to any model.
    ///
    /// The legacy `format_turn_context` helper below is unused dead code and will be
    /// removed in a follow-up.
    async fn execute_turn(
        &self,
        session: &SessionDetail,
        input: &UserInput,
        permission_mode: PermissionMode,
        reasoning_effort: Option<ReasoningEffort>,
        events: TurnEventSink,
    ) -> Result<ProviderTurnOutput, String>;

    async fn interrupt_turn(&self, _session: &SessionDetail) -> Result<String, String> {
        Ok("Interrupt recorded.".to_string())
    }

    /// Mid-turn permission-mode switch. Adapters that wrap a backend
    /// supporting live mode changes (currently only the Claude Agent SDK
    /// via `query.setPermissionMode`) should forward the request; the
    /// default is a no-op so adapters whose backend takes the mode at
    /// turn-start time silently ignore the request, and the runtime
    /// applies it from the next `execute_turn` instead.
    async fn update_permission_mode(
        &self,
        _session: &SessionDetail,
        _mode: PermissionMode,
    ) -> Result<(), String> {
        Ok(())
    }

    /// Tear down any long-lived resources held for this session (subprocesses, connections).
    async fn end_session(&self, _session: &SessionDetail) -> Result<(), String> {
        Ok(())
    }
}
