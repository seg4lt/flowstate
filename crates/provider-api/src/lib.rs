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
}

impl ProviderKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Codex => "Codex",
            Self::Claude => "Claude",
            Self::GitHubCopilot => "GitHub Copilot",
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectRecord {
    pub project_id: String,
    pub name: String,
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
        question: String,
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

/// A handle for adapters to push streaming events during `execute_turn`.
/// Dropping the sink closes the channel, signalling to the runtime that the turn is complete.
#[derive(Clone)]
pub struct TurnEventSink {
    tx: tokio::sync::mpsc::Sender<ProviderTurnEvent>,
    permission_pending: Arc<Mutex<HashMap<String, oneshot::Sender<PermissionDecision>>>>,
    question_pending: Arc<Mutex<HashMap<String, oneshot::Sender<String>>>>,
}

impl TurnEventSink {
    pub fn new(tx: tokio::sync::mpsc::Sender<ProviderTurnEvent>) -> Self {
        Self {
            tx,
            permission_pending: Arc::new(Mutex::new(HashMap::new())),
            question_pending: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Send a streaming event. Silently drops if the channel is closed.
    pub async fn send(&self, event: ProviderTurnEvent) {
        let _ = self.tx.send(event).await;
    }

    /// Ask the host to decide on a tool invocation. Emits a PermissionRequest event
    /// and awaits the host's answer. Returns Deny if the channel closes.
    pub async fn request_permission(
        &self,
        tool_name: String,
        input: Value,
        suggested: PermissionDecision,
    ) -> PermissionDecision {
        let request_id = next_request_id("perm");
        let (sender, receiver) = oneshot::channel();
        {
            let mut guard = self.permission_pending.lock().await;
            guard.insert(request_id.clone(), sender);
        }
        self.send(ProviderTurnEvent::PermissionRequest {
            request_id: request_id.clone(),
            tool_name,
            input,
            suggested_decision: suggested,
        })
        .await;
        match receiver.await {
            Ok(decision) => decision,
            Err(_) => {
                let mut guard = self.permission_pending.lock().await;
                guard.remove(&request_id);
                PermissionDecision::Deny
            }
        }
    }

    /// Ask the user a free-text question and await their typed answer.
    /// Returns None if the channel closes before the user answers.
    pub async fn ask_user(&self, question: String) -> Option<String> {
        let request_id = next_request_id("q");
        let (sender, receiver) = oneshot::channel();
        {
            let mut guard = self.question_pending.lock().await;
            guard.insert(request_id.clone(), sender);
        }
        self.send(ProviderTurnEvent::UserQuestion {
            request_id: request_id.clone(),
            question,
        })
        .await;
        match receiver.await {
            Ok(answer) => Some(answer),
            Err(_) => {
                let mut guard = self.question_pending.lock().await;
                guard.remove(&request_id);
                None
            }
        }
    }

    /// Host-side: called by the runtime when the user answers a permission request.
    pub async fn resolve_permission(&self, request_id: &str, decision: PermissionDecision) {
        let mut guard = self.permission_pending.lock().await;
        if let Some(sender) = guard.remove(request_id) {
            let _ = sender.send(decision);
        }
    }

    /// Host-side: called by the runtime when the user answers a question.
    pub async fn resolve_question(&self, request_id: &str, answer: String) {
        let mut guard = self.question_pending.lock().await;
        if let Some(sender) = guard.remove(request_id) {
            let _ = sender.send(answer);
        }
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
        question: String,
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
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    Ping,
    LoadSnapshot,
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
        #[serde(default)]
        permission_mode: Option<PermissionMode>,
        #[serde(default)]
        reasoning_effort: Option<ReasoningEffort>,
    },
    InterruptTurn {
        session_id: String,
    },
    DeleteSession {
        session_id: String,
    },
    AnswerPermission {
        session_id: String,
        request_id: String,
        decision: PermissionDecision,
    },
    AnswerQuestion {
        session_id: String,
        request_id: String,
        answer: String,
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
    CreateProject {
        name: String,
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
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    Welcome { bootstrap: BootstrapPayload },
    Snapshot { snapshot: AppSnapshot },
    SessionCreated { session: SessionSummary },
    Pong,
    Ack { message: String },
    Event { event: RuntimeEvent },
    Error { message: String },
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
    async fn execute_turn(
        &self,
        session: &SessionDetail,
        input: &str,
        permission_mode: PermissionMode,
        reasoning_effort: Option<ReasoningEffort>,
        events: TurnEventSink,
    ) -> Result<ProviderTurnOutput, String>;

    async fn interrupt_turn(&self, _session: &SessionDetail) -> Result<String, String> {
        Ok("Interrupt recorded.".to_string())
    }

    /// Tear down any long-lived resources held for this session (subprocesses, connections).
    async fn end_session(&self, _session: &SessionDetail) -> Result<(), String> {
        Ok(())
    }
}
