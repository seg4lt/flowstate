pub mod transport;

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};

use chrono::Utc;
use tokio::sync::{Mutex, broadcast};
use zenui_orchestration::OrchestrationService;
use zenui_persistence::PersistenceService;
use zenui_provider_api::{
    AppSnapshot, BootstrapPayload, ClientMessage, ContentBlock, FileChangeRecord, ImageAttachment,
    PermissionDecision, PermissionMode, PlanRecord, PlanStatus, ProviderAdapter, ProviderKind,
    ProviderStatus, ProviderTurnEvent, ReasoningEffort, RuntimeEvent, ServerMessage,
    SessionDetail, SessionStatus, SubagentRecord, SubagentStatus, TokenUsage, ToolCall,
    ToolCallStatus, TurnEventSink, TurnRecord, TurnStatus, UserInput, UserInputAnswer,
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
    /// Sessions whose current turn has been stopped by the user. Set by
    /// `interrupt_turn` and consumed by `send_turn`'s exit path, which then
    /// finalises the turn with `TurnStatus::Interrupted` instead of the
    /// raw adapter outcome. Drives the Interrupted / Failed distinction
    /// without string-matching provider error messages.
    interrupted_sessions: Arc<Mutex<HashSet<String>>>,
    /// Runtime enabled/disabled flag per provider. Seeded from the
    /// `provider_enablement` persistence table on boot and mutated via
    /// `ClientMessage::SetProviderEnabled`. Missing entries fall back
    /// to per-provider defaults: Claude and GitHub Copilot are enabled,
    /// CLI variants and Codex are disabled. Read by `bootstrap`,
    /// `spawn_health_check`, and `handle_send_turn` to gate downstream
    /// behavior.
    provider_enablement: Arc<RwLock<HashMap<ProviderKind, bool>>>,
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
            interrupted_sessions: Arc::new(Mutex::new(HashSet::new())),
            provider_enablement: Arc::new(RwLock::new(HashMap::new())),
            turn_observer,
        }
    }

    /// Populate `provider_enablement` from the persistence table. Called
    /// once at daemon startup from `daemon-core::bootstrap_core` after
    /// `RuntimeCore::new`. Providers without a row in the table default
    /// to enabled on the read side, so this is idempotent and safe to
    /// call multiple times.
    pub async fn seed_provider_enablement(&self) {
        let map = self.persistence.get_provider_enablement().await;
        if let Ok(mut lock) = self.provider_enablement.write() {
            *lock = map;
        }
    }

    /// Read-side helper. When the provider has a row in the
    /// `provider_enablement` table, returns that persisted value.
    /// Otherwise falls back to a per-provider default: Claude and
    /// GitHub Copilot are enabled out of the box; CLI variants and
    /// Codex default to disabled until the user turns them on in
    /// Settings.
    pub fn is_provider_enabled(&self, kind: ProviderKind) -> bool {
        self.provider_enablement
            .read()
            .ok()
            .and_then(|lock| lock.get(&kind).copied())
            .unwrap_or_else(|| {
                matches!(kind, ProviderKind::Claude | ProviderKind::GitHubCopilot)
            })
    }

    /// Mutation side of the provider-enablement toggle. Writes through
    /// to persistence, updates the in-memory lock, and broadcasts a
    /// fresh `ProviderHealthUpdated` event so every connected client
    /// re-renders without needing a full reload.
    ///
    /// The broadcast uses the cached health status as a base and
    /// overwrites just the `enabled` field — we don't re-run the
    /// expensive health probe on a toggle. If there's no cached status
    /// yet (first-boot edge case), we synthesise a minimal one.
    async fn set_provider_enabled(&self, kind: ProviderKind, enabled: bool) {
        self.persistence.set_provider_enabled(kind, enabled).await;
        if let Ok(mut lock) = self.provider_enablement.write() {
            lock.insert(kind, enabled);
        }

        let status = match self.persistence.get_cached_health(kind).await {
            Some((_, mut status)) => {
                status.enabled = enabled;
                status
            }
            None => ProviderStatus {
                kind,
                label: kind.label().to_string(),
                installed: false,
                authenticated: false,
                version: None,
                status: zenui_provider_api::ProviderStatusLevel::Warning,
                message: Some("No health check yet".to_string()),
                models: Vec::new(),
                enabled,
            },
        };
        self.publish(RuntimeEvent::ProviderHealthUpdated { status });
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
        self.live_session_detail_limited(session_id, None).await
    }

    /// Paginated version of [`live_session_detail`]. When `limit` is
    /// `Some(n)`, only the most recent `n` persisted turns are returned —
    /// the in-flight turn (if any) is still spliced in regardless, so a
    /// currently-running turn always appears in the response even if it
    /// would otherwise have fallen outside the page. Callers can tell
    /// whether more older turns exist by comparing `detail.turns.len()`
    /// to `detail.summary.turn_count`.
    pub async fn live_session_detail_limited(
        &self,
        session_id: &str,
        limit: Option<usize>,
    ) -> Option<SessionDetail> {
        let mut detail = self
            .persistence
            .get_session_limited(session_id, limit)
            .await?;
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
        //
        // The runtime `enabled` flag is stamped onto each ProviderStatus
        // here — adapters always emit `true`, persistence caches whatever
        // they emitted, but the authoritative enablement state lives in
        // `self.provider_enablement` so we overwrite on read. Same rule
        // applies to `spawn_health_check`.
        let mut cached_providers: Vec<ProviderStatus> = Vec::new();
        for &kind in self.adapters.keys() {
            // provider_model_cache is the authoritative source for the
            // model list; the `models` vec embedded inside a health
            // cache entry is only a stale snapshot from whenever the
            // last health probe ran. Consult it up front so the same
            // value can both merge into the bootstrap payload and
            // drive the stale-model refresh below.
            let cached_models = self.persistence.get_cached_models(kind).await;

            match self.persistence.get_cached_health(kind).await {
                Some((checked_at, mut status)) => {
                    tracing::info!(
                        ?kind,
                        ?checked_at,
                        "loaded cached provider health"
                    );
                    status.enabled = self.is_provider_enabled(kind);
                    if let Some((_, ref models)) = cached_models {
                        status.models = models.clone();
                    }
                    cached_providers.push(status);
                    if is_cache_stale(&checked_at) {
                        self.spawn_health_check(kind);
                    }
                }
                None => {
                    self.spawn_health_check(kind);
                }
            }

            // Refresh models independently of health-cache freshness so
            // a fresh health cache can't pin the frontend to old models
            // past the 24h model TTL.
            let needs_model_refresh = match &cached_models {
                Some((fetched_at, _)) => is_cache_stale(fetched_at),
                None => true,
            };
            if needs_model_refresh && self.is_provider_enabled(kind) {
                self.spawn_model_refresh(kind);
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
        // Skip the probe entirely when the provider is toggled off —
        // the frontend greys it out based on the `enabled` flag
        // regardless of health status, and the probe itself can be
        // expensive (spawns a bridge, extracts a node runtime, etc.).
        if !self.is_provider_enabled(kind) {
            tracing::debug!(?kind, "provider disabled; skipping health check");
            return;
        }
        let Some(adapter) = self.adapters.get(&kind).cloned() else {
            return;
        };
        let persistence = self.persistence.clone();
        let event_tx = self.event_tx.clone();
        let in_flight = self.in_flight_health_checks.clone();
        let in_flight_models = self.in_flight_model_fetches.clone();
        let enablement = self.provider_enablement.clone();

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

            // Stamp the runtime enablement flag onto the fresh status
            // before persisting / broadcasting. Adapters always emit
            // `true`, so without this overwrite a disabled provider
            // would reappear as enabled after its next health check.
            status.enabled = enablement
                .read()
                .ok()
                .and_then(|lock| lock.get(&kind).copied())
                .unwrap_or(true);

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
            ClientMessage::LoadSession { session_id, limit } => {
                match self.live_session_detail_limited(&session_id, limit).await {
                    Some(session) => Some(ServerMessage::SessionLoaded { session }),
                    None => Some(ServerMessage::Error {
                        message: format!("Session `{session_id}` not found."),
                    }),
                }
            }
            ClientMessage::StartSession {
                provider,
                model,
                project_id,
            } => {
                tracing::info!(?provider, ?model, ?project_id, "Starting session");
                match self.start_session(provider, model, project_id).await {
                    Ok(session) => Some(ServerMessage::SessionCreated {
                        session: session.summary,
                    }),
                    Err(error) => Some(ServerMessage::Error { message: error }),
                }
            }
            ClientMessage::SendTurn {
                session_id,
                input,
                images,
                permission_mode,
                reasoning_effort,
            } => {
                let mode = permission_mode.unwrap_or_default();
                match self
                    .send_turn(session_id, input, images, mode, reasoning_effort)
                    .await
                {
                    Ok(message) => Some(ServerMessage::Ack { message }),
                    Err(error) => Some(ServerMessage::Error { message: error }),
                }
            }
            ClientMessage::GetAttachment { attachment_id } => {
                match self.persistence.read_attachment(&attachment_id).await {
                    Ok(data) => Some(ServerMessage::Attachment { data }),
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
            ClientMessage::SetProviderEnabled { provider, enabled } => {
                self.set_provider_enabled(provider, enabled).await;
                Some(ServerMessage::Ack {
                    message: format!(
                        "{} {}.",
                        provider.label(),
                        if enabled { "enabled" } else { "disabled" }
                    ),
                })
            }
            ClientMessage::CreateProject { path } => {
                match self.persistence.create_project(path).await {
                    Some(project) => {
                        let project_id = project.project_id.clone();
                        self.publish(RuntimeEvent::ProjectCreated {
                            project: project.clone(),
                        });
                        Some(ServerMessage::Ack {
                            message: format!("Project `{project_id}` created."),
                        })
                    }
                    None => Some(ServerMessage::Error {
                        message: "Project creation failed.".to_string(),
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
            ClientMessage::UpdateSessionModel { session_id, model } => {
                if let Some(mut session) = self.persistence.get_session(&session_id).await {
                    session.summary.model = Some(model.clone());
                    self.persistence.upsert_session(session.clone()).await;
                    // Forward the model change to the provider adapter so
                    // any cached bridge process picks up the new model
                    // before the next turn. Errors are logged but not
                    // surfaced — the model is already persisted, so the
                    // worst case is the bridge picks it up on next restart.
                    if let Some(adapter) = self.adapters.get(&session.summary.provider) {
                        if let Err(e) = adapter.update_session_model(&session, model.clone()).await {
                            tracing::warn!(
                                "Failed to forward model change to adapter: {e}"
                            );
                        }
                    }
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
        self.send_turn(
            session_id,
            follow_up,
            Vec::new(),
            PermissionMode::AcceptEdits,
            None,
        )
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
            .create_session(provider, model, project_id);
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
        images: Vec<ImageAttachment>,
        permission_mode: PermissionMode,
        reasoning_effort: Option<ReasoningEffort>,
    ) -> Result<String, String> {
        let trimmed = input.trim().to_string();
        if trimmed.is_empty() && images.is_empty() {
            return Err("Turn input cannot be empty.".to_string());
        }

        let mut session = self
            .persistence
            .get_session(&session_id)
            .await
            .ok_or_else(|| format!("Unknown session `{session_id}`."))?;
        // Drop any stale interrupt flag for this session before the new
        // turn begins. A prior turn that errored out via `?` before the
        // drain loop could leave a set flag behind; without this clear,
        // the next turn's exit path would wrongly finalise as Interrupted.
        self.interrupted_sessions.lock().await.remove(&session_id);
        // Runtime enablement gate. Reject before we touch orchestration
        // so a disabled provider can't start a turn mid-stream. Previous
        // turns on this session stay visible (read-only) — that's the
        // "badge + history preserved" contract from Phase 5.
        if !self.is_provider_enabled(session.summary.provider) {
            return Err(format!(
                "{} is disabled. Re-enable it in Settings to send new messages.",
                session.summary.provider.label()
            ));
        }
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

        let mut turn = self.orchestration.start_turn(
            &mut session,
            trimmed.clone(),
            Some(permission_mode),
            reasoning_effort,
        );

        // Persist any pasted images to disk before the adapter runs.
        // The bytes are written under <data_dir>/attachments/<uuid>.<ext>
        // and a row goes into the turn_attachments table, so on replay
        // the frontend renders a chip pointing at the persisted file.
        // The raw `images` Vec is still passed to the adapter below
        // (we don't re-read from disk for the live send) so multimodal
        // providers see the bytes immediately.
        let mut persisted_attachments = Vec::with_capacity(images.len());
        for img in &images {
            match self
                .persistence
                .write_attachment(
                    &session.summary.session_id,
                    &turn.turn_id,
                    &img.media_type,
                    img.name.as_deref(),
                    &img.data_base64,
                )
                .await
            {
                Ok(att) => persisted_attachments.push(att),
                Err(e) => {
                    tracing::warn!(
                        session_id = %session.summary.session_id,
                        turn_id = %turn.turn_id,
                        error = %e,
                        "failed to persist pasted image; turn will run without it on disk"
                    );
                }
            }
        }
        // Stamp the persisted refs onto both the local turn copy and
        // the session.turns entry orchestration just appended, so
        // every downstream consumer (TurnStarted event, in-flight
        // snapshot, final upsert) sees them.
        turn.input_attachments = persisted_attachments.clone();
        if let Some(stored) = session
            .turns
            .iter_mut()
            .find(|t| t.turn_id == turn.turn_id)
        {
            stored.input_attachments = persisted_attachments;
        }

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
        let user_input = UserInput {
            text: trimmed.clone(),
            images,
        };
        let adapter_sink = sink.clone();
        let active_sinks_for_cleanup = self.active_sinks.clone();
        let sid_for_cleanup = session.summary.session_id.clone();
        let adapter_fut = tokio::spawn(async move {
            let result = adapter_clone
                .execute_turn(
                    &session_clone,
                    &user_input,
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
        let mut usage: Option<TokenUsage> = None;
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
            &usage,
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
                ProviderTurnEvent::TurnUsage { usage: u } => {
                    usage = Some(u);
                }
                ProviderTurnEvent::RateLimitUpdated { info } => {
                    // Rate limits are account-wide, not per-turn, so
                    // we just forward them to the runtime event
                    // stream without touching any turn-local state.
                    self.publish(RuntimeEvent::RateLimitUpdated { info });
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
                &usage,
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

        // Consume the interrupt flag for this session. If set, the user
        // clicked stop while the adapter was streaming — finalise the turn
        // as Interrupted regardless of whether the adapter surfaced the
        // interrupt as an Ok with a stub string (claude-sdk) or an Err
        // from a closed stdout (claude-cli / codex).
        let was_interrupted = self.interrupted_sessions.lock().await.remove(&sid);

        let (status, canonical, result) = match (was_interrupted, adapter_result) {
            (true, Ok(output)) => {
                if output.provider_state.is_some() {
                    session.provider_state = output.provider_state.clone();
                }
                // Prefer the streamed accumulated text — it's what the user
                // actually saw on screen. Fall back to the adapter's stub
                // ("[interrupted]") only when nothing was streamed.
                let canonical = if !accumulated.trim().is_empty() {
                    accumulated.clone()
                } else if !output.output.trim().is_empty() {
                    output.output
                } else {
                    "[interrupted]".to_string()
                };
                (
                    TurnStatus::Interrupted,
                    canonical,
                    Ok("Turn interrupted.".to_string()),
                )
            }
            (true, Err(_)) => {
                let canonical = if accumulated.trim().is_empty() {
                    "[interrupted]".to_string()
                } else {
                    accumulated.clone()
                };
                (
                    TurnStatus::Interrupted,
                    canonical,
                    Ok("Turn interrupted.".to_string()),
                )
            }
            (false, Ok(output)) => {
                if output.provider_state.is_some() {
                    session.provider_state = output.provider_state.clone();
                }
                let canonical = if !output.output.trim().is_empty() {
                    output.output
                } else {
                    accumulated.clone()
                };
                (
                    TurnStatus::Completed,
                    canonical,
                    Ok("Turn completed.".to_string()),
                )
            }
            (false, Err(error)) => (TurnStatus::Failed, error.clone(), Err(error)),
        };

        let finished = self
            .orchestration
            .finish_turn(&mut session, &turn.turn_id, canonical, status)
            .ok_or_else(|| format!("Unknown turn `{}`.", turn.turn_id))?;

        // Merge the streamed locals into the running turn on every exit
        // path. Without this, Err and Interrupted turns broadcast with
        // empty `blocks`, and the frontend's `applyEventToTurns` replaces
        // the cached turn on `turn_completed` — wiping out the content the
        // user was already watching stream in.
        let merged_turn = if let Some(t) = session
            .turns
            .iter_mut()
            .find(|t| t.turn_id == finished.turn_id)
        {
            if !reasoning.is_empty() {
                t.reasoning = Some(reasoning);
            }
            t.tool_calls = tool_calls;
            t.file_changes = file_changes;
            t.subagents = subagents;
            t.plan = plan;
            t.blocks = blocks;
            t.usage = usage;
            t.clone()
        } else {
            finished
        };

        self.persistence.upsert_session(session.clone()).await;
        if status == TurnStatus::Failed {
            self.publish(RuntimeEvent::Error {
                message: merged_turn.output.clone(),
            });
        }
        self.publish(RuntimeEvent::TurnCompleted {
            session_id: session.summary.session_id.clone(),
            session: session.summary.clone(),
            turn: merged_turn,
        });

        result
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
        // Prefer the in-flight snapshot over stale persistence so we never
        // hand the adapter a view of the session that's missing the turn
        // it's supposed to abort. Flag the session as interrupted so
        // `send_turn`'s exit path finalises with `TurnStatus::Interrupted`
        // and the streamed blocks are preserved — `interrupt_turn` itself
        // does not persist or publish, to avoid racing `send_turn`.
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

        self.interrupted_sessions
            .lock()
            .await
            .insert(session_id.clone());
        adapter.interrupt_turn(&session).await
    }

    async fn delete_session(&self, session_id: String) -> Result<String, String> {
        if let Some(session) = self.persistence.get_session(&session_id).await {
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
        } else if !self.persistence.delete_archived_session(&session_id) {
            return Err(format!("Unknown session `{session_id}`."));
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
                // Keep provider_health_cache.status_json in sync with
                // the fresh model list. The bootstrap path now prefers
                // provider_model_cache, but any other reader that
                // touches only the health cache (or a future one) must
                // not observe a stale list.
                if let Some((_, mut status)) = persistence.get_cached_health(kind).await {
                    status.models = models.clone();
                    persistence.set_cached_health(kind, &status).await;
                }
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
    usage: &Option<TokenUsage>,
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
    snap.usage = usage.clone();
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
        ReasoningEffort, RuntimeEvent, SessionDetail, ToolCallStatus, TurnEventSink, TurnStatus,
        UserInput,
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
                enabled: true,
            }
        }

        async fn execute_turn(
            &self,
            _session: &SessionDetail,
            input: &UserInput,
            _permission_mode: PermissionMode,
            _reasoning_effort: Option<ReasoningEffort>,
            _events: TurnEventSink,
        ) -> Result<ProviderTurnOutput, String> {
            Ok(ProviderTurnOutput {
                output: format!("fake response for {}", input.text),
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
                enabled: true,
            }
        }

        async fn execute_turn(
            &self,
            _session: &SessionDetail,
            _input: &UserInput,
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
                enabled: true,
            }
        }

        async fn execute_turn(
            &self,
            _session: &SessionDetail,
            _input: &UserInput,
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
                    images: Vec::new(),
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
                images: Vec::new(),
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
        assert_eq!(session.summary.provider, ProviderKind::Codex);

        let response = runtime
            .handle_client_message(ClientMessage::SendTurn {
                session_id: session.summary.session_id.clone(),
                input: "hello".to_string(),
                images: Vec::new(),
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
                enabled: true,
            }
        }

        async fn execute_turn(
            &self,
            _session: &SessionDetail,
            input: &UserInput,
            _permission_mode: PermissionMode,
            _reasoning_effort: Option<ReasoningEffort>,
            _events: TurnEventSink,
        ) -> Result<ProviderTurnOutput, String> {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            Ok(ProviderTurnOutput {
                output: format!("slow response for {}", input.text),
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
                images: Vec::new(),
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

    /// Adapter that streams a text delta + tool call, waits for the test
    /// to trigger a mid-turn interrupt, then returns `outcome` (Ok or Err)
    /// once released. Lets us exercise both exit paths of `send_turn` with
    /// the interrupt flag already set.
    struct InterruptingAdapter {
        mid_turn_tx: tokio::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
        resume_rx: tokio::sync::Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
        outcome: InterruptOutcome,
    }

    #[derive(Clone, Copy)]
    enum InterruptOutcome {
        OkStub,
        Err,
    }

    #[async_trait]
    impl ProviderAdapter for InterruptingAdapter {
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
                enabled: true,
            }
        }

        async fn execute_turn(
            &self,
            _session: &SessionDetail,
            _input: &UserInput,
            _permission_mode: PermissionMode,
            _reasoning_effort: Option<ReasoningEffort>,
            events: TurnEventSink,
        ) -> Result<ProviderTurnOutput, String> {
            events
                .send(ProviderTurnEvent::AssistantTextDelta {
                    delta: "partial answer".to_string(),
                })
                .await;
            events
                .send(ProviderTurnEvent::ToolCallStarted {
                    call_id: "tool-1".to_string(),
                    name: "search".to_string(),
                    args: serde_json::json!({"q": "z"}),
                    parent_call_id: None,
                })
                .await;
            events
                .send(ProviderTurnEvent::ToolCallCompleted {
                    call_id: "tool-1".to_string(),
                    output: "ok".to_string(),
                    error: None,
                })
                .await;

            // Give the runtime drain loop time to process the events and
            // write them into in_flight_turns before we gate the test.
            tokio::task::yield_now().await;
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;

            if let Some(tx) = self.mid_turn_tx.lock().await.take() {
                let _ = tx.send(());
            }
            if let Some(rx) = self.resume_rx.lock().await.take() {
                let _ = rx.await;
            }

            match self.outcome {
                InterruptOutcome::OkStub => Ok(ProviderTurnOutput {
                    output: "[interrupted]".to_string(),
                    provider_state: None,
                }),
                InterruptOutcome::Err => {
                    Err("adapter stdout closed during interrupt".to_string())
                }
            }
        }
    }

    /// Adapter that streams one delta then returns `Err`. Used to verify
    /// that a genuine provider failure still merges the streamed blocks
    /// into the failed turn (no content wiped on crash), and still
    /// publishes a `RuntimeEvent::Error`.
    struct FailingAdapter;

    #[async_trait]
    impl ProviderAdapter for FailingAdapter {
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
                enabled: true,
            }
        }

        async fn execute_turn(
            &self,
            _session: &SessionDetail,
            _input: &UserInput,
            _permission_mode: PermissionMode,
            _reasoning_effort: Option<ReasoningEffort>,
            events: TurnEventSink,
        ) -> Result<ProviderTurnOutput, String> {
            events
                .send(ProviderTurnEvent::AssistantTextDelta {
                    delta: "half a thought".to_string(),
                })
                .await;
            // Flush the delta through the drain loop before we explode
            // so the test can observe the merged blocks.
            tokio::task::yield_now().await;
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            Err("boom".to_string())
        }
    }

    async fn run_interrupt_scenario(outcome: InterruptOutcome) -> zenui_provider_api::TurnRecord {
        let (mid_tx, mid_rx) = tokio::sync::oneshot::channel();
        let (resume_tx, resume_rx) = tokio::sync::oneshot::channel();
        let adapter = Arc::new(InterruptingAdapter {
            mid_turn_tx: tokio::sync::Mutex::new(Some(mid_tx)),
            resume_rx: tokio::sync::Mutex::new(Some(resume_rx)),
            outcome,
        });
        let runtime = Arc::new(RuntimeCore::new(
            vec![adapter],
            Arc::new(OrchestrationService::new()),
            Arc::new(PersistenceService::in_memory().expect("in-memory db should initialize")),
            None,
            "/tmp/zenui-test/threads".to_string(),
        ));

        let mut events = runtime.subscribe();

        runtime
            .handle_client_message(ClientMessage::StartSession {
                provider: ProviderKind::Codex,
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

        let runtime_clone = runtime.clone();
        let sid_clone = session_id.clone();
        let turn_task = tokio::spawn(async move {
            runtime_clone
                .handle_client_message(ClientMessage::SendTurn {
                    session_id: sid_clone,
                    input: "go".to_string(),
                    images: Vec::new(),
                    permission_mode: None,
                    reasoning_effort: None,
                })
                .await
        });

        mid_rx.await.expect("adapter should signal mid-turn");

        // Simulate the user clicking stop: interrupt_turn flips the flag
        // and tells the adapter to abort.
        runtime
            .handle_client_message(ClientMessage::InterruptTurn {
                session_id: session_id.clone(),
            })
            .await;

        // Release the adapter so it returns (Ok or Err per `outcome`).
        let _ = resume_tx.send(());
        turn_task.await.expect("turn task should complete");

        // Drain broadcast events looking for the TurnCompleted payload.
        // Assert no `RuntimeEvent::Error` fires on the interrupt path —
        // an interrupted turn is a user action, not a failure.
        let mut turn_completed_turn = None;
        while let Ok(evt) = events.try_recv() {
            match evt {
                RuntimeEvent::TurnCompleted { turn, .. } => {
                    turn_completed_turn = Some(turn);
                }
                RuntimeEvent::Error { message } => {
                    panic!("interrupt path must not publish Error, got: {message}");
                }
                _ => {}
            }
        }
        turn_completed_turn.expect("TurnCompleted must be published on interrupt")
    }

    /// User stops the turn; adapter surfaces it as `Err(...)` (the
    /// claude-cli / codex path). The final turn must keep every streamed
    /// block, land in `Interrupted` status, and NOT publish `Error`.
    #[tokio::test]
    async fn interrupt_preserves_blocks_on_err_path() {
        let turn = run_interrupt_scenario(InterruptOutcome::Err).await;
        assert_eq!(turn.status, TurnStatus::Interrupted);
        assert_eq!(turn.blocks.len(), 2, "blocks: {:?}", turn.blocks);
        match &turn.blocks[0] {
            ContentBlock::Text { text } => assert_eq!(text, "partial answer"),
            other => panic!("expected Text block 0, got {other:?}"),
        }
        match &turn.blocks[1] {
            ContentBlock::ToolCall { call_id } => assert_eq!(call_id, "tool-1"),
            other => panic!("expected ToolCall block 1, got {other:?}"),
        }
        assert_eq!(turn.tool_calls.len(), 1);
        assert_eq!(turn.tool_calls[0].status, ToolCallStatus::Completed);
        assert_eq!(turn.output, "partial answer");
    }

    /// Same scenario, but the adapter surfaces the interrupt as
    /// `Ok("[interrupted]")` (the claude-sdk path). The streamed text
    /// must still be preserved as `output` and `blocks`.
    #[tokio::test]
    async fn interrupt_preserves_blocks_on_ok_path() {
        let turn = run_interrupt_scenario(InterruptOutcome::OkStub).await;
        assert_eq!(turn.status, TurnStatus::Interrupted);
        assert_eq!(turn.blocks.len(), 2, "blocks: {:?}", turn.blocks);
        assert_eq!(
            turn.output, "partial answer",
            "accumulated text must win over the adapter's [interrupted] stub"
        );
    }

    /// A genuine adapter failure (no interrupt) still merges the streamed
    /// blocks into the failed turn and still publishes `Error`, so the
    /// UI can surface "Turn failed" without losing partial content.
    #[tokio::test]
    async fn failed_turn_still_merges_blocks() {
        let runtime = Arc::new(RuntimeCore::new(
            vec![Arc::new(FailingAdapter)],
            Arc::new(OrchestrationService::new()),
            Arc::new(PersistenceService::in_memory().expect("in-memory db should initialize")),
            None,
            "/tmp/zenui-test/threads".to_string(),
        ));
        let mut events = runtime.subscribe();

        runtime
            .handle_client_message(ClientMessage::StartSession {
                provider: ProviderKind::Codex,
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
                images: Vec::new(),
                permission_mode: None,
                reasoning_effort: None,
            })
            .await;

        let detail = runtime
            .persistence
            .get_session(&session_id)
            .await
            .expect("session detail should exist");
        let turn = detail.turns.last().expect("turn should exist");
        assert_eq!(turn.status, TurnStatus::Failed);
        assert_eq!(turn.output, "boom", "failed turn carries the error string");
        assert_eq!(turn.blocks.len(), 1, "streamed block must survive");
        match &turn.blocks[0] {
            ContentBlock::Text { text } => assert_eq!(text, "half a thought"),
            other => panic!("expected Text block, got {other:?}"),
        }

        let mut saw_error = false;
        let mut saw_completed = false;
        while let Ok(evt) = events.try_recv() {
            match evt {
                RuntimeEvent::Error { .. } => saw_error = true,
                RuntimeEvent::TurnCompleted { .. } => saw_completed = true,
                _ => {}
            }
        }
        assert!(saw_error, "failed path must publish Error");
        assert!(saw_completed, "failed path must publish TurnCompleted");
    }

    /// `InterruptTurn` on a session with no streaming turn just flips
    /// the flag. The next `send_turn` should clear the stale flag on
    /// entry and finalise normally, not as Interrupted.
    #[tokio::test]
    async fn stale_interrupt_flag_is_cleared_on_next_turn() {
        let runtime = Arc::new(RuntimeCore::new(
            vec![Arc::new(FakeAdapter)],
            Arc::new(OrchestrationService::new()),
            Arc::new(PersistenceService::in_memory().expect("in-memory db should initialize")),
            None,
            "/tmp/zenui-test/threads".to_string(),
        ));

        runtime
            .handle_client_message(ClientMessage::StartSession {
                provider: ProviderKind::Codex,
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

        // Plant a stale flag as if a prior turn's interrupt never got
        // consumed. The next `send_turn` must drop it on entry.
        runtime
            .interrupted_sessions
            .lock()
            .await
            .insert(session_id.clone());

        runtime
            .handle_client_message(ClientMessage::SendTurn {
                session_id: session_id.clone(),
                input: "hello".to_string(),
                images: Vec::new(),
                permission_mode: None,
                reasoning_effort: None,
            })
            .await;

        let detail = runtime
            .persistence
            .get_session(&session_id)
            .await
            .expect("session detail should exist");
        let turn = detail.turns.last().expect("turn should exist");
        assert_eq!(
            turn.status,
            TurnStatus::Completed,
            "stale flag must not flip a normal turn to Interrupted"
        );
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
            input_attachments: Vec::new(),
            usage: None,
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

    /// Build a `ProviderStatus` suitable for seeding the health cache in
    /// the bootstrap tests. The embedded `models` field is what ships in
    /// `status_json`; the tests assert whether it or `provider_model_cache`
    /// wins during bootstrap.
    fn fake_status_with_models(
        models: Vec<zenui_provider_api::ProviderModel>,
    ) -> ProviderStatus {
        ProviderStatus {
            kind: ProviderKind::Codex,
            label: "Codex".to_string(),
            installed: true,
            authenticated: true,
            version: Some("test".to_string()),
            status: ProviderStatusLevel::Ready,
            message: None,
            models,
            enabled: true,
        }
    }

    fn model(value: &str, label: &str) -> zenui_provider_api::ProviderModel {
        zenui_provider_api::ProviderModel {
            value: value.to_string(),
            label: label.to_string(),
        }
    }

    /// Refreshed models must survive an app restart. Seeds an old model
    /// list in the health cache and a fresh list in the dedicated model
    /// cache, then spins up a new RuntimeCore reusing the same
    /// persistence Arc (simulating a restart) and asserts the fresh list
    /// wins the bootstrap merge.
    #[tokio::test]
    async fn bootstrap_prefers_provider_model_cache_over_health_cache_models() {
        let persistence =
            Arc::new(PersistenceService::in_memory().expect("in-memory db should initialize"));

        // The "last health check" stored [old-model] inside status_json.
        persistence
            .set_cached_health(
                ProviderKind::Codex,
                &fake_status_with_models(vec![model("old-model", "Old")]),
            )
            .await;

        // The user then clicked "Refresh models" which wrote to
        // provider_model_cache (with a fresh, non-stale timestamp).
        persistence
            .set_cached_models(
                ProviderKind::Codex,
                &[model("fresh-model", "Fresh")],
            )
            .await;

        // Simulate an app restart: brand-new RuntimeCore against the
        // same persistence. FakeAdapter::health() returns empty models,
        // so any non-empty models vec in the payload must come from
        // the cache merge.
        let runtime = Arc::new(RuntimeCore::new(
            vec![Arc::new(FakeAdapter)],
            Arc::new(OrchestrationService::new()),
            persistence.clone(),
            None,
            "/tmp/zenui-test/threads".to_string(),
        ));

        let payload = runtime.bootstrap("ws://test".to_string()).await;
        assert_eq!(payload.providers.len(), 1);
        let models = &payload.providers[0].models;
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].value, "fresh-model");
    }

    /// When provider_model_cache has no row (e.g. upgrading from an
    /// older build that only populated the health cache), bootstrap
    /// must fall back to the models embedded in the health cache
    /// instead of clearing them.
    #[tokio::test]
    async fn bootstrap_falls_back_to_health_cache_models_when_model_cache_missing() {
        let persistence =
            Arc::new(PersistenceService::in_memory().expect("in-memory db should initialize"));

        persistence
            .set_cached_health(
                ProviderKind::Codex,
                &fake_status_with_models(vec![model("legacy-model", "Legacy")]),
            )
            .await;

        let runtime = Arc::new(RuntimeCore::new(
            vec![Arc::new(FakeAdapter)],
            Arc::new(OrchestrationService::new()),
            persistence.clone(),
            None,
            "/tmp/zenui-test/threads".to_string(),
        ));

        let payload = runtime.bootstrap("ws://test".to_string()).await;
        assert_eq!(payload.providers.len(), 1);
        let models = &payload.providers[0].models;
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].value, "legacy-model");
    }

    /// Adapter whose `fetch_models` returns a known non-empty list and
    /// bumps an atomic counter. Used by the stale-model-cache test to
    /// observe whether bootstrap fired an independent model refresh.
    struct ModelFetchCountingAdapter {
        fetch_count: std::sync::atomic::AtomicUsize,
    }

    #[async_trait]
    impl ProviderAdapter for ModelFetchCountingAdapter {
        fn kind(&self) -> ProviderKind {
            ProviderKind::Codex
        }

        async fn health(&self) -> ProviderStatus {
            fake_status_with_models(vec![])
        }

        async fn fetch_models(&self) -> Result<Vec<zenui_provider_api::ProviderModel>, String> {
            self.fetch_count
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(vec![model("refreshed-model", "Refreshed")])
        }

        async fn execute_turn(
            &self,
            _session: &SessionDetail,
            input: &UserInput,
            _permission_mode: PermissionMode,
            _reasoning_effort: Option<ReasoningEffort>,
            _events: TurnEventSink,
        ) -> Result<zenui_provider_api::ProviderTurnOutput, String> {
            Ok(zenui_provider_api::ProviderTurnOutput {
                output: format!("fake response for {}", input.text),
                provider_state: None,
            })
        }
    }

    /// A stale provider_model_cache must trigger an independent model
    /// refresh even when the health cache is fresh. Otherwise a fresh
    /// health cache could pin the frontend to old models past the 24h
    /// model TTL. The refresh is async — we assert it fires by waiting
    /// for the adapter's `fetch_models` counter to increment AND for a
    /// `ProviderModelsUpdated` event to arrive on the runtime bus.
    #[tokio::test]
    async fn bootstrap_triggers_model_refresh_when_model_cache_is_stale() {
        let persistence =
            Arc::new(PersistenceService::in_memory().expect("in-memory db should initialize"));

        // Fresh health cache (checked_at = now via set_cached_health).
        persistence
            .set_cached_health(
                ProviderKind::Codex,
                &fake_status_with_models(vec![model("old-model", "Old")]),
            )
            .await;

        // Stale model cache: fetched_at 25h in the past.
        let stale = (chrono::Utc::now() - chrono::Duration::hours(25)).to_rfc3339();
        persistence
            .set_cached_models_at(
                ProviderKind::Codex,
                &[model("old-model", "Old")],
                &stale,
            )
            .await;

        let adapter = Arc::new(ModelFetchCountingAdapter {
            fetch_count: std::sync::atomic::AtomicUsize::new(0),
        });
        let runtime = Arc::new(RuntimeCore::new(
            vec![adapter.clone()],
            Arc::new(OrchestrationService::new()),
            persistence.clone(),
            None,
            "/tmp/zenui-test/threads".to_string(),
        ));

        // Subscribe before bootstrapping so we don't miss the event.
        let mut events = runtime.subscribe();
        let payload = runtime.bootstrap("ws://test".to_string()).await;

        // Bootstrap merges the stale (but still present) model cache
        // into the payload, so the frontend still gets SOMETHING to
        // render immediately — the refresh comes in after.
        assert_eq!(payload.providers.len(), 1);
        assert_eq!(payload.providers[0].models.len(), 1);
        assert_eq!(payload.providers[0].models[0].value, "old-model");

        // Poll for the background refresh: fetch counter must bump
        // and ProviderModelsUpdated must fire with the fresh list.
        let mut saw_updated = false;
        for _ in 0..100 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            while let Ok(event) = events.try_recv() {
                if let RuntimeEvent::ProviderModelsUpdated { provider, models } = event {
                    if provider == ProviderKind::Codex
                        && models.len() == 1
                        && models[0].value == "refreshed-model"
                    {
                        saw_updated = true;
                    }
                }
            }
            if saw_updated {
                break;
            }
        }
        assert!(
            saw_updated,
            "expected ProviderModelsUpdated event with refreshed-model"
        );
        assert!(
            adapter
                .fetch_count
                .load(std::sync::atomic::Ordering::SeqCst)
                >= 1,
            "expected fetch_models to be called at least once"
        );

        // And the model cache on disk should now have the fresh list
        // with a current timestamp — the next restart won't trigger
        // yet another refresh.
        let (_, cached) = persistence
            .get_cached_models(ProviderKind::Codex)
            .await
            .expect("refreshed model cache row");
        assert_eq!(cached.len(), 1);
        assert_eq!(cached[0].value, "refreshed-model");
    }
}
