//! Session / turn lifecycle primitives. Pure functions on `SessionDetail`
//! that create a session row, start a turn, finalise a turn, or mark a
//! session interrupted. Previously lived in the `zenui-orchestration`
//! crate, which turned out to host exactly these four helpers and
//! nothing else — a crate boundary paying for no isolation. Folded in
//! here so runtime-core reaches its own session primitives directly.

use chrono::Utc;
use uuid::Uuid;
use zenui_provider_api::{
    ProviderKind, ReasoningEffort, SessionDetail, SessionStatus, SessionSummary, TurnRecord,
    TurnStatus,
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
        model: Option<String>,
        project_id: Option<String>,
    ) -> SessionDetail {
        let created_at = Utc::now().to_rfc3339();
        let session_id = Uuid::new_v4().to_string();

        SessionDetail {
            summary: SessionSummary {
                session_id,
                provider,
                status: SessionStatus::Ready,
                created_at: created_at.clone(),
                updated_at: created_at,
                turn_count: 0,
                model,
                project_id,
            },
            turns: Vec::new(),
            provider_state: None,
            cwd: None,
        }
    }

    pub fn start_turn(
        &self,
        session: &mut SessionDetail,
        input: String,
        permission_mode: Option<zenui_provider_api::PermissionMode>,
        reasoning_effort: Option<ReasoningEffort>,
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
            reasoning_effort,
            blocks: Vec::new(),
            // Filled in by runtime-core::send_turn after the turn row
            // is created and the per-image write_attachment() calls
            // succeed.
            input_attachments: Vec::new(),
            usage: None,
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

        turn.output = output;
        turn.status = status;
        turn.updated_at = now.clone();

        session.summary.status = match status {
            TurnStatus::Interrupted => SessionStatus::Interrupted,
            _ => SessionStatus::Ready,
        };
        session.summary.updated_at = now;

        Some(turn.clone())
    }

    pub fn interrupt_session(&self, session: &mut SessionDetail, message: &str) {
        let now = Utc::now().to_rfc3339();
        session.summary.status = SessionStatus::Interrupted;
        session.summary.updated_at = now.clone();

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
