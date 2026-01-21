use std::collections::HashMap;
use std::sync::Arc;

use chrono::Utc;
use futures::future::join_all;
use tokio::sync::{Mutex, broadcast};
use zenui_orchestration::OrchestrationService;
use zenui_persistence::PersistenceService;
use zenui_provider_api::{
    AppSnapshot, BootstrapPayload, ClientMessage, FileChangeRecord, PermissionDecision,
    PermissionMode, PlanRecord, PlanStatus, ProviderAdapter, ProviderKind, ProviderTurnEvent,
    RuntimeEvent, ServerMessage, SessionDetail, SubagentRecord, SubagentStatus, ToolCall,
    ToolCallStatus, TurnEventSink, TurnStatus,
};

pub struct RuntimeCore {
    adapters: HashMap<ProviderKind, Arc<dyn ProviderAdapter>>,
    event_tx: broadcast::Sender<RuntimeEvent>,
    orchestration: Arc<OrchestrationService>,
    persistence: Arc<PersistenceService>,
    active_sinks: Arc<Mutex<HashMap<String, TurnEventSink>>>,
}

impl RuntimeCore {
    pub fn new(
        adapters: Vec<Arc<dyn ProviderAdapter>>,
        orchestration: Arc<OrchestrationService>,
        persistence: Arc<PersistenceService>,
    ) -> Self {
        let adapters = adapters
            .into_iter()
            .map(|adapter| (adapter.kind(), adapter))
            .collect::<HashMap<_, _>>();
        let (event_tx, _) = broadcast::channel(128);

        let registered: Vec<_> = adapters.keys().map(|k| k.label()).collect();
        tracing::info!(?registered, "Registered provider adapters");

        Self {
            adapters,
            event_tx,
            orchestration,
            persistence,
            active_sinks: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<RuntimeEvent> {
        self.event_tx.subscribe()
    }

    pub fn publish(&self, event: RuntimeEvent) {
        let _ = self.event_tx.send(event);
    }

    pub async fn snapshot(&self) -> AppSnapshot {
        AppSnapshot {
            generated_at: Utc::now().to_rfc3339(),
            sessions: self.persistence.list_sessions().await,
        }
    }

    pub async fn bootstrap(&self, ws_url: String) -> BootstrapPayload {
        let providers = join_all(self.adapters.values().map(|adapter| adapter.health())).await;

        BootstrapPayload {
            app_name: "zenui".to_string(),
            generated_at: Utc::now().to_rfc3339(),
            ws_url,
            providers,
            snapshot: self.snapshot().await,
        }
    }

    pub async fn handle_client_message(&self, message: ClientMessage) -> Option<ServerMessage> {
        tracing::debug!(?message, "Received client message");
        match message {
            ClientMessage::Ping => Some(ServerMessage::Pong),
            ClientMessage::LoadSnapshot => Some(ServerMessage::Snapshot {
                snapshot: self.snapshot().await,
            }),
            ClientMessage::StartSession { provider, title, model } => {
                tracing::info!(?provider, ?model, "Starting session");
                match self.start_session(provider, title, model).await {
                    Ok(session) => Some(ServerMessage::SessionCreated {
                        session: session.summary,
                    }),
                    Err(error) => Some(ServerMessage::Error { message: error }),
                }
            }
            ClientMessage::SendTurn {
                session_id,
                input,
                permission_mode,
            } => {
                let mode = permission_mode.unwrap_or_default();
                match self.send_turn(session_id, input, mode).await {
                    Ok(message) => Some(ServerMessage::Ack { message }),
                    Err(error) => Some(ServerMessage::Error { message: error }),
                }
            }
            ClientMessage::InterruptTurn { session_id } => {
                match self.interrupt_turn(session_id).await {
                    Ok(message) => Some(ServerMessage::Ack { message }),
                    Err(error) => Some(ServerMessage::Error { message: error }),
                }
            }
            ClientMessage::DeleteSession { session_id } => {
                match self.delete_session(session_id).await {
                    Ok(message) => Some(ServerMessage::Ack { message }),
                    Err(error) => Some(ServerMessage::Error { message: error }),
                }
            }
            ClientMessage::AnswerPermission {
                session_id,
                request_id,
                decision,
            } => {
                self.answer_permission(&session_id, &request_id, decision).await;
                Some(ServerMessage::Ack {
                    message: "Permission answer recorded.".to_string(),
                })
            }
            ClientMessage::AnswerQuestion {
                session_id,
                request_id,
                answer,
            } => {
                self.answer_question(&session_id, &request_id, answer).await;
                Some(ServerMessage::Ack {
                    message: "Question answer recorded.".to_string(),
                })
            }
            ClientMessage::AcceptPlan {
                session_id,
                plan_id,
            } => match self.accept_plan(session_id, plan_id).await {
                Ok(message) => Some(ServerMessage::Ack { message }),
                Err(error) => Some(ServerMessage::Error { message: error }),
            },
            ClientMessage::RejectPlan {
                session_id,
                plan_id,
            } => match self.reject_plan(session_id, plan_id).await {
                Ok(message) => Some(ServerMessage::Ack { message }),
                Err(error) => Some(ServerMessage::Error { message: error }),
            },
        }
    }

    async fn answer_permission(
        &self,
        session_id: &str,
        request_id: &str,
        decision: PermissionDecision,
    ) {
        let sink = self.active_sinks.lock().await.get(session_id).cloned();
        if let Some(sink) = sink {
            sink.resolve_permission(request_id, decision).await;
        } else {
            tracing::warn!(session_id, request_id, "no active sink for permission answer");
        }
    }

    async fn answer_question(&self, session_id: &str, request_id: &str, answer: String) {
        let sink = self.active_sinks.lock().await.get(session_id).cloned();
        if let Some(sink) = sink {
            sink.resolve_question(request_id, answer).await;
        } else {
            tracing::warn!(session_id, request_id, "no active sink for question answer");
        }
    }

    async fn accept_plan(&self, session_id: String, plan_id: String) -> Result<String, String> {
        let mut session = self
            .persistence
            .get_session(&session_id)
            .await
            .ok_or_else(|| format!("Unknown session `{session_id}`."))?;

        let plan_raw = {
            let mut found = None;
            for turn in session.turns.iter_mut().rev() {
                if let Some(plan) = turn.plan.as_mut() {
                    if plan.plan_id == plan_id {
                        plan.status = PlanStatus::Accepted;
                        found = Some(plan.raw.clone());
                        break;
                    }
                }
            }
            found.ok_or_else(|| format!("Unknown plan `{plan_id}`."))?
        };
        self.persistence.upsert_session(session.clone()).await;

        let follow_up = format!("Proceed with the plan above.\n\nPlan:\n{plan_raw}");
        self.send_turn(session_id, follow_up, PermissionMode::AcceptEdits)
            .await
    }

    async fn reject_plan(&self, session_id: String, plan_id: String) -> Result<String, String> {
        let mut session = self
            .persistence
            .get_session(&session_id)
            .await
            .ok_or_else(|| format!("Unknown session `{session_id}`."))?;

        for turn in session.turns.iter_mut().rev() {
            if let Some(plan) = turn.plan.as_mut() {
                if plan.plan_id == plan_id {
                    plan.status = PlanStatus::Rejected;
                    self.persistence.upsert_session(session.clone()).await;
                    return Ok("Plan rejected.".to_string());
                }
            }
        }
        Err(format!("Unknown plan `{plan_id}`."))
    }

    async fn start_session(
        &self,
        provider: ProviderKind,
        title: Option<String>,
        model: Option<String>,
    ) -> Result<SessionDetail, String> {
        tracing::info!(?provider, "Looking up adapter for provider");
        let available: Vec<_> = self.adapters.keys().map(|k| k.label()).collect();
        tracing::debug!(?available, "Available adapters");

        let adapter = self
            .adapters
            .get(&provider)
            .ok_or_else(|| {
                tracing::error!(?provider, ?available, "Adapter not found for provider");
                format!("No adapter registered for {}.", provider.label())
            })?
            .clone();

        let mut session = self.orchestration.create_session(provider, title, model);
        tracing::info!("Session created in orchestration, calling adapter.start_session");

        match adapter.start_session(&session).await {
            Ok(provider_state) => {
                tracing::info!("Adapter start_session succeeded");
                session.provider_state = provider_state;
            }
            Err(error) => {
                tracing::error!(?error, "Adapter start_session failed");
                return Err(format!("Failed to start {} session: {}", provider.label(), error));
            }
        }

        self.persistence.upsert_session(session.clone()).await;
        self.publish(RuntimeEvent::SessionStarted {
            session: session.summary.clone(),
        });
        Ok(session)
    }

    async fn send_turn(
        &self,
        session_id: String,
        input: String,
        permission_mode: PermissionMode,
    ) -> Result<String, String> {
        let trimmed = input.trim().to_string();
        if trimmed.is_empty() {
            return Err("Turn input cannot be empty.".to_string());
        }

        let mut session = self
            .persistence
            .get_session(&session_id)
            .await
            .ok_or_else(|| format!("Unknown session `{session_id}`."))?;
        let adapter = self
            .adapters
            .get(&session.summary.provider)
            .ok_or_else(|| {
                format!(
                    "No adapter registered for {}.",
                    session.summary.provider.label()
                )
            })?
            .clone();

        let turn =
            self.orchestration
                .start_turn(&mut session, trimmed.clone(), Some(permission_mode));
        self.persistence.upsert_session(session.clone()).await;
        self.publish(RuntimeEvent::TurnStarted {
            session_id: session.summary.session_id.clone(),
            turn: turn.clone(),
        });

        // Set up streaming channel: adapter pushes events, we forward them.
        let (event_tx, mut event_rx) = tokio::sync::mpsc::channel::<ProviderTurnEvent>(64);
        let sink = TurnEventSink::new(event_tx);

        // Register the sink so AnswerPermission/AnswerQuestion can route to it.
        {
            let mut guard = self.active_sinks.lock().await;
            guard.insert(session.summary.session_id.clone(), sink.clone());
        }

        // Spawn the adapter call so it runs concurrently with our event drain loop.
        let adapter_clone = adapter.clone();
        let session_clone = session.clone();
        let trimmed_clone = trimmed.clone();
        let adapter_sink = sink.clone();
        let adapter_fut = tokio::spawn(async move {
            adapter_clone
                .execute_turn(
                    &session_clone,
                    &trimmed_clone,
                    permission_mode,
                    adapter_sink,
                )
                .await
        });
        // Drop our local sink reference so the channel closes once the adapter task exits.
        drop(sink);

        // Drain streaming events, broadcasting each to websocket clients and accumulating
        // structured records onto the running turn.
        let mut accumulated = String::new();
        let mut reasoning = String::new();
        let mut tool_calls: Vec<ToolCall> = Vec::new();
        let mut file_changes: Vec<FileChangeRecord> = Vec::new();
        let mut subagents: Vec<SubagentRecord> = Vec::new();
        let mut plan: Option<PlanRecord> = None;
        let sid = session.summary.session_id.clone();
        let tid = turn.turn_id.clone();

        while let Some(ev) = event_rx.recv().await {
            match ev {
                ProviderTurnEvent::AssistantTextDelta { delta } => {
                    accumulated.push_str(&delta);
                    self.publish(RuntimeEvent::ContentDelta {
                        session_id: sid.clone(),
                        turn_id: tid.clone(),
                        delta,
                        accumulated_output: accumulated.clone(),
                    });
                }
                ProviderTurnEvent::ReasoningDelta { delta } => {
                    reasoning.push_str(&delta);
                    self.publish(RuntimeEvent::ReasoningDelta {
                        session_id: sid.clone(),
                        turn_id: tid.clone(),
                        delta,
                    });
                }
                ProviderTurnEvent::ToolCallStarted { call_id, name, args } => {
                    tool_calls.push(ToolCall {
                        call_id: call_id.clone(),
                        name: name.clone(),
                        args: args.clone(),
                        output: None,
                        error: None,
                        status: ToolCallStatus::Pending,
                    });
                    self.publish(RuntimeEvent::ToolCallStarted {
                        session_id: sid.clone(),
                        turn_id: tid.clone(),
                        call_id,
                        name,
                        args,
                    });
                }
                ProviderTurnEvent::ToolCallCompleted { call_id, output, error } => {
                    if let Some(tc) = tool_calls.iter_mut().find(|tc| tc.call_id == call_id) {
                        tc.output = Some(output.clone());
                        tc.error = error.clone();
                        tc.status = if error.is_some() {
                            ToolCallStatus::Failed
                        } else {
                            ToolCallStatus::Completed
                        };
                    }
                    self.publish(RuntimeEvent::ToolCallCompleted {
                        session_id: sid.clone(),
                        turn_id: tid.clone(),
                        call_id,
                        output,
                        error,
                    });
                }
                ProviderTurnEvent::Info { message } => {
                    self.publish(RuntimeEvent::Info { message });
                }
                ProviderTurnEvent::PermissionRequest {
                    request_id,
                    tool_name,
                    input,
                    suggested_decision,
                } => {
                    self.publish(RuntimeEvent::PermissionRequested {
                        session_id: sid.clone(),
                        turn_id: tid.clone(),
                        request_id,
                        tool_name,
                        input,
                        suggested: suggested_decision,
                    });
                }
                ProviderTurnEvent::UserQuestion {
                    request_id,
                    question,
                } => {
                    self.publish(RuntimeEvent::UserQuestionAsked {
                        session_id: sid.clone(),
                        turn_id: tid.clone(),
                        request_id,
                        question,
                    });
                }
                ProviderTurnEvent::FileChange {
                    call_id,
                    path,
                    operation,
                    before,
                    after,
                } => {
                    file_changes.push(FileChangeRecord {
                        call_id: call_id.clone(),
                        path: path.clone(),
                        operation,
                        before: before.clone(),
                        after: after.clone(),
                    });
                    self.publish(RuntimeEvent::FileChanged {
                        session_id: sid.clone(),
                        turn_id: tid.clone(),
                        call_id,
                        path,
                        operation,
                        before,
                        after,
                    });
                }
                ProviderTurnEvent::SubagentStarted {
                    parent_call_id,
                    agent_id,
                    agent_type,
                    prompt,
                } => {
                    subagents.push(SubagentRecord {
                        agent_id: agent_id.clone(),
                        parent_call_id: parent_call_id.clone(),
                        agent_type: agent_type.clone(),
                        prompt: prompt.clone(),
                        events: Vec::new(),
                        output: None,
                        error: None,
                        status: SubagentStatus::Running,
                    });
                    self.publish(RuntimeEvent::SubagentStarted {
                        session_id: sid.clone(),
                        turn_id: tid.clone(),
                        parent_call_id,
                        agent_id,
                        agent_type,
                        prompt,
                    });
                }
                ProviderTurnEvent::SubagentEvent { agent_id, event } => {
                    if let Some(rec) = subagents.iter_mut().find(|r| r.agent_id == agent_id) {
                        rec.events.push(event.clone());
                    }
                    self.publish(RuntimeEvent::SubagentEvent {
                        session_id: sid.clone(),
                        turn_id: tid.clone(),
                        agent_id,
                        event,
                    });
                }
                ProviderTurnEvent::SubagentCompleted {
                    agent_id,
                    output,
                    error,
                } => {
                    if let Some(rec) = subagents.iter_mut().find(|r| r.agent_id == agent_id) {
                        rec.output = Some(output.clone());
                        rec.error = error.clone();
                        rec.status = if error.is_some() {
                            SubagentStatus::Failed
                        } else {
                            SubagentStatus::Completed
                        };
                    }
                    self.publish(RuntimeEvent::SubagentCompleted {
                        session_id: sid.clone(),
                        turn_id: tid.clone(),
                        agent_id,
                        output,
                        error,
                    });
                }
                ProviderTurnEvent::PlanProposed {
                    plan_id,
                    title,
                    steps,
                    raw,
                } => {
                    plan = Some(PlanRecord {
                        plan_id: plan_id.clone(),
                        title: title.clone(),
                        steps: steps.clone(),
                        raw: raw.clone(),
                        status: PlanStatus::Proposed,
                    });
                    self.publish(RuntimeEvent::PlanProposed {
                        session_id: sid.clone(),
                        turn_id: tid.clone(),
                        plan_id,
                        title,
                        steps,
                        raw,
                    });
                }
            }
        }

        // Join the adapter task.
        let adapter_result = adapter_fut
            .await
            .map_err(|e| format!("Adapter task panicked: {e}"))?;

        // Drop the sink registration whichever way the adapter exits.
        {
            let mut guard = self.active_sinks.lock().await;
            guard.remove(&sid);
        }

        match adapter_result {
            Ok(output) => {
                if output.provider_state.is_some() {
                    session.provider_state = output.provider_state.clone();
                }

                // Use the adapter's canonical output if non-empty, else fall back to accumulated.
                let canonical = if !output.output.trim().is_empty() {
                    output.output
                } else {
                    accumulated
                };

                let completed_turn = self
                    .orchestration
                    .finish_turn(
                        &mut session,
                        &turn.turn_id,
                        canonical,
                        TurnStatus::Completed,
                    )
                    .ok_or_else(|| format!("Unknown turn `{}`.", turn.turn_id))?;

                // Attach reasoning, tool_calls, and structured records to the persisted turn.
                let merged_turn = if let Some(t) = session
                    .turns
                    .iter_mut()
                    .find(|t| t.turn_id == completed_turn.turn_id)
                {
                    if !reasoning.is_empty() {
                        t.reasoning = Some(reasoning);
                    }
                    t.tool_calls = tool_calls;
                    t.file_changes = file_changes;
                    t.subagents = subagents;
                    t.plan = plan;
                    t.clone()
                } else {
                    completed_turn
                };

                self.persistence.upsert_session(session.clone()).await;
                self.publish(RuntimeEvent::TurnCompleted {
                    session_id: session.summary.session_id.clone(),
                    session: session.summary.clone(),
                    turn: merged_turn,
                });

                Ok("Turn completed.".to_string())
            }
            Err(error) => {
                let failed_turn = self
                    .orchestration
                    .finish_turn(
                        &mut session,
                        &turn.turn_id,
                        error.clone(),
                        TurnStatus::Failed,
                    )
                    .ok_or_else(|| format!("Unknown turn `{}`.", turn.turn_id))?;

                self.persistence.upsert_session(session.clone()).await;
                self.publish(RuntimeEvent::Error {
                    message: failed_turn.output.clone(),
                });
                self.publish(RuntimeEvent::TurnCompleted {
                    session_id: session.summary.session_id.clone(),
                    session: session.summary.clone(),
                    turn: failed_turn,
                });

                Err(error)
            }
        }
    }

    async fn interrupt_turn(&self, session_id: String) -> Result<String, String> {
        let mut session = self
            .persistence
            .get_session(&session_id)
            .await
            .ok_or_else(|| format!("Unknown session `{session_id}`."))?;
        let adapter = self
            .adapters
            .get(&session.summary.provider)
            .ok_or_else(|| {
                format!(
                    "No adapter registered for {}.",
                    session.summary.provider.label()
                )
            })?
            .clone();

        let message = adapter.interrupt_turn(&session).await?;
        self.orchestration.interrupt_session(&mut session, &message);
        self.persistence.upsert_session(session.clone()).await;
        self.publish(RuntimeEvent::SessionInterrupted {
            session: session.summary.clone(),
            message: message.clone(),
        });
        Ok(message)
    }

    async fn delete_session(&self, session_id: String) -> Result<String, String> {
        let session = self
            .persistence
            .get_session(&session_id)
            .await
            .ok_or_else(|| format!("Unknown session `{session_id}`."))?;

        let adapter = self.adapters.get(&session.summary.provider).cloned();

        // Best-effort teardown of the provider's long-lived resources.
        if let Some(adapter) = adapter {
            if let Err(e) = adapter.end_session(&session).await {
                tracing::warn!("end_session error for {session_id}: {e}");
            }
        }

        if !self.persistence.delete_session(&session_id) {
            return Err(format!("Session `{session_id}` could not be deleted."));
        }

        self.publish(RuntimeEvent::SessionDeleted {
            session_id: session_id.clone(),
        });

        Ok(format!("Session {session_id} deleted."))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use async_trait::async_trait;
    use zenui_orchestration::OrchestrationService;
    use zenui_persistence::PersistenceService;
    use zenui_provider_api::{
        ClientMessage, PermissionMode, ProviderAdapter, ProviderKind, ProviderStatus,
        ProviderStatusLevel, ProviderTurnOutput, SessionDetail, TurnEventSink,
    };

    use super::RuntimeCore;

    struct FakeAdapter;

    #[async_trait]
    impl ProviderAdapter for FakeAdapter {
        fn kind(&self) -> ProviderKind {
            ProviderKind::Codex
        }

        async fn health(&self) -> ProviderStatus {
            ProviderStatus {
                kind: ProviderKind::Codex,
                label: "Codex".to_string(),
                installed: true,
                authenticated: true,
                version: Some("test".to_string()),
                status: ProviderStatusLevel::Ready,
                message: None,
                models: vec![],
            }
        }

        async fn execute_turn(
            &self,
            _session: &SessionDetail,
            input: &str,
            _permission_mode: PermissionMode,
            _events: TurnEventSink,
        ) -> Result<ProviderTurnOutput, String> {
            Ok(ProviderTurnOutput {
                output: format!("fake response for {input}"),
                provider_state: None,
            })
        }
    }

    #[tokio::test]
    async fn creates_session_and_turn_snapshot() {
        let runtime = RuntimeCore::new(
            vec![Arc::new(FakeAdapter)],
            Arc::new(OrchestrationService::new()),
            Arc::new(PersistenceService::in_memory().expect("in-memory db should initialize")),
        );

        let response = runtime
            .handle_client_message(ClientMessage::StartSession {
                provider: ProviderKind::Codex,
                title: Some("Test Session".to_string()),
                model: None,
            })
            .await;
        assert!(matches!(
            response,
            Some(zenui_provider_api::ServerMessage::SessionCreated { .. })
        ));

        let snapshot = runtime.snapshot().await;
        let session = snapshot.sessions.first().expect("session should exist");
        assert_eq!(session.summary.title, "Test Session");

        let response = runtime
            .handle_client_message(ClientMessage::SendTurn {
                session_id: session.summary.session_id.clone(),
                input: "hello".to_string(),
                permission_mode: None,
            })
            .await;
        assert!(matches!(
            response,
            Some(zenui_provider_api::ServerMessage::Ack { .. })
        ));

        let snapshot = runtime.snapshot().await;
        let session = snapshot.sessions.first().expect("session should exist");
        assert_eq!(session.turns.len(), 1);
        assert_eq!(session.turns[0].output, "fake response for hello");
    }
}
