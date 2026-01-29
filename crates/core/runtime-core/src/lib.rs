use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use chrono::Utc;
use futures::future::join_all;
use tokio::sync::{Mutex, broadcast};
use zenui_orchestration::OrchestrationService;
use zenui_persistence::PersistenceService;
use zenui_provider_api::{
    AppSnapshot, BootstrapPayload, ClientMessage, FileChangeRecord, PermissionDecision,
    PermissionMode, PlanRecord, PlanStatus, ProviderAdapter, ProviderKind, ProviderStatus,
    ProviderTurnEvent, ReasoningEffort, RuntimeEvent, ServerMessage, SessionDetail, SessionStatus,
    SubagentRecord, SubagentStatus, ToolCall, ToolCallStatus, TurnEventSink, TurnStatus,
    UserInputAnswer,
};

const MODEL_CACHE_TTL_HOURS: i64 = 24;

/// Observer for turn lifecycle events. Exists so daemon-core can plug in a
/// `DaemonLifecycle` counter for idle shutdown without runtime-core depending
/// on any daemon crate. Default path (no observer) is a no-op.
pub trait TurnLifecycleObserver: Send + Sync {
    fn on_turn_start(&self, session_id: &str);
    fn on_turn_end(&self, session_id: &str);
}

pub struct RuntimeCore {
    adapters: HashMap<ProviderKind, Arc<dyn ProviderAdapter>>,
    event_tx: broadcast::Sender<RuntimeEvent>,
    orchestration: Arc<OrchestrationService>,
    persistence: Arc<PersistenceService>,
    active_sinks: Arc<Mutex<HashMap<String, TurnEventSink>>>,
    /// Providers with an in-flight model fetch. Prevents the dual bootstrap
    /// path (HTTP + WebSocket) from spawning two parallel fetches per provider
    /// on a fresh connection.
    in_flight_model_fetches: Arc<Mutex<HashSet<ProviderKind>>>,
    turn_observer: Option<Arc<dyn TurnLifecycleObserver>>,
}

/// RAII guard that ticks the `TurnLifecycleObserver` counter around the
/// lifetime of `send_turn`. Drop runs on every exit path (normal return,
/// early `?` return, panic), so the daemon-side counter cannot leak even if
/// an adapter panics or a `.await?` unwinds the task.
struct TurnCounterGuard {
    observer: Option<Arc<dyn TurnLifecycleObserver>>,
    session_id: String,
}

impl TurnCounterGuard {
    fn new(observer: Option<Arc<dyn TurnLifecycleObserver>>, session_id: String) -> Self {
        if let Some(obs) = &observer {
            obs.on_turn_start(&session_id);
        }
        Self {
            observer,
            session_id,
        }
    }
}

impl Drop for TurnCounterGuard {
    fn drop(&mut self) {
        if let Some(obs) = &self.observer {
            obs.on_turn_end(&self.session_id);
        }
    }
}

impl RuntimeCore {
    pub fn new(
        adapters: Vec<Arc<dyn ProviderAdapter>>,
        orchestration: Arc<OrchestrationService>,
        persistence: Arc<PersistenceService>,
        turn_observer: Option<Arc<dyn TurnLifecycleObserver>>,
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
            in_flight_model_fetches: Arc::new(Mutex::new(HashSet::new())),
            turn_observer,
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
            projects: self.persistence.list_projects().await,
        }
    }

    /// Walk all persisted sessions and mark any stuck-in-flight sessions
    /// (status == Running) as Interrupted. Fixes the latent bug where a
    /// daemon crash mid-turn leaves sessions stuck in `Running` forever with
    /// no client-facing recovery path. Call once at startup before serving
    /// clients.
    pub async fn reconcile_startup(&self) {
        let sessions = self.persistence.list_sessions().await;
        let mut reconciled = 0usize;
        for mut session in sessions {
            if session.summary.status != SessionStatus::Running {
                continue;
            }
            self.orchestration.interrupt_session(
                &mut session,
                "Daemon was restarted while this turn was in flight.",
            );
            self.persistence.upsert_session(session).await;
            reconciled += 1;
        }
        if reconciled > 0 {
            tracing::info!(
                reconciled,
                "startup reconciliation marked stuck sessions as interrupted"
            );
        }
    }

    /// Walk `active_sinks`, send `interrupt_turn` to each session, and wait
    /// (up to `grace`) for the sinks to drain. Called by daemon-core during
    /// graceful shutdown. Loops-and-re-snapshots to catch races where a new
    /// turn slips in between phases. Returns the count of interrupt calls
    /// that succeeded (not necessarily the count of turns that finished
    /// cleanly within the grace window).
    pub async fn shutdown_all_turns(&self, grace: std::time::Duration) -> usize {
        let deadline = std::time::Instant::now() + grace;
        let mut total = 0usize;
        loop {
            let session_ids: Vec<String> = {
                let guard = self.active_sinks.lock().await;
                guard.keys().cloned().collect()
            };
            if session_ids.is_empty() {
                break;
            }
            for session_id in session_ids {
                match self.interrupt_turn(session_id.clone()).await {
                    Ok(_) => total += 1,
                    Err(err) => {
                        tracing::warn!(session_id, "interrupt_turn during shutdown: {err}");
                    }
                }
            }
            if std::time::Instant::now() >= deadline {
                tracing::warn!(
                    ?grace,
                    "shutdown_all_turns: grace period elapsed with active sinks remaining"
                );
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        total
    }

    pub async fn bootstrap(&self, ws_url: String) -> BootstrapPayload {
        let mut providers: Vec<ProviderStatus> =
            join_all(self.adapters.values().map(|adapter| adapter.health())).await;

        // Merge cached models into ProviderStatus.models. If a provider has no
        // cache or the cache is stale, kick off a background refresh that will
        // broadcast a ProviderModelsUpdated event when it completes.
        for status in providers.iter_mut() {
            let kind = status.kind;
            match self.persistence.get_cached_models(kind).await {
                Some((fetched_at, cached)) => {
                    tracing::info!(
                        ?kind,
                        cached_count = cached.len(),
                        ?fetched_at,
                        "loaded cached provider models"
                    );
                    status.models = cached;
                    if is_cache_stale(&fetched_at) {
                        tracing::info!(?kind, "model cache stale, refreshing in background");
                        self.spawn_model_refresh(kind);
                    }
                }
                None => {
                    tracing::info!(
                        ?kind,
                        fallback_count = status.models.len(),
                        "no cached models, using hardcoded fallback and refreshing"
                    );
                    self.spawn_model_refresh(kind);
                }
            }
        }

        BootstrapPayload {
            app_name: "zenui".to_string(),
            generated_at: Utc::now().to_rfc3339(),
            ws_url,
            providers,
            snapshot: self.snapshot().await,
        }
    }

    /// Background-fetch the model list for one provider, persist it, and
    /// broadcast a ProviderModelsUpdated event so connected clients can update.
    /// Deduped per provider — repeated calls while a fetch is in flight are
    /// ignored. Errors are logged and swallowed (cached/hardcoded list stays).
    fn spawn_model_refresh(self: &Self, kind: ProviderKind) {
        let Some(adapter) = self.adapters.get(&kind).cloned() else {
            return;
        };
        let persistence = self.persistence.clone();
        let event_tx = self.event_tx.clone();
        let in_flight = self.in_flight_model_fetches.clone();

        tokio::spawn(async move {
            // Dedupe: skip if another refresh for this provider is already running.
            {
                let mut guard = in_flight.lock().await;
                if guard.contains(&kind) {
                    tracing::debug!(?kind, "skipping duplicate model refresh");
                    return;
                }
                guard.insert(kind);
            }

            let result = adapter.fetch_models().await;

            // Always release the in-flight slot, regardless of outcome.
            {
                let mut guard = in_flight.lock().await;
                guard.remove(&kind);
            }

            match result {
                Ok(models) if !models.is_empty() => {
                    tracing::info!(
                        ?kind,
                        count = models.len(),
                        "fetched provider models, persisting and broadcasting"
                    );
                    persistence.set_cached_models(kind, &models).await;
                    let _ = event_tx.send(RuntimeEvent::ProviderModelsUpdated {
                        provider: kind,
                        models,
                    });
                }
                Ok(_) => {
                    tracing::debug!(?kind, "fetch_models returned empty list");
                }
                Err(e) => {
                    tracing::warn!(?kind, "fetch_models failed: {e}");
                }
            }
        });
    }

    pub async fn handle_client_message(&self, message: ClientMessage) -> Option<ServerMessage> {
        tracing::debug!(?message, "Received client message");
        match message {
            ClientMessage::Ping => Some(ServerMessage::Pong),
            ClientMessage::LoadSnapshot => Some(ServerMessage::Snapshot {
                snapshot: self.snapshot().await,
            }),
            ClientMessage::StartSession {
                provider,
                title,
                model,
                project_id,
            } => {
                tracing::info!(?provider, ?model, ?project_id, "Starting session");
                match self.start_session(provider, title, model, project_id).await {
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
                reasoning_effort,
            } => {
                let mode = permission_mode.unwrap_or_default();
                match self.send_turn(session_id, input, mode, reasoning_effort).await {
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
                answers,
            } => {
                self.answer_question(&session_id, &request_id, answers).await;
                Some(ServerMessage::Ack {
                    message: "Question answer recorded.".to_string(),
                })
            }
            ClientMessage::CancelQuestion {
                session_id,
                request_id,
            } => {
                self.cancel_question(&session_id, &request_id).await;
                Some(ServerMessage::Ack {
                    message: "Question cancelled.".to_string(),
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
            ClientMessage::RefreshModels { provider } => {
                self.spawn_model_refresh(provider);
                Some(ServerMessage::Ack {
                    message: format!("Refreshing models for {}.", provider.label()),
                })
            }
            ClientMessage::CreateProject { name } => {
                match self.persistence.create_project(name).await {
                    Some(project) => {
                        self.publish(RuntimeEvent::ProjectCreated {
                            project: project.clone(),
                        });
                        Some(ServerMessage::Ack {
                            message: format!("Project `{}` created.", project.name),
                        })
                    }
                    None => Some(ServerMessage::Error {
                        message: "Project name cannot be empty.".to_string(),
                    }),
                }
            }
            ClientMessage::RenameProject { project_id, name } => {
                match self.persistence.rename_project(&project_id, name.clone()).await {
                    Some(updated_at) => {
                        let trimmed = name.trim().to_string();
                        self.publish(RuntimeEvent::ProjectRenamed {
                            project_id,
                            name: trimmed,
                            updated_at,
                        });
                        Some(ServerMessage::Ack {
                            message: "Project renamed.".to_string(),
                        })
                    }
                    None => Some(ServerMessage::Error {
                        message: "Rename failed — project not found or name empty.".to_string(),
                    }),
                }
            }
            ClientMessage::DeleteProject { project_id } => {
                match self.persistence.delete_project(&project_id).await {
                    Some(reassigned_session_ids) => {
                        self.publish(RuntimeEvent::ProjectDeleted {
                            project_id,
                            reassigned_session_ids,
                        });
                        Some(ServerMessage::Ack {
                            message: "Project deleted.".to_string(),
                        })
                    }
                    None => Some(ServerMessage::Error {
                        message: "Delete failed — project not found.".to_string(),
                    }),
                }
            }
            ClientMessage::AssignSessionToProject {
                session_id,
                project_id,
            } => {
                let updated = self
                    .persistence
                    .assign_session_to_project(&session_id, project_id.as_deref())
                    .await;
                if updated {
                    self.publish(RuntimeEvent::SessionProjectAssigned {
                        session_id,
                        project_id,
                    });
                    Some(ServerMessage::Ack {
                        message: "Session assignment updated.".to_string(),
                    })
                } else {
                    Some(ServerMessage::Error {
                        message: "Session not found.".to_string(),
                    })
                }
            }
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

    async fn answer_question(
        &self,
        session_id: &str,
        request_id: &str,
        answers: Vec<UserInputAnswer>,
    ) {
        let sink = self.active_sinks.lock().await.get(session_id).cloned();
        if let Some(sink) = sink {
            sink.resolve_question(request_id, answers).await;
        } else {
            tracing::warn!(session_id, request_id, "no active sink for question answer");
        }
    }

    async fn cancel_question(&self, session_id: &str, request_id: &str) {
        let sink = self.active_sinks.lock().await.get(session_id).cloned();
        if let Some(sink) = sink {
            sink.cancel_question(request_id).await;
        } else {
            tracing::warn!(
                session_id,
                request_id,
                "no active sink for question cancellation"
            );
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
        self.send_turn(session_id, follow_up, PermissionMode::AcceptEdits, None)
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
        project_id: Option<String>,
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

        let mut session = self
            .orchestration
            .create_session(provider, title, model, project_id);
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
        reasoning_effort: Option<ReasoningEffort>,
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

        let turn = self.orchestration.start_turn(
            &mut session,
            trimmed.clone(),
            Some(permission_mode),
            reasoning_effort,
        );
        self.persistence.upsert_session(session.clone()).await;
        self.publish(RuntimeEvent::TurnStarted {
            session_id: session.summary.session_id.clone(),
            turn: turn.clone(),
        });

        // RAII lifecycle counter: increments daemon's in_flight_turns here,
        // decrements unconditionally on any exit (return, ?, panic). Keeps the
        // daemon from auto-shutting-down while a turn is actually running.
        let _turn_guard = TurnCounterGuard::new(
            self.turn_observer.clone(),
            session.summary.session_id.clone(),
        );

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
        let active_sinks_for_cleanup = self.active_sinks.clone();
        let sid_for_cleanup = session.summary.session_id.clone();
        let adapter_fut = tokio::spawn(async move {
            let result = adapter_clone
                .execute_turn(
                    &session_clone,
                    &trimmed_clone,
                    permission_mode,
                    reasoning_effort,
                    adapter_sink,
                )
                .await;
            // Drop the sink clone held in `active_sinks` from inside the task so
            // the mpsc channel closes as soon as this task finishes (the task's
            // own `adapter_sink` goes out of scope at return). Without this,
            // the `event_rx.recv()` drain loop would wait forever on the
            // lingering `active_sinks` clone.
            active_sinks_for_cleanup
                .lock()
                .await
                .remove(&sid_for_cleanup);
            result
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
                    questions,
                } => {
                    self.publish(RuntimeEvent::UserQuestionAsked {
                        session_id: sid.clone(),
                        turn_id: tid.clone(),
                        request_id,
                        questions,
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

/// Returns true if the ISO-8601 `fetched_at` timestamp is older than the model
/// cache TTL. Unparseable timestamps are treated as stale so we'll re-fetch.
fn is_cache_stale(fetched_at: &str) -> bool {
    match chrono::DateTime::parse_from_rfc3339(fetched_at) {
        Ok(parsed) => {
            let age = Utc::now().signed_duration_since(parsed.with_timezone(&Utc));
            age > chrono::Duration::hours(MODEL_CACHE_TTL_HOURS)
        }
        Err(_) => true,
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
        ProviderStatusLevel, ProviderTurnOutput, ReasoningEffort, SessionDetail, TurnEventSink,
        TurnStatus,
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
            _reasoning_effort: Option<ReasoningEffort>,
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
            None,
        );

        let response = runtime
            .handle_client_message(ClientMessage::StartSession {
                provider: ProviderKind::Codex,
                title: Some("Test Session".to_string()),
                model: None,
                project_id: None,
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
                reasoning_effort: None,
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

    struct SlowFakeAdapter;

    #[async_trait]
    impl ProviderAdapter for SlowFakeAdapter {
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
            _reasoning_effort: Option<ReasoningEffort>,
            _events: TurnEventSink,
        ) -> Result<ProviderTurnOutput, String> {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            Ok(ProviderTurnOutput {
                output: format!("slow response for {input}"),
                provider_state: None,
            })
        }
    }

    /// Proves the turn drain loop does not depend on a live broadcast subscriber.
    /// If it did, `send_turn` would hang forever here and the timeout would fire.
    #[tokio::test]
    async fn turn_completes_without_subscribers() {
        let runtime = RuntimeCore::new(
            vec![Arc::new(SlowFakeAdapter)],
            Arc::new(OrchestrationService::new()),
            Arc::new(PersistenceService::in_memory().expect("in-memory db should initialize")),
            None,
        );

        runtime
            .handle_client_message(ClientMessage::StartSession {
                provider: ProviderKind::Codex,
                title: Some("No-subscriber Test".to_string()),
                model: None,
                project_id: None,
            })
            .await;

        let snapshot = runtime.snapshot().await;
        let session_id = snapshot
            .sessions
            .first()
            .expect("session should exist")
            .summary
            .session_id
            .clone();

        let response = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            runtime.handle_client_message(ClientMessage::SendTurn {
                session_id: session_id.clone(),
                input: "hello".to_string(),
                permission_mode: None,
                reasoning_effort: None,
            }),
        )
        .await
        .expect("send_turn must complete even with zero subscribers");

        assert!(matches!(
            response,
            Some(zenui_provider_api::ServerMessage::Ack { .. })
        ));

        let snapshot = runtime.snapshot().await;
        let session = snapshot.sessions.first().expect("session should exist");
        assert_eq!(session.turns.len(), 1);
        assert_eq!(session.turns[0].status, TurnStatus::Completed);
        assert_eq!(session.turns[0].output, "slow response for hello");
    }

    /// reconcile_startup should flip any session whose persisted status is
    /// Running (because a prior daemon crashed mid-turn) to Interrupted.
    #[tokio::test]
    async fn reconcile_startup_fixes_stuck_running_sessions() {
        let persistence =
            Arc::new(PersistenceService::in_memory().expect("in-memory db should initialize"));
        let runtime = RuntimeCore::new(
            vec![Arc::new(FakeAdapter)],
            Arc::new(OrchestrationService::new()),
            persistence.clone(),
            None,
        );

        // Create a session and hand-stamp it as Running to simulate a prior
        // daemon crash that never got a chance to flip it back.
        runtime
            .handle_client_message(ClientMessage::StartSession {
                provider: ProviderKind::Codex,
                title: Some("Stuck".to_string()),
                model: None,
                project_id: None,
            })
            .await;

        let snapshot = runtime.snapshot().await;
        let mut session = snapshot
            .sessions
            .first()
            .expect("session should exist")
            .clone();
        session.summary.status = zenui_provider_api::SessionStatus::Running;
        session.turns.push(zenui_provider_api::TurnRecord {
            turn_id: "turn-1".to_string(),
            input: "hi".to_string(),
            output: String::new(),
            status: TurnStatus::Running,
            created_at: chrono::Utc::now().to_rfc3339(),
            updated_at: chrono::Utc::now().to_rfc3339(),
            reasoning: None,
            tool_calls: Vec::new(),
            file_changes: Vec::new(),
            subagents: Vec::new(),
            plan: None,
            permission_mode: None,
            reasoning_effort: None,
        });
        persistence.upsert_session(session).await;

        runtime.reconcile_startup().await;

        let snapshot = runtime.snapshot().await;
        let session = snapshot.sessions.first().expect("session should exist");
        assert_eq!(session.summary.status, zenui_provider_api::SessionStatus::Interrupted);
        let last_turn = session.turns.last().expect("turn should exist");
        assert_eq!(last_turn.status, TurnStatus::Interrupted);
    }
}
