use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

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
    TurnCompleted {
        session_id: String,
        session: SessionSummary,
        turn: TurnRecord,
    },
    SessionInterrupted {
        session: SessionSummary,
        message: String,
    },
    Error {
        message: String,
    },
    Info {
        message: String,
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
    },
    SendTurn {
        session_id: String,
        input: String,
    },
    InterruptTurn {
        session_id: String,
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

    async fn start_session(
        &self,
        _session: &SessionDetail,
    ) -> Result<Option<ProviderSessionState>, String> {
        Ok(None)
    }

    async fn execute_turn(
        &self,
        session: &SessionDetail,
        input: &str,
    ) -> Result<ProviderTurnOutput, String>;

    async fn interrupt_turn(&self, _session: &SessionDetail) -> Result<String, String> {
        Ok("Interrupt recorded.".to_string())
    }
}
