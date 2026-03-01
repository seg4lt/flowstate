use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};

use chrono::Utc;
use tokio::sync::{Mutex, broadcast};
use zenui_orchestration::OrchestrationService;
use zenui_persistence::PersistenceService;
use zenui_provider_api::{
    AppSnapshot, BootstrapPayload, ClientMessage, ContentBlock, FileChangeRecord,
    PermissionDecision, PermissionMode, PlanRecord, PlanStatus, ProviderAdapter, ProviderKind,
    ProviderStatus, ProviderTurnEvent, ReasoningEffort, RuntimeEvent, ServerMessage,
    SessionDetail, SessionStatus, SubagentRecord, SubagentStatus, ToolCall, ToolCallStatus,
    TurnEventSink, TurnRecord, TurnStatus, UserInputAnswer,
};

const MODEL_CACHE_TTL_HOURS: i64 = 24;

/// Observer for turn lifecycle events. Exists so daemon-core can plug in a
/// `DaemonLifecycle` counter for idle shutdown without runtime-core depending
/// on any daemon crate. Default path (no observer) is a no-op.
pub trait TurnLifecycleObserver: Send + Sync {
    fn on_turn_start(&self, session_id: &str);
    fn on_turn_end(&self, session_id: &str);
}

/// Status snapshot returned by `ConnectionObserver::status` and serialized
/// by admin endpoints such as `GET /api/status` in the HTTP transport.
/// Plain serde POD — lives in `runtime-core` so transports and the daemon
/// lifecycle share a single definition without depending on each other.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DaemonStatus {
    pub connected_clients: usize,
    pub in_flight_turns: usize,
    pub uptime_seconds: u64,
    pub daemon_version: String,
    pub started_at: String,
}

/// Hooks a transport calls whenever a client connects or disconnects, plus
/// the shutdown-request hook used by daemon-owned admin routes. `daemon-core`
/// implements this on `DaemonLifecycle` to drive its idle watchdog;
/// non-daemon callers (tests, apps with no idle shutdown) pass an
/// `Arc<NoopObserver>`.
///
/// This trait is the symmetric counterpart to [`TurnLifecycleObserver`] —
/// one counts connected clients, the other counts in-flight turns.
/// Keeping both in `runtime-core` means transport crates can depend only
/// on `runtime-core` + `provider-api` without ever pulling in `daemon-core`
/// or any specific transport.
pub trait ConnectionObserver: Send + Sync {
    fn on_client_connected(&self);
    fn on_client_disconnected(&self);
    fn on_shutdown_requested(&self);
    /// Optional status snapshot for admin endpoints. Daemons override this;
    /// the default `None` is what non-daemon callers use.
    fn status(&self) -> Option<DaemonStatus> {
        None
    }
}

/// No-op `ConnectionObserver`. Hand this to a transport when you don't care
/// about connection counting (in-process tests, embedded shells without
/// idle-shutdown behavior).
pub struct NoopObserver;

impl ConnectionObserver for NoopObserver {
    fn on_client_connected(&self) {}
    fn on_client_disconnected(&self) {}
    fn on_shutdown_requested(&self) {}
}

pub struct RuntimeCore {
    adapters: HashMap<ProviderKind, Arc<dyn ProviderAdapter>>,
    event_tx: broadcast::Sender<RuntimeEvent>,
    orchestration: Arc<OrchestrationService>,
    persistence: Arc<PersistenceService>,
    active_sinks: Arc<Mutex<HashMap<String, TurnEventSink>>>,
    /// Per-session permission policy memory. Keyed by session_id; the inner
    /// map remembers tools the user answered AllowAlways/DenyAlways for, so
    /// later turns in the same session short-circuit the host prompt.
    /// Lives at runtime-core level (not on TurnEventSink directly) so it
    /// outlives any single turn.
    session_policies: Arc<Mutex<HashMap<String, zenui_provider_api::PermissionPolicy>>>,
    /// Default working directory for sessions without a project path.
    default_threads_dir: String,
    /// Providers with an in-flight model fetch. Prevents the dual bootstrap
    /// path (HTTP + WebSocket) from spawning two parallel fetches per provider
    /// on a fresh connection.
    in_flight_model_fetches: Arc<Mutex<HashSet<ProviderKind>>>,
    /// Providers with an in-flight health check. Prevents duplicate health
    /// checks when multiple transports bootstrap near-simultaneously.
    in_flight_health_checks: Arc<Mutex<HashSet<ProviderKind>>>,
    /// Live snapshot of every actively-streaming turn, keyed by session id.
    /// The accumulator inside `handle_send_turn` writes a fresh `TurnRecord`
    /// here after every mutating event so lag-recovery code paths can hand
    /// the client an authoritative view of the in-flight turn — persistence
    /// only catches state at turn completion, so any tool call dropped from
    /// the broadcast queue would otherwise be invisible until the very end.
    /// Cleared in both the success and failure exit paths.
    in_flight_turns: Arc<RwLock<HashMap<String, TurnRecord>>>,
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
        default_threads_dir: String,
    ) -> Self {
        let adapters = adapters
            .into_iter()
            .map(|adapter| (adapter.kind(), adapter))
            .collect::<HashMap<_, _>>();
        // Sized for chatty providers: a single Codex/Claude turn easily emits
        // 500–2000 events (one content_delta per token plus tool start/end
        // pairs). At 128 the receiver lagged behind during normal turns and
        // tokio dropped events from the middle of the stream, leaving tool
        // calls visually stuck on `pending` until turn_completed swept them.
        // 4096 covers a long turn end-to-end with headroom; the lag-recovery
        // path below reseeds chat-view from `live_session_detail` if a client
        // ever does fall behind.
        let (event_tx, _) = broadcast::channel(4096);

        let registered: Vec<_> = adapters.keys().map(|k| k.label()).collect();
        tracing::info!(?registered, "Registered provider adapters");

        Self {
            adapters,
            event_tx,
            orchestration,
            persistence,
            active_sinks: Arc::new(Mutex::new(HashMap::new())),
            session_policies: Arc::new(Mutex::new(HashMap::new())),
            default_threads_dir,
            in_flight_model_fetches: Arc::new(Mutex::new(HashSet::new())),
            in_flight_health_checks: Arc::new(Mutex::new(HashSet::new())),
            in_flight_turns: Arc::new(RwLock::new(HashMap::new())),
            turn_observer,
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<RuntimeEvent> {
        self.event_tx.subscribe()
    }

    pub fn publish(&self, event: RuntimeEvent) {
        let _ = self.event_tx.send(event);
    }

    /// Load a session from persistence and splice the in-flight turn (if
    /// any) into the returned `SessionDetail.turns`. Replaces by `turn_id`
    /// if a matching entry already exists, otherwise appends. Used by
    /// `LoadSession` and by lag-recovery in transports — both want the
    /// authoritative live view, not whatever happened to be persisted at
    /// the last `turn_completed`.
    pub async fn live_session_detail(&self, session_id: &str) -> Option<SessionDetail> {
        let mut detail = self.persistence.get_session(session_id).await?;
        let live = self
            .in_flight_turns
            .read()
            .ok()
            .and_then(|map| map.get(session_id).cloned());
        if let Some(live_turn) = live {
            if let Some(slot) = detail
                .turns
                .iter_mut()
                .find(|t| t.turn_id == live_turn.turn_id)
            {
                *slot = live_turn;
            } else {
                detail.turns.push(live_turn);
            }
        }
        Some(detail)
    }

    /// Returns the `SessionDetail` for every session that currently has
    /// an in-flight turn, with the live turn merged in. Transports use
    /// this on broadcast-lag recovery to atomically reseed any client
    /// that's mid-stream — without it, the lost ToolCallCompleted events
    /// would leave tool calls visually stuck on `pending` until the very
    /// end of the turn.
    pub async fn active_session_details(&self) -> Vec<SessionDetail> {
        let session_ids: Vec<String> = match self.in_flight_turns.read() {
            Ok(map) => map.keys().cloned().collect(),
            Err(_) => return Vec::new(),
        };
        let mut details = Vec::with_capacity(session_ids.len());
        for sid in session_ids {
            if let Some(detail) = self.live_session_detail(&sid).await {
                details.push(detail);
            }
        }
        details
    }

    pub async fn snapshot(&self) -> AppSnapshot {
        // Summary-only load: the sidebar only reads `session.summary.*`, and
        // shipping every turn of every session in the bootstrap payload was
        // previously the dominant startup cost. Full turn lists are fetched
        // on demand via `ClientMessage::LoadSession` when the user opens a
        // session.
        let summaries = self.persistence.list_session_summaries().await;
        let sessions = summaries
            .into_iter()
            .map(|summary| SessionDetail {
                summary,
                turns: Vec::new(),
                provider_state: None,
                cwd: None,
            })
            .collect();
        AppSnapshot {
            generated_at: Utc::now().to_rfc3339(),
            sessions,
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
        // Load cached health for every adapter so the frontend gets a
        // populated providers list immediately. If the cache is stale
        // (>24h) or missing, spawn a background health check to refresh.
        // If fresh, skip the expensive health check entirely — next launch
        // will still see the cached entry.
        let mut cached_providers: Vec<ProviderStatus> = Vec::new();
        for &kind in self.adapters.keys() {
            match self.persistence.get_cached_health(kind).await {
                Some((checked_at, status)) => {
                    tracing::info!(
                        ?kind,
                        ?checked_at,
                        "loaded cached provider health"
                    );
                    cached_providers.push(status);
                    if is_cache_stale(&checked_at) {
                        self.spawn_health_check(kind);
                    }
                }
                None => {
                    self.spawn_health_check(kind);
                }
            }
        }

        BootstrapPayload {
            app_name: "zenui".to_string(),
            generated_at: Utc::now().to_rfc3339(),
            ws_url,
            providers: cached_providers,
            snapshot: self.snapshot().await,
        }
    }

    /// Background health-check for one provider. Merges cached models into the
    /// result, kicks off a model refresh if stale, and broadcasts a
    /// `ProviderHealthUpdated` event when done. Deduped per provider.
    fn spawn_health_check(&self, kind: ProviderKind) {
        let Some(adapter) = self.adapters.get(&kind).cloned() else {
            return;
        };
        let persistence = self.persistence.clone();
        let event_tx = self.event_tx.clone();
        let in_flight = self.in_flight_health_checks.clone();
        let in_flight_models = self.in_flight_model_fetches.clone();

        tokio::spawn(async move {
            // Dedupe: skip if another health check for this provider is running.
            {
                let mut guard = in_flight.lock().await;
                if guard.contains(&kind) {
                    tracing::debug!(?kind, "skipping duplicate health check");
                    return;
                }
                guard.insert(kind);
            }

            let mut status = adapter.health().await;

            // Merge cached models into the result.
            let needs_refresh = match persistence.get_cached_models(kind).await {
                Some((fetched_at, cached)) => {
                    tracing::info!(
                        ?kind,
                        cached_count = cached.len(),
                        ?fetched_at,
                        "loaded cached provider models"
                    );
                    status.models = cached;
                    is_cache_stale(&fetched_at)
                }
                None => {
                    tracing::info!(
                        ?kind,
                        fallback_count = status.models.len(),
                        "no cached models, using hardcoded fallback and refreshing"
                    );
                    true
                }
            };

            // Persist the freshly-checked health so the next daemon start
            // can return it in the bootstrap payload without re-running
            // the slow health probe.
            persistence.set_cached_health(kind, &status).await;

            if needs_refresh {
                spawn_model_refresh_detached(
                    kind,
                    adapter.clone(),
                    persistence,
                    event_tx.clone(),
                    in_flight_models,
                );
            }

            let _ = event_tx.send(RuntimeEvent::ProviderHealthUpdated {
                status,
            });

            // Release the in-flight slot.
            {
                let mut guard = in_flight.lock().await;
                guard.remove(&kind);
            }
        });
    }

    /// Background-fetch the model list for one provider, persist it, and
    /// broadcast a ProviderModelsUpdated event so connected clients can update.
    /// Deduped per provider — repeated calls while a fetch is in flight are
    /// ignored. Errors are logged and swallowed (cached/hardcoded list stays).
    fn spawn_model_refresh(&self, kind: ProviderKind) {
        let Some(adapter) = self.adapters.get(&kind).cloned() else {
            return;
        };
        spawn_model_refresh_detached(
            kind,
            adapter,
            self.persistence.clone(),
            self.event_tx.clone(),
            self.in_flight_model_fetches.clone(),
        );
    }

    pub async fn handle_client_message(&self, message: ClientMessage) -> Option<ServerMessage> {
        tracing::debug!(?message, "Received client message");
        match message {
            ClientMessage::Ping => Some(ServerMessage::Pong),
            ClientMessage::LoadSnapshot => Some(ServerMessage::Snapshot {
                snapshot: self.snapshot().await,
            }),
            ClientMessage::LoadSession { session_id } => {
                match self.live_session_detail(&session_id).await {
                    Some(session) => Some(ServerMessage::SessionLoaded { session }),
                    None => Some(ServerMessage::Error {
                        message: format!("Session `{session_id}` not found."),
                    }),
                }
            }
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
            ClientMessage::UpdatePermissionMode {
                session_id,
                permission_mode,
            } => {
                match self.update_permission_mode(session_id, permission_mode).await {
                    Ok(()) => Some(ServerMessage::Ack {
                        message: "Permission mode updated.".to_string(),
                    }),
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
                permission_mode_override,
            } => {
                self.answer_permission(
                    &session_id,
                    &request_id,
                    decision,
                    permission_mode_override,
                )
                .await;
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
            ClientMessage::CreateProject { name, path } => {
                match self.persistence.create_project(name, path).await {
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
            ClientMessage::RenameSession { session_id, title } => {
                match self.persistence.rename_session(&session_id, title.clone()).await {
                    Some(_updated_at) => {
                        let trimmed = title.trim().to_string();
                        self.publish(RuntimeEvent::SessionRenamed {
                            session_id,
                            title: trimmed,
                        });
                        Some(ServerMessage::Ack {
                            message: "Session renamed.".to_string(),
                        })
                    }
                    None => Some(ServerMessage::Error {
                        message: "Rename failed — session not found or title empty.".to_string(),
                    }),
                }
            }
            ClientMessage::UpdateSessionModel { session_id, model } => {
                if let Some(mut session) = self.persistence.get_session(&session_id).await {
                    session.summary.model = Some(model.clone());
                    self.persistence.upsert_session(session).await;
                    self.publish(RuntimeEvent::SessionModelUpdated {
                        session_id,
                        model,
                    });
                    Some(ServerMessage::Ack {
                        message: "Session model updated.".to_string(),
                    })
                } else {
                    Some(ServerMessage::Error {
                        message: "Session not found.".to_string(),
                    })
                }
            }
            ClientMessage::ArchiveSession { session_id } => {
                if self.persistence.archive_session(&session_id).await {
                    self.publish(RuntimeEvent::SessionArchived {
                        session_id: session_id.clone(),
                    });
                    Some(ServerMessage::Ack {
                        message: "Session archived.".to_string(),
                    })
                } else {
                    Some(ServerMessage::Error {
                        message: "Archive failed — session not found.".to_string(),
                    })
                }
            }
            ClientMessage::UnarchiveSession { session_id } => {
                if let Some(session) = self.persistence.unarchive_session(&session_id).await {
                    self.publish(RuntimeEvent::SessionUnarchived {
                        session: session.summary,
                    });
                    Some(ServerMessage::Ack {
                        message: "Session unarchived.".to_string(),
                    })
                } else {
                    Some(ServerMessage::Error {
                        message: "Unarchive failed — session not found.".to_string(),
                    })
                }
            }
            ClientMessage::ListArchivedSessions => {
                let sessions = self.persistence.list_archived_session_summaries().await;
                Some(ServerMessage::ArchivedSessionsList { sessions })
            }
        }
    }

    async fn answer_permission(
        &self,
        session_id: &str,
        request_id: &str,
        decision: PermissionDecision,
        mode_override: Option<PermissionMode>,
    ) {
        let sink = self.active_sinks.lock().await.get(session_id).cloned();
        if let Some(sink) = sink {
            tracing::info!(
                session_id,
                request_id,
                ?decision,
                has_mode_override = mode_override.is_some(),
                "runtime-core routing answer_permission to sink"
            );
            sink.resolve_permission_with_mode(request_id, decision, mode_override)
                .await;
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

    async fn resolve_session_cwd(&self, session: &mut SessionDetail) {
        if let Some(ref project_id) = session.summary.project_id {
            if let Some(project) = self.persistence.get_project(project_id).await {
                if let Some(path) = project.path {
                    session.cwd = Some(path);
                    return;
                }
            }
        }
        session.cwd = Some(self.default_threads_dir.clone());
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
        self.resolve_session_cwd(&mut session).await;

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
        self.resolve_session_cwd(&mut session).await;
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

        let title_before = session.summary.title.clone();
        let turn = self.orchestration.start_turn(
            &mut session,
            trimmed.clone(),
            Some(permission_mode),
            reasoning_effort,
        );
        self.persistence.upsert_session(session.clone()).await;
        // Publish title change immediately so the sidebar updates before the turn finishes.
        if session.summary.title != title_before {
            self.publish(RuntimeEvent::SessionRenamed {
                session_id: session.summary.session_id.clone(),
                title: session.summary.title.clone(),
            });
        }
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
        // Look up (or lazily create) this session's persistent permission
        // policy so AllowAlways/DenyAlways decisions from prior turns
        // short-circuit subsequent prompts in the same session.
        let session_policy = {
            let mut guard = self.session_policies.lock().await;
            guard
                .entry(session.summary.session_id.clone())
                .or_insert_with(|| {
                    Arc::new(Mutex::new(HashMap::new()))
                })
                .clone()
        };
        let sink = TurnEventSink::with_policy(event_tx, session_policy);

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
        // Ordered content stream — text, reasoning, and tool-call positions
        // captured in the exact order events arrived. Adjacent text or
        // reasoning deltas coalesce into the trailing block; a tool call
        // closes any open run and records its position so the UI can
        // render interleaved "text → tool → text → tool" turns faithfully.
        let mut blocks: Vec<ContentBlock> = Vec::new();
        let sid = session.summary.session_id.clone();
        let tid = turn.turn_id.clone();
        // Seed the in-flight snapshot with the freshly-started turn so any
        // lag-recovery query before the first event still returns something
        // sensible (the running turn with empty content).
        write_in_flight_snapshot(
            &self.in_flight_turns,
            &sid,
            &turn,
            &accumulated,
            &reasoning,
            &tool_calls,
            &file_changes,
            &subagents,
            &plan,
            &blocks,
        );

        while let Some(ev) = event_rx.recv().await {
            match ev {
                ProviderTurnEvent::AssistantTextDelta { delta } => {
                    accumulated.push_str(&delta);
                    match blocks.last_mut() {
                        Some(ContentBlock::Text { text }) => text.push_str(&delta),
                        _ => blocks.push(ContentBlock::Text { text: delta.clone() }),
                    }
                    self.publish(RuntimeEvent::ContentDelta {
                        session_id: sid.clone(),
                        turn_id: tid.clone(),
                        delta,
                        accumulated_output: accumulated.clone(),
                    });
                }
                ProviderTurnEvent::ReasoningDelta { delta } => {
                    reasoning.push_str(&delta);
                    match blocks.last_mut() {
                        Some(ContentBlock::Reasoning { text }) => text.push_str(&delta),
                        _ => blocks.push(ContentBlock::Reasoning { text: delta.clone() }),
                    }
                    self.publish(RuntimeEvent::ReasoningDelta {
                        session_id: sid.clone(),
                        turn_id: tid.clone(),
                        delta,
                    });
                }
                ProviderTurnEvent::ToolCallStarted {
                    call_id,
                    name,
                    args,
                    parent_call_id,
                } => {
                    tool_calls.push(ToolCall {
                        call_id: call_id.clone(),
                        name: name.clone(),
                        args: args.clone(),
                        output: None,
                        error: None,
                        status: ToolCallStatus::Pending,
                        parent_call_id: parent_call_id.clone(),
                    });
                    blocks.push(ContentBlock::ToolCall {
                        call_id: call_id.clone(),
                    });
                    self.publish(RuntimeEvent::ToolCallStarted {
                        session_id: sid.clone(),
                        turn_id: tid.clone(),
                        call_id,
                        name,
                        args,
                        parent_call_id,
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
            // Refresh the live in-flight snapshot after every event so a
            // client recovering from broadcast lag gets the authoritative
            // current view of this turn (including any pending tool calls)
            // even when the events themselves were dropped from the queue.
            write_in_flight_snapshot(
                &self.in_flight_turns,
                &sid,
                &turn,
                &accumulated,
                &reasoning,
                &tool_calls,
                &file_changes,
                &subagents,
                &plan,
                &blocks,
            );
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

        // The drain loop has finished, so the in-flight snapshot is now
        // stale and persistence is about to take over. Drop the entry on
        // both success and failure exits — done here, before the match,
        // so it can't be skipped on an early `?` return.
        if let Ok(mut map) = self.in_flight_turns.write() {
            map.remove(&sid);
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
                    t.blocks = blocks;
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

    async fn update_permission_mode(
        &self,
        session_id: String,
        mode: PermissionMode,
    ) -> Result<(), String> {
        // Look up the session and forward the mode change to its
        // adapter. Adapters that don't support mid-turn switching no-op;
        // the mode will still apply to subsequent send_turn requests
        // because the frontend tracks the chosen mode and sends it on
        // the next ClientMessage::SendTurn.
        let session = self
            .live_session_detail(&session_id)
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
        adapter.update_permission_mode(&session, mode).await
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

        // Drop the session's permission policy so it doesn't grow forever.
        self.session_policies.lock().await.remove(&session_id);

        self.publish(RuntimeEvent::SessionDeleted {
            session_id: session_id.clone(),
        });

        Ok(format!("Session {session_id} deleted."))
    }
}

/// Standalone model-refresh spawner, usable from both `RuntimeCore::spawn_model_refresh`
/// and from within already-spawned tasks (like `spawn_health_check`) that don't have `&self`.
fn spawn_model_refresh_detached(
    kind: ProviderKind,
    adapter: Arc<dyn ProviderAdapter>,
    persistence: Arc<PersistenceService>,
    event_tx: broadcast::Sender<RuntimeEvent>,
    in_flight: Arc<Mutex<HashSet<ProviderKind>>>,
) {
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

/// Build a `TurnRecord` snapshot from the accumulator's local state and
/// stash it under `session_id` in the live in-flight map. Called after
/// every event in the drain loop, so the map always reflects the
/// latest known state of the running turn — that's what `live_session_detail`
/// hands back to a client recovering from broadcast lag.
#[allow(clippy::too_many_arguments)]
fn write_in_flight_snapshot(
    in_flight_turns: &Arc<RwLock<HashMap<String, TurnRecord>>>,
    session_id: &str,
    base: &TurnRecord,
    accumulated: &str,
    reasoning_text: &str,
    tool_calls: &[ToolCall],
    file_changes: &[FileChangeRecord],
    subagents: &[SubagentRecord],
    plan: &Option<PlanRecord>,
    blocks: &[ContentBlock],
) {
    let mut snap = base.clone();
    snap.output = accumulated.to_string();
    snap.reasoning = if reasoning_text.is_empty() {
        None
    } else {
        Some(reasoning_text.to_string())
    };
    snap.tool_calls = tool_calls.to_vec();
    snap.file_changes = file_changes.to_vec();
    snap.subagents = subagents.to_vec();
    snap.plan = plan.clone();
    snap.blocks = blocks.to_vec();
    if let Ok(mut map) = in_flight_turns.write() {
        map.insert(session_id.to_string(), snap);
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
        ClientMessage, ContentBlock, PermissionMode, ProviderAdapter, ProviderKind,
        ProviderStatus, ProviderStatusLevel, ProviderTurnEvent, ProviderTurnOutput,
        ReasoningEffort, SessionDetail, ToolCallStatus, TurnEventSink, TurnStatus,
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

    /// Adapter that emits a deliberately interleaved event stream:
    /// `text → tool → text → tool → text`. The runtime accumulator is
    /// expected to capture these in the same order as a `Vec<ContentBlock>`
    /// so the UI can render them faithfully — that's the entire bug
    /// this fix exists for.
    struct InterleavingAdapter;

    #[async_trait]
    impl ProviderAdapter for InterleavingAdapter {
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
            _input: &str,
            _permission_mode: PermissionMode,
            _reasoning_effort: Option<ReasoningEffort>,
            events: TurnEventSink,
        ) -> Result<ProviderTurnOutput, String> {
            // Two adjacent deltas to verify they coalesce.
            events
                .send(ProviderTurnEvent::AssistantTextDelta {
                    delta: "Looking it up. ".to_string(),
                })
                .await;
            events
                .send(ProviderTurnEvent::AssistantTextDelta {
                    delta: "One sec.".to_string(),
                })
                .await;
            events
                .send(ProviderTurnEvent::ToolCallStarted {
                    call_id: "call-a".to_string(),
                    name: "search".to_string(),
                    args: serde_json::json!({"q": "x"}),
                    parent_call_id: None,
                })
                .await;
            events
                .send(ProviderTurnEvent::ToolCallCompleted {
                    call_id: "call-a".to_string(),
                    output: "ok".to_string(),
                    error: None,
                })
                .await;
            events
                .send(ProviderTurnEvent::AssistantTextDelta {
                    delta: "Found it. Now editing.".to_string(),
                })
                .await;
            events
                .send(ProviderTurnEvent::ToolCallStarted {
                    call_id: "call-b".to_string(),
                    name: "edit".to_string(),
                    args: serde_json::json!({"path": "f"}),
                    parent_call_id: None,
                })
                .await;
            events
                .send(ProviderTurnEvent::ToolCallCompleted {
                    call_id: "call-b".to_string(),
                    output: "ok".to_string(),
                    error: None,
                })
                .await;
            events
                .send(ProviderTurnEvent::AssistantTextDelta {
                    delta: "Done.".to_string(),
                })
                .await;
            // Empty output → runtime falls back to accumulated text.
            Ok(ProviderTurnOutput {
                output: String::new(),
                provider_state: None,
            })
        }
    }

    /// Adapter that emits a tool-call started event, signals the test
    /// it's "mid-turn", waits for permission, then completes. Lets the
    /// test inspect `live_session_detail` while the turn is in flight.
    struct PausingAdapter {
        mid_turn_tx: tokio::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
        resume_rx: tokio::sync::Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
    }

    #[async_trait]
    impl ProviderAdapter for PausingAdapter {
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
            _input: &str,
            _permission_mode: PermissionMode,
            _reasoning_effort: Option<ReasoningEffort>,
            events: TurnEventSink,
        ) -> Result<ProviderTurnOutput, String> {
            events
                .send(ProviderTurnEvent::AssistantTextDelta {
                    delta: "starting work…".to_string(),
                })
                .await;
            events
                .send(ProviderTurnEvent::ToolCallStarted {
                    call_id: "stuck-call".to_string(),
                    name: "search".to_string(),
                    args: serde_json::json!({}),
                    parent_call_id: None,
                })
                .await;
            // Yield so the runtime drain loop has a chance to process
            // the events above and write them into in_flight_turns
            // before we open the gate.
            tokio::task::yield_now().await;
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            // Tell the test we're paused mid-turn.
            if let Some(tx) = self.mid_turn_tx.lock().await.take() {
                let _ = tx.send(());
            }
            // Wait until the test releases us.
            if let Some(rx) = self.resume_rx.lock().await.take() {
                let _ = rx.await;
            }
            events
                .send(ProviderTurnEvent::ToolCallCompleted {
                    call_id: "stuck-call".to_string(),
                    output: "ok".to_string(),
                    error: None,
                })
                .await;
            Ok(ProviderTurnOutput {
                output: String::new(),
                provider_state: None,
            })
        }
    }

    #[tokio::test]
    async fn live_session_detail_includes_in_flight_turn() {
        let (mid_tx, mid_rx) = tokio::sync::oneshot::channel();
        let (resume_tx, resume_rx) = tokio::sync::oneshot::channel();
        let adapter = Arc::new(PausingAdapter {
            mid_turn_tx: tokio::sync::Mutex::new(Some(mid_tx)),
            resume_rx: tokio::sync::Mutex::new(Some(resume_rx)),
        });
        let runtime = Arc::new(RuntimeCore::new(
            vec![adapter],
            Arc::new(OrchestrationService::new()),
            Arc::new(PersistenceService::in_memory().expect("in-memory db should initialize")),
            None,
            "/tmp/zenui-test/threads".to_string(),
        ));

        runtime
            .handle_client_message(ClientMessage::StartSession {
                provider: ProviderKind::Codex,
                title: Some("Live".to_string()),
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

        // Kick off the turn in the background — it will pause partway
        // until we send `resume_tx`.
        let runtime_clone = runtime.clone();
        let sid_clone = session_id.clone();
        let turn_task = tokio::spawn(async move {
            runtime_clone
                .handle_client_message(ClientMessage::SendTurn {
                    session_id: sid_clone,
                    input: "go".to_string(),
                    permission_mode: None,
                    reasoning_effort: None,
                })
                .await
        });

        // Wait for the adapter to signal it's mid-turn.
        mid_rx
            .await
            .expect("adapter should signal mid-turn");

        // While the turn is paused, live_session_detail must already
        // surface the in-flight turn with its still-pending tool call.
        // Persistence has nothing for this turn yet — turn_completed
        // hasn't fired — so without the in-flight tracker this would
        // return an empty turns vec.
        let live = runtime
            .live_session_detail(&session_id)
            .await
            .expect("session should exist");
        assert_eq!(live.turns.len(), 1, "in-flight turn must appear in live detail");
        let live_turn = &live.turns[0];
        assert_eq!(live_turn.status, TurnStatus::Running);
        assert_eq!(live_turn.tool_calls.len(), 1);
        assert_eq!(live_turn.tool_calls[0].call_id, "stuck-call");
        assert_eq!(
            live_turn.tool_calls[0].status,
            ToolCallStatus::Pending,
            "tool call should still be pending while the turn is mid-flight"
        );
        assert_eq!(live_turn.blocks.len(), 2, "blocks: {:?}", live_turn.blocks);

        // active_session_details should report this session too.
        let active = runtime.active_session_details().await;
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].summary.session_id, session_id);

        // Release the adapter and let the turn finish.
        let _ = resume_tx.send(());
        turn_task.await.expect("turn task should complete");

        // After completion, the in-flight entry must be cleared.
        let active_after = runtime.active_session_details().await;
        assert!(
            active_after.is_empty(),
            "in_flight_turns should be cleared on completion"
        );

        // And the persisted detail now reflects the completed tool call.
        let final_detail = runtime
            .live_session_detail(&session_id)
            .await
            .expect("session should exist");
        assert_eq!(final_detail.turns.len(), 1);
        assert_eq!(final_detail.turns[0].status, TurnStatus::Completed);
        assert_eq!(
            final_detail.turns[0].tool_calls[0].status,
            ToolCallStatus::Completed
        );
    }

    #[tokio::test]
    async fn interleaved_events_become_ordered_blocks() {
        let runtime = RuntimeCore::new(
            vec![Arc::new(InterleavingAdapter)],
            Arc::new(OrchestrationService::new()),
            Arc::new(PersistenceService::in_memory().expect("in-memory db should initialize")),
            None,
            "/tmp/zenui-test/threads".to_string(),
        );

        runtime
            .handle_client_message(ClientMessage::StartSession {
                provider: ProviderKind::Codex,
                title: Some("Interleave".to_string()),
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

        runtime
            .handle_client_message(ClientMessage::SendTurn {
                session_id: session_id.clone(),
                input: "go".to_string(),
                permission_mode: None,
                reasoning_effort: None,
            })
            .await;

        let detail = runtime
            .persistence
            .get_session(&session_id)
            .await
            .expect("session detail should exist");
        assert_eq!(detail.turns.len(), 1);
        let turn = &detail.turns[0];
        assert_eq!(turn.status, TurnStatus::Completed);

        // Five blocks: text, tool, text, tool, text — exactly the order
        // the adapter emitted them. The two leading text deltas must
        // coalesce into a single Text block.
        assert_eq!(turn.blocks.len(), 5, "blocks: {:?}", turn.blocks);
        match &turn.blocks[0] {
            ContentBlock::Text { text } => {
                assert_eq!(text, "Looking it up. One sec.");
            }
            other => panic!("expected Text block 0, got {other:?}"),
        }
        match &turn.blocks[1] {
            ContentBlock::ToolCall { call_id } => assert_eq!(call_id, "call-a"),
            other => panic!("expected ToolCall block 1, got {other:?}"),
        }
        match &turn.blocks[2] {
            ContentBlock::Text { text } => {
                assert_eq!(text, "Found it. Now editing.");
            }
            other => panic!("expected Text block 2, got {other:?}"),
        }
        match &turn.blocks[3] {
            ContentBlock::ToolCall { call_id } => assert_eq!(call_id, "call-b"),
            other => panic!("expected ToolCall block 3, got {other:?}"),
        }
        match &turn.blocks[4] {
            ContentBlock::Text { text } => assert_eq!(text, "Done."),
            other => panic!("expected Text block 4, got {other:?}"),
        }

        // Legacy fields still populated for back-compat.
        assert_eq!(turn.tool_calls.len(), 2);
        assert_eq!(
            turn.output, "Looking it up. One sec.Found it. Now editing.Done.",
            "accumulated text falls into the legacy output field"
        );
    }

    #[tokio::test]
    async fn creates_session_and_turn_snapshot() {
        let runtime = RuntimeCore::new(
            vec![Arc::new(FakeAdapter)],
            Arc::new(OrchestrationService::new()),
            Arc::new(PersistenceService::in_memory().expect("in-memory db should initialize")),
            None,
            "/tmp/zenui-test/threads".to_string(),
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
        let session_id = snapshot
            .sessions
            .first()
            .expect("session should exist")
            .summary
            .session_id
            .clone();
        let detail = runtime
            .persistence
            .get_session(&session_id)
            .await
            .expect("session detail should exist");
        assert_eq!(detail.turns.len(), 1);
        assert_eq!(detail.turns[0].output, "fake response for hello");
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
            "/tmp/zenui-test/threads".to_string(),
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

        let detail = runtime
            .persistence
            .get_session(&session_id)
            .await
            .expect("session detail should exist");
        assert_eq!(detail.turns.len(), 1);
        assert_eq!(detail.turns[0].status, TurnStatus::Completed);
        assert_eq!(detail.turns[0].output, "slow response for hello");
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
            "/tmp/zenui-test/threads".to_string(),
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
            blocks: Vec::new(),
        });
        persistence.upsert_session(session).await;

        runtime.reconcile_startup().await;

        let snapshot = runtime.snapshot().await;
        let session_id = snapshot
            .sessions
            .first()
            .expect("session should exist")
            .summary
            .session_id
            .clone();
        let detail = persistence
            .get_session(&session_id)
            .await
            .expect("session detail should exist");
        assert_eq!(
            detail.summary.status,
            zenui_provider_api::SessionStatus::Interrupted
        );
        let last_turn = detail.turns.last().expect("turn should exist");
        assert_eq!(last_turn.status, TurnStatus::Interrupted);
    }
}
