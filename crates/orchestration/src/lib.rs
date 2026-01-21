use chrono::Utc;
use uuid::Uuid;
use zenui_provider_api::{
    ProviderKind, SessionDetail, SessionStatus, SessionSummary, TurnRecord, TurnStatus,
};

#[derive(Debug, Default)]
pub struct OrchestrationService;

impl OrchestrationService {
    pub fn new() -> Self {
        Self
    }

    pub fn create_session(
        &self,
        provider: ProviderKind,
        title: Option<String>,
        model: Option<String>,
    ) -> SessionDetail {
        let created_at = Utc::now().to_rfc3339();
        let session_id = Uuid::new_v4().to_string();
        let title = title
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| format!("{} Session", provider.label()));

        SessionDetail {
            summary: SessionSummary {
                session_id,
                provider,
                title,
                status: SessionStatus::Ready,
                created_at: created_at.clone(),
                updated_at: created_at,
                last_turn_preview: None,
                turn_count: 0,
                model,
            },
            turns: Vec::new(),
            provider_state: None,
        }
    }

    pub fn start_turn(
        &self,
        session: &mut SessionDetail,
        input: String,
        permission_mode: Option<zenui_provider_api::PermissionMode>,
    ) -> TurnRecord {
        let now = Utc::now().to_rfc3339();
        let turn = TurnRecord {
            turn_id: Uuid::new_v4().to_string(),
            input,
            output: String::new(),
            status: TurnStatus::Running,
            created_at: now.clone(),
            updated_at: now.clone(),
            reasoning: None,
            tool_calls: Vec::new(),
            file_changes: Vec::new(),
            subagents: Vec::new(),
            plan: None,
            permission_mode,
        };

        session.summary.status = SessionStatus::Running;
        session.summary.updated_at = now;
        session.summary.turn_count += 1;
        session.turns.push(turn.clone());
        turn
    }

    pub fn finish_turn(
        &self,
        session: &mut SessionDetail,
        turn_id: &str,
        output: String,
        status: TurnStatus,
    ) -> Option<TurnRecord> {
        let now = Utc::now().to_rfc3339();
        let turn = session
            .turns
            .iter_mut()
            .find(|turn| turn.turn_id == turn_id)?;

        turn.output = output.clone();
        turn.status = status;
        turn.updated_at = now.clone();

        session.summary.status = match status {
            TurnStatus::Interrupted => SessionStatus::Interrupted,
            _ => SessionStatus::Ready,
        };
        session.summary.updated_at = now;
        session.summary.last_turn_preview = Some(output.chars().take(140).collect());

        Some(turn.clone())
    }

    pub fn interrupt_session(&self, session: &mut SessionDetail, message: &str) {
        let now = Utc::now().to_rfc3339();
        session.summary.status = SessionStatus::Interrupted;
        session.summary.updated_at = now.clone();
        session.summary.last_turn_preview = Some(message.chars().take(140).collect());

        if let Some(turn) = session
            .turns
            .iter_mut()
            .rev()
            .find(|turn| turn.status == TurnStatus::Running)
        {
            turn.status = TurnStatus::Interrupted;
            turn.output = message.to_string();
            turn.updated_at = now;
        }
    }
}
