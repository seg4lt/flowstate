mod internals;
pub mod orchestration;
pub mod session_ops;
pub mod transport;

pub use orchestration::OrchestrationState;
pub use session_ops::OrchestrationService;

use internals::{
    InFlightPermissionModeGuard, TurnCounterGuard, is_cache_stale,
    spawn_model_refresh_detached, write_in_flight_snapshot,
};

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};

use chrono::Utc;
use tokio::sync::{Mutex, Notify, broadcast};
use zenui_persistence::PersistenceService;
use zenui_provider_api::{
    AppSnapshot, BootstrapPayload, ClientMessage, CommandCatalog, ContentBlock, FileChangeRecord,
    ImageAttachment, PermissionDecision, PermissionMode, PlanRecord, PlanStatus, PollOutcome,
    ProviderAdapter, ProviderKind, ProviderSessionState, ProviderStatus, ProviderTurnEvent,
    ReasoningEffort, RewindConflictWire, RewindOutcomeWire, RewindUnavailableReason, RuntimeCall,
    RuntimeCallError, RuntimeCallOrigin, RuntimeCallResult, RuntimeEvent, ServerMessage,
    SessionDetail, SessionLinkReason, SessionStatus, SubagentRecord, SubagentStatus, TokenUsage,
    ToolCall, ToolCallStatus, TurnEventSink, TurnRecord, TurnStatus, UserInput, UserInputAnswer,
};

use crate::orchestration::{
    PendingReply, clamp_timeout, poll_result_from_turn, resolve_pending_reply,
};

const MODEL_CACHE_TTL_HOURS: i64 = 24;

/// Observer for turn lifecycle events. Exists so daemon-core can plug in a
/// `DaemonLifecycle` counter for idle shutdown without runtime-core depending
/// on any daemon crate. Default path (no observer) is a no-op.
pub trait TurnLifecycleObserver: Send + Sync {
    fn on_turn_start(&self, session_id: &str);
    fn on_turn_end(&self, session_id: &str);
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
    /// Optional status snapshot for admin endpoints. Daemons override
    /// this; the default `None` is what non-daemon callers use. The
    /// payload is opaque JSON so the concrete daemon type (and its
    /// runtime fields like uptime, client counters, …) stays in
    /// `daemon-core` instead of leaking into the shared runtime.
    fn status(&self) -> Option<serde_json::Value> {
        None
    }
}

/// Read-side hook that lets the host app inject display-layer metadata
/// — session titles, project names — into the orchestration dispatcher.
///
/// runtime-core's persistence only stores SDK-level state (session_id,
/// provider, status, ...); user-facing titles and names live in the
/// host app's own store (flowstate's `session_display` /
/// `project_display` tables). Rather than duplicating that data or
/// moving it into runtime-core, we accept a lazy resolver — the
/// dispatcher calls into it when building tool responses. An agent
/// running `list_sessions` then sees the same titles a human sees in
/// the sidebar.
///
/// Default (no provider installed): titles and names are `None` and
/// agents fall back to `firstInputPreview` for disambiguation.
#[async_trait::async_trait]
pub trait AppMetadataProvider: Send + Sync {
    async fn session_title(&self, session_id: &str) -> Option<String>;
    async fn project_name(&self, project_id: &str) -> Option<String>;
}

/// Host-provided git worktree creator. Runtime-core has no git
/// knowledge — this trait lets the Tauri app layer (which already
/// owns `create_git_worktree` / `list_git_worktrees` commands) expose
/// that capability to the orchestration dispatcher so agents can spawn
/// a session directly into a new worktree.
///
/// Implementations are responsible for the full round-trip:
/// 1. Run `git worktree add …` at a path derived from the parent.
/// 2. Create an SDK project row for the new worktree via persistence.
/// 3. Link it to the parent in whatever host-side table tracks
///    worktree ancestry (flowstate's `project_worktree`).
/// 4. Return a `WorktreeBlueprint` describing the new state.
///
/// Missing implementation = `RuntimeCall::CreateWorktree` /
/// `SpawnInWorktree` / `ListWorktrees` return `Internal` errors with
/// a "worktree support not available" message.
#[async_trait::async_trait]
pub trait WorktreeProvisioner: Send + Sync {
    /// Create a new git worktree off `base_project_id` and an SDK
    /// project row to go with it. Returns the new project id + path +
    /// branch info.
    ///
    /// - `branch` — the branch the worktree will check out. If
    ///   `create_branch` is true, git creates it from `base_ref`
    ///   (default: HEAD of the base project).
    /// - `base_ref` — ignored when `create_branch` is false.
    /// - `create_branch` — true: `git worktree add -b <branch>`;
    ///   false: `git worktree add <path> <branch>` (checks out
    ///   existing branch).
    async fn create_worktree(
        &self,
        base_project_id: &str,
        branch: &str,
        base_ref: Option<&str>,
        create_branch: bool,
    ) -> Result<WorktreeBlueprint, String>;

    /// List worktrees. If `base_project_id` is `Some(pid)`, restrict to
    /// worktrees whose parent is `pid`; otherwise return every known
    /// worktree.
    async fn list_worktrees(
        &self,
        base_project_id: Option<&str>,
    ) -> Result<Vec<WorktreeBlueprint>, String>;
}

/// Description of a git worktree as the runtime sees it — the new
/// SDK project id, its on-disk path, the branch, and the parent
/// project it descends from. Returned by [`WorktreeProvisioner`] and
/// carried in [`zenui_provider_api::RuntimeCallResult::Worktree`].
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorktreeBlueprint {
    pub project_id: String,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_project_id: Option<String>,
}

/// Translate the runtime-core blueprint into the provider-api wire
/// shape the dispatcher returns. Keeps the provider-api crate free of
/// a runtime-core dependency.
fn blueprint_to_summary(bp: WorktreeBlueprint) -> zenui_provider_api::WorktreeSummary {
    zenui_provider_api::WorktreeSummary {
        project_id: bp.project_id,
        path: bp.path,
        branch: bp.branch,
        parent_project_id: bp.parent_project_id,
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
    /// Human-readable identifier of the hosting application. Surfaces
    /// in the `BootstrapPayload.app_name` wire field so clients can
    /// label the host without runtime-core having to know who is
    /// embedding it. Provided by the app layer via `RuntimeCore::new`.
    app_name: String,
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
    /// Cached per-session command catalogs (slash commands, agents,
    /// MCP servers). Populated by `spawn_catalog_refresh` after a
    /// `StartSession` / `LoadSession` and whenever the client asks for
    /// a refresh. The value is also broadcast as
    /// `RuntimeEvent::SessionCommandCatalogUpdated`; the cache lets
    /// late-joining transports reseed without re-running the adapter.
    session_command_catalogs: Arc<RwLock<HashMap<String, CommandCatalog>>>,
    /// Dedupe guard — session ids with a catalog refresh currently in
    /// flight. Prevents a `RefreshSessionCommands` burst (e.g. rapid
    /// popup opens) from piling up adapter calls.
    in_flight_catalog_refreshes: Arc<Mutex<HashSet<String>>>,
    /// Per-session live permission mode during an in-flight turn.
    /// Seeded at `send_turn` entry from the turn-start mode and
    /// updated when `answer_permission` carries a mode override or
    /// `UpdatePermissionMode` client message lands mid-stream. Read
    /// by the drain loop's Bypass safety net so a mid-turn mode
    /// change (e.g. ExitPlanMode → Bypass Permissions) takes effect
    /// for every tool call later in the same turn — the earlier
    /// implementation only read the turn-start mode via the local
    /// `permission_mode` parameter, which went stale after the
    /// override.
    in_flight_permission_mode: Arc<RwLock<HashMap<String, PermissionMode>>>,
    /// Per-session notifier tripped at the very end of `send_turn`'s
    /// exit path, right after `TurnCompleted` publishes. `steer_turn`
    /// awaits this after cooperatively interrupting the in-flight turn
    /// so the follow-up `send_turn` only fires once the bridge's
    /// `turnInProgress` flag has cleared — closing the race where two
    /// back-to-back RPCs (`interrupt_turn` + `send_turn`) from the old
    /// frontend steer dance could hit the bridge before the pump had
    /// observed the post-interrupt `result`.
    ///
    /// Keyed by session id; entries are created lazily on first use
    /// and live for the session's lifetime (cheap — just an `Arc<Notify>`).
    /// A `Notify` with no waiters simply drops the notification, so
    /// the finish-turn `notify_waiters()` call is a no-op when nobody
    /// is steering.
    turn_finalized_notifiers: Arc<Mutex<HashMap<String, Arc<Notify>>>>,
    turn_observer: Option<Arc<dyn TurnLifecycleObserver>>,
    /// Cross-session orchestration state: pending reply oneshots,
    /// async-message mailboxes, awaiting-graph for cycle detection,
    /// per-turn budget counter. See `orchestration.rs` for the full
    /// lifecycle. Always `Some` after construction; wrapped in Arc so
    /// spawned tasks can clone it cheaply.
    orchestration_state: Arc<OrchestrationState>,
    /// Back-reference used by the drain-loop hook to spawn dispatcher
    /// tasks that outlive the originating `send_turn` call (e.g. a
    /// `SpawnAndAwait` that blocks until the target session's turn
    /// completes). Installed by `install_self_ref` right after
    /// `Arc::new(RuntimeCore::new(...))`; left as `None` on pure
    /// unit-test builds that never spawn cross-session work.
    self_weak: std::sync::RwLock<Option<std::sync::Weak<RuntimeCore>>>,
    /// Optional host-app metadata resolver — supplies session titles
    /// and project names to the orchestration dispatcher. Installed
    /// via `install_metadata_provider` from the app layer. `None`
    /// means agents see no titles / names (they fall back to
    /// `firstInputPreview` for disambiguation).
    metadata_provider: std::sync::RwLock<Option<Arc<dyn AppMetadataProvider>>>,
    /// Optional host-app worktree provisioner. Installed via
    /// `install_worktree_provisioner`. `None` means worktree
    /// orchestration tools return a structured "not available" error.
    worktree_provisioner: std::sync::RwLock<Option<Arc<dyn WorktreeProvisioner>>>,
    /// Checkpoint store used at turn end to capture a snapshot of the
    /// session's workspace, and on `RewindFiles` / `DeleteSession` to
    /// restore or reclaim. Always set; the daemon bootstrap passes in a
    /// real `FsCheckpointStore`, while unit tests pass `NoopCheckpointStore`.
    /// See the `zenui-checkpoints` crate for details.
    checkpoints: Arc<dyn zenui_checkpoints::CheckpointStore>,
}

impl RuntimeCore {
    pub fn new(
        adapters: Vec<Arc<dyn ProviderAdapter>>,
        orchestration: Arc<OrchestrationService>,
        persistence: Arc<PersistenceService>,
        checkpoints: Arc<dyn zenui_checkpoints::CheckpointStore>,
        turn_observer: Option<Arc<dyn TurnLifecycleObserver>>,
        default_threads_dir: String,
        app_name: String,
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
            app_name,
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
            session_command_catalogs: Arc::new(RwLock::new(HashMap::new())),
            in_flight_catalog_refreshes: Arc::new(Mutex::new(HashSet::new())),
            in_flight_permission_mode: Arc::new(RwLock::new(HashMap::new())),
            turn_finalized_notifiers: Arc::new(Mutex::new(HashMap::new())),
            turn_observer,
            orchestration_state: OrchestrationState::new(),
            self_weak: std::sync::RwLock::new(None),
            metadata_provider: std::sync::RwLock::new(None),
            worktree_provisioner: std::sync::RwLock::new(None),
            checkpoints,
        }
    }

    /// Wire a host-app metadata resolver. Call once after construction
    /// before serving traffic. Safe to call repeatedly; the last caller
    /// wins (useful for tests that swap a fake in and out).
    pub fn install_metadata_provider(&self, provider: Arc<dyn AppMetadataProvider>) {
        if let Ok(mut slot) = self.metadata_provider.write() {
            *slot = Some(provider);
        }
    }

    /// Wire a host-app worktree provisioner. Call once after
    /// construction; leave unset on platforms / host apps that don't
    /// support git worktrees.
    pub fn install_worktree_provisioner(&self, provisioner: Arc<dyn WorktreeProvisioner>) {
        if let Ok(mut slot) = self.worktree_provisioner.write() {
            *slot = Some(provisioner);
        }
    }

    fn metadata_provider(&self) -> Option<Arc<dyn AppMetadataProvider>> {
        self.metadata_provider.read().ok()?.clone()
    }

    fn worktree_provisioner(&self) -> Option<Arc<dyn WorktreeProvisioner>> {
        self.worktree_provisioner.read().ok()?.clone()
    }

    /// Create a flowstate project row for the given on-disk path and
    /// publish `ProjectCreated`. Provided so the app-layer
    /// `WorktreeProvisioner` can register a project for a freshly-
    /// created worktree without duplicating persistence plumbing.
    pub async fn create_project_for_path(
        &self,
        path: String,
    ) -> Result<zenui_provider_api::ProjectRecord, String> {
        let project = self.persist_project_for_path(path).await?;
        self.publish(RuntimeEvent::ProjectCreated {
            project: project.clone(),
        });
        Ok(project)
    }

    /// Persist a project row without broadcasting `ProjectCreated`.
    /// The caller is responsible for firing the event (via `publish`)
    /// once any app-layer companion state (e.g. the `project_worktree`
    /// link) has also been written. Splitting the create and the
    /// publish lets the frontend see both rows in the same `listX()`
    /// hydration read when it reacts to the event — otherwise the
    /// sidebar briefly paints the new worktree project as an
    /// ungrouped, unnamed top-level entry before the link lands.
    pub async fn persist_project_for_path(
        &self,
        path: String,
    ) -> Result<zenui_provider_api::ProjectRecord, String> {
        self.persistence
            .create_project(Some(path))
            .await
            .ok_or_else(|| "persistence.create_project returned None".to_string())
    }

    /// Install a Weak back-reference to the owning Arc. Called once by
    /// the daemon bootstrap right after `Arc::new(RuntimeCore::new(...))`.
    /// The orchestration dispatcher uses this to spawn tasks that need
    /// the full runtime (e.g. scheduling a turn on a peer session) from
    /// inside another turn's drain loop.
    pub fn install_self_ref(self: &Arc<Self>) {
        if let Ok(mut slot) = self.self_weak.write() {
            *slot = Some(Arc::downgrade(self));
        }
    }

    /// Upgrade the back-reference into a live Arc. Returns `None` if
    /// the runtime has been dropped (shouldn't happen in practice, but
    /// the dispatcher code handles it defensively).
    fn self_arc(&self) -> Option<Arc<RuntimeCore>> {
        self.self_weak
            .read()
            .ok()
            .and_then(|slot| slot.as_ref().and_then(|w| w.upgrade()))
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
    /// Otherwise falls back to the adapter's `default_enabled()` hook,
    /// keeping the decision inside the provider crate. If no adapter
    /// for `kind` is registered the provider is reported disabled.
    pub fn is_provider_enabled(&self, kind: ProviderKind) -> bool {
        self.provider_enablement
            .read()
            .ok()
            .and_then(|lock| lock.get(&kind).copied())
            .unwrap_or_else(|| {
                self.adapters
                    .get(&kind)
                    .map(|adapter| adapter.default_enabled())
                    .unwrap_or(false)
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
                features: zenui_provider_api::ProviderFeatures::default(),
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

    pub async fn bootstrap(&self, ws_url: Option<String>) -> BootstrapPayload {
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
                    tracing::info!(?kind, ?checked_at, "loaded cached provider health");
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
            app_name: self.app_name.clone(),
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

            let _ = event_tx.send(RuntimeEvent::ProviderHealthUpdated { status });

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

    /// Read-side accessor for the cached command catalog. Returns `None`
    /// when the catalog hasn't been fetched yet; callers should fire a
    /// `RefreshSessionCommands` to prime the cache.
    pub fn session_command_catalog(&self, session_id: &str) -> Option<CommandCatalog> {
        self.session_command_catalogs
            .read()
            .ok()
            .and_then(|map| map.get(session_id).cloned())
    }

    /// Background-refresh the command catalog for a session. Dedup guard
    /// keyed by session id; repeated calls while a refresh is in flight
    /// are ignored. Errors are logged and swallowed — the catalog is a
    /// best-effort UX affordance, never essential to a turn.
    fn spawn_catalog_refresh(&self, session: SessionDetail) {
        let Some(adapter) = self.adapters.get(&session.summary.provider).cloned() else {
            return;
        };
        let event_tx = self.event_tx.clone();
        let catalogs = self.session_command_catalogs.clone();
        let in_flight = self.in_flight_catalog_refreshes.clone();
        let session_id = session.summary.session_id.clone();
        tokio::spawn(async move {
            {
                let mut guard = in_flight.lock().await;
                if !guard.insert(session_id.clone()) {
                    tracing::debug!(
                        %session_id,
                        "skipping duplicate catalog refresh"
                    );
                    return;
                }
            }

            let result = adapter.session_command_catalog(&session).await;

            {
                let mut guard = in_flight.lock().await;
                guard.remove(&session_id);
            }

            match result {
                Ok(catalog) => {
                    tracing::debug!(
                        %session_id,
                        provider = ?session.summary.provider,
                        commands = catalog.commands.len(),
                        agents = catalog.agents.len(),
                        mcp_servers = catalog.mcp_servers.len(),
                        "catalog refresh complete"
                    );
                    if let Ok(mut map) = catalogs.write() {
                        map.insert(session_id.clone(), catalog.clone());
                    }
                    let _ = event_tx.send(RuntimeEvent::SessionCommandCatalogUpdated {
                        session_id,
                        catalog,
                    });
                }
                Err(error) => {
                    tracing::warn!(%session_id, %error, "catalog refresh failed");
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
            ClientMessage::LoadSession { session_id, limit } => {
                match self.live_session_detail_limited(&session_id, limit).await {
                    Some(mut session) => {
                        // Only spawn a catalog refresh the first time we see
                        // a session id since daemon start. Switching back
                        // and forth between threads would otherwise retrigger
                        // the refresh on every navigation — on Claude SDK
                        // that's a Node bridge spawn per click. The cached
                        // catalog lives until the daemon restarts; the user
                        // gets a fresh one by creating a new thread.
                        self.resolve_session_cwd(&mut session).await;
                        let already_cached = self
                            .session_command_catalogs
                            .read()
                            .ok()
                            .map(|map| map.contains_key(&session_id))
                            .unwrap_or(false);
                        if !already_cached {
                            self.spawn_catalog_refresh(session.clone());
                        }
                        Some(ServerMessage::SessionLoaded { session })
                    }
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
                    Ok(session) => {
                        let summary = session.summary.clone();
                        // Prime the command catalog on the very first
                        // StartSession so the composer popup has data
                        // before the user even focuses the textarea.
                        self.spawn_catalog_refresh(session);
                        Some(ServerMessage::SessionCreated { session: summary })
                    }
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
            ClientMessage::SteerTurn {
                session_id,
                input,
                images,
                permission_mode,
                reasoning_effort,
            } => {
                let mode = permission_mode.unwrap_or_default();
                match self
                    .steer_turn(session_id, input, images, mode, reasoning_effort)
                    .await
                {
                    Ok(message) => Some(ServerMessage::Ack { message }),
                    Err(error) => Some(ServerMessage::Error { message: error }),
                }
            }
            ClientMessage::UpdatePermissionMode {
                session_id,
                permission_mode,
            } => {
                match self
                    .update_permission_mode(session_id, permission_mode)
                    .await
                {
                    Ok(()) => Some(ServerMessage::Ack {
                        message: "Permission mode updated.".to_string(),
                    }),
                    Err(error) => Some(ServerMessage::Error { message: error }),
                }
            }
            ClientMessage::UpdateSessionSettings {
                session_id,
                compact_custom_instructions,
            } => {
                match self
                    .update_session_settings(session_id, compact_custom_instructions)
                    .await
                {
                    Ok(()) => Some(ServerMessage::Ack {
                        message: "Session settings updated.".to_string(),
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
                self.answer_question(&session_id, &request_id, answers)
                    .await;
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
            ClientMessage::RefreshSessionCommands { session_id } => {
                // Resolve cwd off the live session before handing the
                // detail to the spawned refresh — the default adapter
                // scan relies on `session.cwd` to find project-local
                // SKILL.md files.
                match self.live_session_detail(&session_id).await {
                    Some(mut session) => {
                        self.resolve_session_cwd(&mut session).await;
                        self.spawn_catalog_refresh(session);
                        Some(ServerMessage::Ack {
                            message: format!("Refreshing commands for `{session_id}`."),
                        })
                    }
                    None => Some(ServerMessage::Error {
                        message: format!("Session `{session_id}` not found."),
                    }),
                }
            }
            ClientMessage::GetContextUsage { session_id } => {
                // Lazy, on-demand RPC — fired when the user opens
                // the context-usage popover. Adapters that don't
                // implement context introspection return `Ok(None)`
                // from the default; frontend hides the popover
                // trigger upfront via `ProviderFeatures`, so
                // `None` here only lands if something bypasses
                // that gate.
                let session = match self.live_session_detail(&session_id).await {
                    Some(s) => s,
                    None => {
                        return Some(ServerMessage::Error {
                            message: format!("Session `{session_id}` not found."),
                        });
                    }
                };
                let Some(adapter) = self.adapters.get(&session.summary.provider).cloned() else {
                    return Some(ServerMessage::Error {
                        message: format!(
                            "No adapter for provider `{}`",
                            session.summary.provider.label()
                        ),
                    });
                };
                match adapter.get_context_usage(&session).await {
                    Ok(breakdown) => Some(ServerMessage::ContextUsage {
                        session_id,
                        breakdown,
                    }),
                    Err(err) => Some(ServerMessage::Error {
                        message: format!("get_context_usage failed: {err}"),
                    }),
                }
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
            ClientMessage::RewindFiles {
                session_id,
                turn_id,
                dry_run,
                confirm_conflicts,
            } => {
                // The handler maps `CheckpointStore` outcomes and error
                // variants onto the wire `RewindOutcomeWire` shape. See
                // the checkpoints crate for the full semantics
                // (diff-driven restore, conflict detection, dry-run
                // preview). Infrastructure errors (IO, sqlite) surface
                // as `ServerMessage::Error`; everything else — including
                // "no checkpoint for this turn" and "session has no
                // cwd" — is a clean outcome on the RewindFilesResult.
                let session = match self.live_session_detail(&session_id).await {
                    Some(s) => s,
                    None => {
                        return Some(ServerMessage::RewindFilesResult {
                            session_id,
                            turn_id,
                            outcome: RewindOutcomeWire::Unavailable {
                                reason: RewindUnavailableReason::SessionNotFound,
                            },
                        });
                    }
                };
                let Some(cwd) = session.cwd.as_deref() else {
                    return Some(ServerMessage::RewindFilesResult {
                        session_id,
                        turn_id,
                        outcome: RewindOutcomeWire::Unavailable {
                            reason: RewindUnavailableReason::NoWorkspace,
                        },
                    });
                };

                let opts = zenui_checkpoints::RestoreOptions {
                    dry_run,
                    confirm_conflicts,
                };
                let outcome = match self
                    .checkpoints
                    .restore(&session_id, &turn_id, std::path::Path::new(cwd), opts)
                    .await
                {
                    Ok(zenui_checkpoints::RestoreResult::Applied(o)) => {
                        RewindOutcomeWire::Applied {
                            paths_restored: o.paths_restored,
                            paths_deleted: o.paths_deleted,
                            paths_skipped: o.paths_skipped,
                            dry_run: o.dry_run,
                        }
                    }
                    Ok(zenui_checkpoints::RestoreResult::NeedsConfirmation(report)) => {
                        RewindOutcomeWire::NeedsConfirmation {
                            conflicts: report
                                .conflicts
                                .into_iter()
                                .map(|c| RewindConflictWire {
                                    path: c.path,
                                    session_last_seen_hash: c
                                        .session_last_seen_hash
                                        .map(|h| h.as_str().to_string()),
                                    disk_current_hash: c
                                        .disk_current_hash
                                        .map(|h| h.as_str().to_string()),
                                })
                                .collect(),
                        }
                    }
                    Err(zenui_checkpoints::CheckpointError::NoCheckpoint { .. }) => {
                        RewindOutcomeWire::Unavailable {
                            reason: RewindUnavailableReason::NoCheckpoint,
                        }
                    }
                    Err(e) => {
                        // Actual failure (IO, sqlite, corrupt manifest).
                        // Surface as Error so clients can distinguish
                        // "feature can't run right now" from "rewind is
                        // semantically unavailable".
                        return Some(ServerMessage::Error {
                            message: format!("rewind failed: {e}"),
                        });
                    }
                };
                Some(ServerMessage::RewindFilesResult {
                    session_id,
                    turn_id,
                    outcome,
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
                        if let Err(e) = adapter.update_session_model(&session, model.clone()).await
                        {
                            tracing::warn!("Failed to forward model change to adapter: {e}");
                        }
                    }
                    self.publish(RuntimeEvent::SessionModelUpdated { session_id, model });
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
        // Update the live-mode tracker BEFORE we forward to the sink.
        // The sink wakes the adapter's pending canUseTool promise,
        // which may resolve quickly enough for the SDK to issue a
        // subsequent tool call that re-enters the drain loop — if
        // the safety net is still reading the stale turn-start mode,
        // that tool call would get prompted instead of auto-allowed.
        // Updating first makes the mode change visible to the drain
        // loop by the time it matters.
        if let Some(mode) = mode_override {
            if let Ok(mut live) = self.in_flight_permission_mode.write() {
                live.insert(session_id.to_string(), mode);
            }
        }
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
            tracing::warn!(
                session_id,
                request_id,
                "no active sink for permission answer"
            );
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
                return Err(format!(
                    "Failed to start {} session: {}",
                    provider.label(),
                    error
                ));
            }
        }

        self.persistence.upsert_session(session.clone()).await;
        self.publish(RuntimeEvent::SessionStarted {
            session: session.summary.clone(),
        });
        Ok(session)
    }

    pub(crate) async fn send_turn(
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
        // Seed the live-mode tracker. Any mid-turn override via
        // `answer_permission` or `UpdatePermissionMode` mutates this
        // entry so the drain loop's bypass safety net reads the
        // current effective mode, not the frozen turn-start one.
        // The RAII guard below removes the entry on every exit path
        // (normal return, early `?`, panic) so it can't leak.
        if let Ok(mut live) = self.in_flight_permission_mode.write() {
            live.insert(session_id.clone(), permission_mode);
        }
        let _in_flight_mode_guard = InFlightPermissionModeGuard {
            map: self.in_flight_permission_mode.clone(),
            session_id: session_id.clone(),
        };
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
        if let Some(stored) = session.turns.iter_mut().find(|t| t.turn_id == turn.turn_id) {
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
                .or_insert_with(|| Arc::new(Mutex::new(HashMap::new())))
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
                        _ => blocks.push(ContentBlock::Text {
                            text: delta.clone(),
                        }),
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
                        _ => blocks.push(ContentBlock::Reasoning {
                            text: delta.clone(),
                        }),
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
                        // Stamp the issue time so the frontend can
                        // render a live elapsed counter. We do it
                        // cross-provider so even adapters that
                        // haven't opted into the `tool_progress`
                        // feature flag get a reasonable "Bash ·
                        // 12s" if the UI ever unhides the timer.
                        started_at: Some(chrono::Utc::now().to_rfc3339()),
                        // Heartbeat tracker. None on creation; gets
                        // stamped by ProviderTurnEvent::ToolProgress
                        // events for providers that opt into
                        // `ProviderFeatures.tool_progress`. The
                        // frontend's stalled-tool pip compares this
                        // against wall time (≈30s threshold) while
                        // the call is still pending.
                        last_progress_at: None,
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
                ProviderTurnEvent::ToolCallCompleted {
                    call_id,
                    output,
                    error,
                } => {
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
                ProviderTurnEvent::ToolProgress {
                    call_id,
                    tool_name,
                    parent_call_id,
                    occurred_at,
                } => {
                    // Stamp `last_progress_at` on the matching live
                    // ToolCall so a mid-turn persistence write (or a
                    // session reload) preserves the heartbeat the
                    // frontend's stalled-tool pip is comparing
                    // against. Silently ignore heartbeats whose
                    // call_id we don't recognise — it usually means
                    // the heartbeat raced ahead of ToolCallStarted
                    // by a few ms; the next heartbeat will land on a
                    // resolved ToolCall and the pip catches up.
                    if let Some(tc) = tool_calls.iter_mut().find(|tc| tc.call_id == call_id) {
                        tc.last_progress_at = Some(occurred_at.clone());
                    }
                    self.publish(RuntimeEvent::ToolProgress {
                        session_id: sid.clone(),
                        turn_id: tid.clone(),
                        call_id,
                        tool_name,
                        parent_call_id,
                        occurred_at,
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
                    // Bypass-mode safety net. The user explicitly
                    // opted out of permission prompting by selecting
                    // Bypass Permissions. Primary fix lives at each
                    // adapter's emission site (e.g. the Claude SDK
                    // bridge short-circuits inside canUseTool before
                    // ever emitting), but this cross-provider net
                    // catches any permission request that still
                    // slips through — CLI-backed adapters whose
                    // upstream binary forwards prompts in danger-
                    // mode, future adapters that don't know about
                    // bypass yet, or a stale bridge build on
                    // someone's disk. Auto-answer Allow on the sink
                    // and skip the publish so no dialog reaches the
                    // UI.
                    //
                    // Read the LIVE mode, not the turn-start
                    // parameter. If the user just approved an
                    // ExitPlanMode → Bypass, `answer_permission`
                    // already flipped the live-mode entry; the
                    // local `permission_mode` captured at
                    // `send_turn` entry is stale and would fall
                    // through to the publish path, re-prompting
                    // for the first post-plan tool call.
                    let effective_mode = self
                        .in_flight_permission_mode
                        .read()
                        .ok()
                        .and_then(|live| live.get(&sid).copied())
                        .unwrap_or(permission_mode);
                    if effective_mode == PermissionMode::Bypass {
                        let sink = self.active_sinks.lock().await.get(&sid).cloned();
                        if let Some(sink) = sink {
                            sink.resolve_permission(&request_id, PermissionDecision::Allow)
                                .await;
                        } else {
                            tracing::warn!(
                                session_id = %sid,
                                request_id,
                                "bypass safety net: no active sink to auto-allow"
                            );
                        }
                    } else {
                        self.publish(RuntimeEvent::PermissionRequested {
                            session_id: sid.clone(),
                            turn_id: tid.clone(),
                            request_id,
                            tool_name,
                            input,
                            suggested: suggested_decision,
                        });
                    }
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
                    model,
                } => {
                    subagents.push(SubagentRecord {
                        agent_id: agent_id.clone(),
                        parent_call_id: parent_call_id.clone(),
                        agent_type: agent_type.clone(),
                        prompt: prompt.clone(),
                        model: model.clone(),
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
                        model,
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
                ProviderTurnEvent::SubagentModelObserved { agent_id, model } => {
                    // The observed (SDK-resolved) model is stronger
                    // than the planned catalog value, so always
                    // overwrite when this event fires.
                    if let Some(rec) = subagents.iter_mut().find(|r| r.agent_id == agent_id) {
                        rec.model = Some(model.clone());
                    }
                    self.publish(RuntimeEvent::SubagentModelObserved {
                        session_id: sid.clone(),
                        turn_id: tid.clone(),
                        agent_id,
                        model,
                    });
                }
                ProviderTurnEvent::TurnUsage { usage: u } => {
                    // Stash on the per-turn local so the final TurnRecord
                    // merges it on completion (see merge site below where
                    // `t.usage = usage;` is assigned before TurnCompleted).
                    usage = Some(u.clone());
                    // Broadcast incremental usage so clients rendering a
                    // live context indicator can update mid-turn rather
                    // than waiting for TurnCompleted. The provider bridge
                    // emits `turn_usage` per API call — for a long tool
                    // loop that's the difference between a frozen
                    // numerator and one that updates as the SDK works.
                    self.publish(RuntimeEvent::TurnUsageUpdated {
                        session_id: sid.clone(),
                        turn_id: tid.clone(),
                        usage: u,
                    });
                }
                ProviderTurnEvent::RateLimitUpdated { info } => {
                    // Rate limits are account-wide, not per-turn, so
                    // we just forward them to the runtime event
                    // stream without touching any turn-local state.
                    self.publish(RuntimeEvent::RateLimitUpdated { info });
                }
                ProviderTurnEvent::ModelResolved { model: resolved } => {
                    // The provider told us which model it actually ran
                    // this turn on. If that differs from what's stored
                    // on the session (typically because the user or the
                    // default-settings picked an alias like `sonnet`
                    // and the SDK resolved it to a pinned
                    // `claude-sonnet-4-5-<date>` id), upgrade
                    // `session.summary.model` and persist. The model-
                    // selector dropdown matches against the pinned id
                    // from the provider's `models` list, so without
                    // this the dropdown never highlights the active
                    // entry for alias-backed sessions.
                    //
                    // Guarded so a turn running on an already-pinned
                    // model doesn't write the same value back and
                    // churn persistence.
                    if session.summary.model.as_deref() != Some(resolved.as_str()) {
                        session.summary.model = Some(resolved.clone());
                        self.persistence.upsert_session(session.clone()).await;
                        self.publish(RuntimeEvent::SessionModelUpdated {
                            session_id: sid.clone(),
                            model: resolved,
                        });
                    }
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
                ProviderTurnEvent::CompactBoundary {
                    trigger,
                    pre_tokens,
                    post_tokens,
                    duration_ms,
                } => {
                    // Merge into the last Compact block if it's still
                    // waiting on metrics (summary arrived first);
                    // otherwise append a new one. Don't re-use a
                    // completed Compact block — two compactions in
                    // one turn (rare but possible on very long turns)
                    // must show as two separate blocks.
                    let merged_into_existing = match blocks.last_mut() {
                        Some(ContentBlock::Compact {
                            trigger: t,
                            pre_tokens: pt,
                            post_tokens: po,
                            duration_ms: dm,
                            summary: _,
                        }) if pt.is_none() && po.is_none() && dm.is_none() => {
                            *t = trigger;
                            *pt = pre_tokens;
                            *po = post_tokens;
                            *dm = duration_ms;
                            true
                        }
                        _ => false,
                    };
                    if !merged_into_existing {
                        blocks.push(ContentBlock::Compact {
                            trigger,
                            pre_tokens,
                            post_tokens,
                            duration_ms,
                            summary: None,
                        });
                    }
                    // Pull the merged state back out so the live event
                    // carries whatever the block now holds (summary
                    // may already be present if it arrived first).
                    let (summary_now, trigger_now) = match blocks.last() {
                        Some(ContentBlock::Compact {
                            summary, trigger, ..
                        }) => (summary.clone(), *trigger),
                        _ => (None, trigger),
                    };
                    self.publish(RuntimeEvent::CompactUpdated {
                        session_id: sid.clone(),
                        turn_id: tid.clone(),
                        trigger: trigger_now,
                        pre_tokens,
                        post_tokens,
                        duration_ms,
                        summary: summary_now,
                    });
                }
                ProviderTurnEvent::CompactSummary { trigger, summary } => {
                    let merged_into_existing = match blocks.last_mut() {
                        Some(ContentBlock::Compact {
                            summary: s,
                            trigger: t,
                            ..
                        }) if s.is_none() => {
                            *s = Some(summary.clone());
                            *t = trigger;
                            true
                        }
                        _ => false,
                    };
                    if !merged_into_existing {
                        blocks.push(ContentBlock::Compact {
                            trigger,
                            pre_tokens: None,
                            post_tokens: None,
                            duration_ms: None,
                            summary: Some(summary.clone()),
                        });
                    }
                    let (pt, po, dm, tr) = match blocks.last() {
                        Some(ContentBlock::Compact {
                            pre_tokens,
                            post_tokens,
                            duration_ms,
                            trigger,
                            ..
                        }) => (*pre_tokens, *post_tokens, *duration_ms, *trigger),
                        _ => (None, None, None, trigger),
                    };
                    self.publish(RuntimeEvent::CompactUpdated {
                        session_id: sid.clone(),
                        turn_id: tid.clone(),
                        trigger: tr,
                        pre_tokens: pt,
                        post_tokens: po,
                        duration_ms: dm,
                        summary: Some(summary),
                    });
                }
                ProviderTurnEvent::MemoryRecall { mode, memories } => {
                    blocks.push(ContentBlock::MemoryRecall {
                        mode,
                        memories: memories.clone(),
                    });
                    self.publish(RuntimeEvent::MemoryRecalled {
                        session_id: sid.clone(),
                        turn_id: tid.clone(),
                        mode,
                        memories,
                    });
                }
                ProviderTurnEvent::StatusChanged { phase } => {
                    // Pass-through. Working-indicator reads the
                    // latest phase from chat-view's local state;
                    // we don't persist phases to `TurnRecord`
                    // because they're transient and meaningless
                    // once the turn completes.
                    self.publish(RuntimeEvent::TurnStatusChanged {
                        session_id: sid.clone(),
                        turn_id: tid.clone(),
                        phase,
                    });
                }
                ProviderTurnEvent::TurnRetrying {
                    attempt,
                    max_retries,
                    retry_delay_ms,
                    error_status,
                    error,
                } => {
                    // Pure diagnostic signal. Frontend shows a
                    // banner; clears on the next assistant delta
                    // or turn completion. Not persisted.
                    self.publish(RuntimeEvent::TurnRetrying {
                        session_id: sid.clone(),
                        turn_id: tid.clone(),
                        attempt,
                        max_retries,
                        retry_delay_ms,
                        error_status,
                        error,
                    });
                }
                ProviderTurnEvent::PromptSuggestion { suggestion } => {
                    // Forward the latest predicted next prompt to
                    // the frontend. Not persisted — suggestions
                    // are session-local affordances and stale
                    // ones aren't useful on re-open.
                    self.publish(RuntimeEvent::PromptSuggested {
                        session_id: sid.clone(),
                        turn_id: tid.clone(),
                        suggestion,
                    });
                }
                ProviderTurnEvent::RuntimeCall { request_id, call } => {
                    // Cross-session orchestration: the agent invoked a
                    // flowstate_* capability tool. Dispatch on a separate
                    // task so the drain loop keeps receiving events for
                    // the current turn while the peer work runs. The
                    // spawned task resolves the sink's runtime_pending
                    // oneshot when the dispatcher returns.
                    //
                    // We need a live `Arc<RuntimeCore>` to outlive the
                    // drain loop — `self_arc()` upgrades the back-ref.
                    // Missing ref = install_self_ref was never called
                    // (test harness); fail-close with Cancelled.
                    let Some(rc) = self.self_arc() else {
                        tracing::warn!(
                            "runtime_call arrived but self_weak not installed; rejecting"
                        );
                        let sink_for_err = {
                            let guard = self.active_sinks.lock().await;
                            guard.get(&sid).cloned()
                        };
                        if let Some(sink) = sink_for_err {
                            sink.resolve_runtime_call(
                                &request_id,
                                Err(RuntimeCallError::Internal {
                                    message: "runtime back-reference not installed".to_string(),
                                }),
                            )
                            .await;
                        }
                        continue;
                    };
                    let origin = RuntimeCallOrigin {
                        session_id: sid.clone(),
                        turn_id: tid.clone(),
                    };
                    let sink_for_resolve = {
                        let guard = self.active_sinks.lock().await;
                        guard.get(&sid).cloned()
                    };
                    crate::orchestration::spawn_dispatch(
                        rc,
                        origin,
                        call,
                        request_id,
                        sink_for_resolve,
                    );
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
                // Intentionally leave `session.provider_state` untouched.
                // We reach here when the adapter surfaced an Err during
                // an interrupt (e.g. bridge stdout closed, writer task
                // died forwarding a permission answer). The adapter's
                // Err path also invalidated its bridge cache, so the
                // next turn spawns a fresh bridge and hydrates its
                // `resumeSessionId` from whatever is still on disk in
                // `provider_state.native_thread_id`.
                //
                // Invariant (maintained by the provider bridge's
                // two-phase session-id scheme): the persisted
                // `native_thread_id` always points at the last turn
                // the SDK COMMITTED via its `result` message, never at
                // an interrupted turn's uncommitted init id. So
                // preserving it here is exactly what we want — don't
                // "helpfully" clear it.
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

        // Capture a workspace checkpoint for this turn. Happens after
        // persistence so the turn is durable regardless of capture
        // outcome. Fire-and-forget semantics — capture failures never
        // bubble up to the turn; the user just sees "revert unavailable
        // for this turn" if they later try to rewind here. Gated on
        // `session.cwd` because checkpointing requires a workspace to
        // snapshot; pure SDK sessions without a project go unchecked.
        //
        // The separate `CheckpointCaptured` event is what the frontend
        // uses to light up the per-turn "Revert since here" affordance,
        // so we publish it only on success — turns where capture failed
        // get no affordance, which is the correct behavior (pretending
        // to offer rewind when no checkpoint exists would be worse).
        if let Some(cwd) = session.cwd.as_deref() {
            match self
                .checkpoints
                .capture(
                    &session.summary.session_id,
                    &merged_turn.turn_id,
                    std::path::Path::new(cwd),
                )
                .await
            {
                Ok(Some(_handle)) => {
                    self.publish(RuntimeEvent::CheckpointCaptured {
                        session_id: session.summary.session_id.clone(),
                        turn_id: merged_turn.turn_id.clone(),
                    });
                }
                Ok(None) => {
                    // Store deliberately skipped — e.g. disabled by
                    // user setting (PR 5.5). No affordance lit up.
                }
                Err(e) => {
                    tracing::warn!(
                        session_id = %session.summary.session_id,
                        turn_id = %merged_turn.turn_id,
                        error = %e,
                        "checkpoint capture failed",
                    );
                }
            }
        }

        if status == TurnStatus::Failed {
            self.publish(RuntimeEvent::Error {
                message: merged_turn.output.clone(),
            });
        }

        // Capture the final canonical output BEFORE moving `merged_turn`
        // into the `TurnCompleted` event — the orchestration dispatcher
        // uses it to resolve pending replies below.
        let final_output_for_orch = merged_turn.output.clone();
        let finished_turn_id = merged_turn.turn_id.clone();

        self.publish(RuntimeEvent::TurnCompleted {
            session_id: session.summary.session_id.clone(),
            session: session.summary.clone(),
            turn: merged_turn,
        });

        // Cross-session orchestration: deliver this turn's final output
        // to any peers awaiting a reply from this session. Fan out to
        // every registered awaiter in one pass — they can't affect
        // each other (oneshot senders are independent).
        let pending = self
            .orchestration_state
            .drain_replies_for(&session.summary.session_id)
            .await;
        for awaiter in pending {
            resolve_pending_reply(awaiter, &finished_turn_id, &final_output_for_orch, status);
        }
        // Release this turn's orchestration budget. Per-turn counter,
        // so a new turn starts fresh.
        self.orchestration_state
            .release_budget(&finished_turn_id)
            .await;

        // If the just-finished session has queued async messages in its
        // mailbox (from a peer's `flowstate_send`), drain the next one
        // and schedule a fresh turn. Fire-and-forget — we don't await
        // the new turn here; it runs like any user-initiated one.
        if status == TurnStatus::Completed {
            if let Some(next) = self
                .orchestration_state
                .pop_message(&session.summary.session_id)
                .await
            {
                if let Some(rc) = self.self_arc() {
                    let target = session.summary.session_id.clone();
                    crate::orchestration::spawn_peer_turn(
                        rc,
                        target,
                        next.message,
                        "mailbox_drain",
                    );
                }
            }
        }

        // Wake any `steer_turn` awaiting the turn's post-interrupt
        // unwind. `notify_waiters()` is a no-op when nobody is waiting,
        // so the normal (non-steer) completion path pays nothing for
        // this. We look up (but don't create) the per-session entry —
        // `steer_turn` is responsible for inserting before it awaits.
        if let Some(notifier) = self
            .turn_finalized_notifiers
            .lock()
            .await
            .get(&session.summary.session_id)
            .cloned()
        {
            notifier.notify_waiters();
        }

        result
    }

    // ============================================================
    // Cross-session orchestration dispatcher
    // ============================================================

    /// Entry point for every `RuntimeCall` produced by an agent. The
    /// drain loop in `send_turn` spawns a task that calls into here.
    /// All variants share the same budget + cycle-guard rails before
    /// branching to their specific handler.
    pub(crate) async fn dispatch_runtime_call(
        self: Arc<Self>,
        origin: RuntimeCallOrigin,
        call: RuntimeCall,
    ) -> Result<RuntimeCallResult, RuntimeCallError> {
        // Reserve budget first — stops fan-out storms at the door.
        self.orchestration_state
            .reserve_budget(&origin.turn_id)
            .await?;

        match call {
            RuntimeCall::SpawnAndAwait {
                project_id,
                provider,
                model,
                initial_message,
                timeout_secs,
            } => {
                self.dispatch_spawn_and_await(
                    origin,
                    project_id,
                    provider,
                    model,
                    initial_message,
                    timeout_secs,
                )
                .await
            }
            RuntimeCall::Spawn {
                project_id,
                provider,
                model,
                initial_message,
            } => {
                self.dispatch_spawn(origin, project_id, provider, model, initial_message)
                    .await
            }
            RuntimeCall::SendAndAwait {
                session_id,
                message,
                timeout_secs,
            } => {
                self.dispatch_send_and_await(origin, session_id, message, timeout_secs)
                    .await
            }
            RuntimeCall::Send {
                session_id,
                message,
            } => self.dispatch_send(origin, session_id, message).await,
            RuntimeCall::Poll {
                session_id,
                since_turn_id,
            } => self.dispatch_poll(session_id, since_turn_id).await,
            RuntimeCall::ReadSession {
                session_id,
                last_turns,
            } => self.dispatch_read_session(session_id, last_turns).await,
            RuntimeCall::ListSessions { project_id } => {
                self.dispatch_list_sessions(origin, project_id).await
            }
            RuntimeCall::ListProjects => self.dispatch_list_projects().await,
            RuntimeCall::CreateWorktree {
                base_project_id,
                branch,
                base_ref,
                create_branch,
            } => {
                self.dispatch_create_worktree(
                    &base_project_id,
                    &branch,
                    base_ref.as_deref(),
                    create_branch.unwrap_or(true),
                )
                .await
            }
            RuntimeCall::ListWorktrees { base_project_id } => {
                self.dispatch_list_worktrees(base_project_id.as_deref())
                    .await
            }
            RuntimeCall::SpawnInWorktree {
                base_project_id,
                branch,
                base_ref,
                create_branch,
                initial_message,
                provider,
                model,
                await_reply,
                timeout_secs,
            } => {
                self.dispatch_spawn_in_worktree(
                    origin,
                    base_project_id,
                    branch,
                    base_ref,
                    create_branch.unwrap_or(true),
                    initial_message,
                    provider,
                    model,
                    await_reply.unwrap_or(false),
                    timeout_secs,
                )
                .await
            }
        }
    }

    /// Inherit provider / model from the caller's session if the
    /// RuntimeCall didn't specify one. Keeps the agent from having to
    /// remember what it is just to spawn a like-for-like peer.
    async fn resolve_spawn_defaults(
        &self,
        origin: &RuntimeCallOrigin,
        provider: Option<ProviderKind>,
        model: Option<String>,
    ) -> Result<(ProviderKind, Option<String>), RuntimeCallError> {
        if let (Some(p), m) = (provider, model.clone()) {
            return Ok((p, m));
        }
        let caller = self
            .persistence
            .get_session(&origin.session_id)
            .await
            .ok_or_else(|| RuntimeCallError::SessionNotFound {
                session_id: origin.session_id.clone(),
            })?;
        Ok((
            provider.unwrap_or(caller.summary.provider),
            model.or(caller.summary.model),
        ))
    }

    async fn dispatch_spawn_and_await(
        self: Arc<Self>,
        origin: RuntimeCallOrigin,
        project_id: Option<String>,
        provider: Option<ProviderKind>,
        model: Option<String>,
        initial_message: String,
        timeout_secs: Option<u64>,
    ) -> Result<RuntimeCallResult, RuntimeCallError> {
        let (provider, model) = self
            .resolve_spawn_defaults(&origin, provider, model)
            .await?;
        if !self.is_provider_enabled(provider) {
            return Err(RuntimeCallError::ProviderDisabled {
                provider: provider.label().to_string(),
            });
        }
        let new_session = self
            .start_session(provider, model, project_id.clone())
            .await
            .map_err(|e| RuntimeCallError::Internal { message: e })?;
        let new_sid = new_session.summary.session_id.clone();
        self.publish(RuntimeEvent::SessionLinked {
            from_session_id: origin.session_id.clone(),
            to_session_id: new_sid.clone(),
            reason: SessionLinkReason::Spawn,
        });

        // Register the reply awaiter BEFORE scheduling the peer turn —
        // otherwise the target could finish before we register and the
        // reply would silently fall on the floor.
        self.orchestration_state
            .register_await(&origin.session_id, &new_sid)
            .await?;
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.orchestration_state
            .register_reply(
                &new_sid,
                PendingReply {
                    after_turn_id: None,
                    sender: reply_tx,
                },
            )
            .await;

        let rc_for_turn = self.clone();
        let peer_sid = new_sid.clone();
        crate::orchestration::spawn_peer_turn(
            rc_for_turn,
            peer_sid,
            initial_message,
            "spawn_and_await",
        );

        let timeout = clamp_timeout(timeout_secs);
        let outcome = tokio::time::timeout(timeout, reply_rx).await;
        self.orchestration_state
            .unregister_await(&origin.session_id, &new_sid)
            .await;

        match outcome {
            Ok(Ok((_turn_id, reply))) => Ok(RuntimeCallResult::Spawned {
                session_id: new_sid,
                reply: Some(reply),
            }),
            Ok(Err(_)) => Err(RuntimeCallError::Cancelled),
            Err(_) => Err(RuntimeCallError::Timeout {
                session_id: new_sid,
            }),
        }
    }

    async fn dispatch_spawn(
        self: Arc<Self>,
        origin: RuntimeCallOrigin,
        project_id: Option<String>,
        provider: Option<ProviderKind>,
        model: Option<String>,
        initial_message: String,
    ) -> Result<RuntimeCallResult, RuntimeCallError> {
        let (provider, model) = self
            .resolve_spawn_defaults(&origin, provider, model)
            .await?;
        if !self.is_provider_enabled(provider) {
            return Err(RuntimeCallError::ProviderDisabled {
                provider: provider.label().to_string(),
            });
        }
        let new_session = self
            .start_session(provider, model, project_id)
            .await
            .map_err(|e| RuntimeCallError::Internal { message: e })?;
        let new_sid = new_session.summary.session_id.clone();
        self.publish(RuntimeEvent::SessionLinked {
            from_session_id: origin.session_id.clone(),
            to_session_id: new_sid.clone(),
            reason: SessionLinkReason::Spawn,
        });

        let rc_for_turn = self.clone();
        let peer_sid = new_sid.clone();
        crate::orchestration::spawn_peer_turn(rc_for_turn, peer_sid, initial_message, "spawn");

        Ok(RuntimeCallResult::SpawnedAsync {
            session_id: new_sid,
        })
    }

    async fn dispatch_send_and_await(
        self: Arc<Self>,
        origin: RuntimeCallOrigin,
        target_session_id: String,
        message: String,
        timeout_secs: Option<u64>,
    ) -> Result<RuntimeCallResult, RuntimeCallError> {
        // Target must exist.
        let _ = self
            .persistence
            .get_session(&target_session_id)
            .await
            .ok_or(RuntimeCallError::SessionNotFound {
                session_id: target_session_id.clone(),
            })?;

        self.orchestration_state
            .register_await(&origin.session_id, &target_session_id)
            .await?;
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.orchestration_state
            .register_reply(
                &target_session_id,
                PendingReply {
                    after_turn_id: None,
                    sender: reply_tx,
                },
            )
            .await;

        self.publish(RuntimeEvent::SessionLinked {
            from_session_id: origin.session_id.clone(),
            to_session_id: target_session_id.clone(),
            reason: SessionLinkReason::Send,
        });

        // Decide: target idle → deliver immediately; target busy → queue
        // on the mailbox and rely on the completion hook to drain.
        let target_busy = {
            let guard = self.active_sinks.lock().await;
            guard.contains_key(&target_session_id)
        };
        if target_busy {
            self.orchestration_state
                .enqueue_message(&target_session_id, &origin.session_id, message)
                .await;
        } else {
            let rc_for_turn = self.clone();
            let target = target_session_id.clone();
            crate::orchestration::spawn_peer_turn(rc_for_turn, target, message, "send_and_await");
        }

        let timeout = clamp_timeout(timeout_secs);
        let outcome = tokio::time::timeout(timeout, reply_rx).await;
        self.orchestration_state
            .unregister_await(&origin.session_id, &target_session_id)
            .await;

        match outcome {
            Ok(Ok((_turn_id, reply))) => Ok(RuntimeCallResult::Sent { reply: Some(reply) }),
            Ok(Err(_)) => Err(RuntimeCallError::Cancelled),
            Err(_) => Err(RuntimeCallError::Timeout {
                session_id: target_session_id,
            }),
        }
    }

    async fn dispatch_send(
        self: Arc<Self>,
        origin: RuntimeCallOrigin,
        target_session_id: String,
        message: String,
    ) -> Result<RuntimeCallResult, RuntimeCallError> {
        let _ = self
            .persistence
            .get_session(&target_session_id)
            .await
            .ok_or(RuntimeCallError::SessionNotFound {
                session_id: target_session_id.clone(),
            })?;
        self.publish(RuntimeEvent::SessionLinked {
            from_session_id: origin.session_id.clone(),
            to_session_id: target_session_id.clone(),
            reason: SessionLinkReason::Send,
        });

        let target_busy = {
            let guard = self.active_sinks.lock().await;
            guard.contains_key(&target_session_id)
        };
        if target_busy {
            self.orchestration_state
                .enqueue_message(&target_session_id, &origin.session_id, message)
                .await;
        } else {
            let rc_for_turn = self.clone();
            let target = target_session_id.clone();
            crate::orchestration::spawn_peer_turn(rc_for_turn, target, message, "send");
        }

        Ok(RuntimeCallResult::SentAsync)
    }

    async fn dispatch_poll(
        self: Arc<Self>,
        session_id: String,
        since_turn_id: Option<String>,
    ) -> Result<RuntimeCallResult, RuntimeCallError> {
        let detail = self
            .persistence
            .get_session(&session_id)
            .await
            .ok_or_else(|| RuntimeCallError::SessionNotFound {
                session_id: session_id.clone(),
            })?;

        // Find the most-recent completed turn after the cursor (or the
        // most recent overall if there's no cursor).
        let newest_after: Option<&TurnRecord> = detail
            .turns
            .iter()
            .rev()
            .filter(|t| t.status == TurnStatus::Completed)
            .find(|t| match &since_turn_id {
                Some(cursor) => t.turn_id != *cursor,
                None => true,
            });

        match newest_after {
            Some(turn) => Ok(poll_result_from_turn(&turn.turn_id, &turn.output)),
            None => Ok(RuntimeCallResult::Poll(PollOutcome::Pending)),
        }
    }

    async fn dispatch_read_session(
        self: Arc<Self>,
        session_id: String,
        last_turns: Option<u32>,
    ) -> Result<RuntimeCallResult, RuntimeCallError> {
        let detail = self
            .live_session_detail_limited(&session_id, last_turns.map(|n| n as usize))
            .await
            .ok_or_else(|| RuntimeCallError::SessionNotFound {
                session_id: session_id.clone(),
            })?;
        Ok(RuntimeCallResult::Session(detail))
    }

    async fn dispatch_list_sessions(
        self: Arc<Self>,
        origin: RuntimeCallOrigin,
        project_id: Option<String>,
    ) -> Result<RuntimeCallResult, RuntimeCallError> {
        let summaries = self.persistence.list_session_summaries().await;
        // Pre-load the projects table once so we can attach
        // `project_path` to each digest without O(n) lookups.
        let projects = self.persistence.list_projects().await;
        let project_paths: std::collections::HashMap<String, String> = projects
            .into_iter()
            .filter_map(|p| p.path.map(|path| (p.project_id, path)))
            .collect();

        let filtered: Vec<_> = summaries
            .into_iter()
            .filter(|s| s.session_id != origin.session_id)
            .filter(|s| match &project_id {
                Some(pid) => s.project_id.as_ref() == Some(pid),
                None => true,
            })
            .collect();

        let metadata = self.metadata_provider();
        let mut digests = Vec::with_capacity(filtered.len());
        for summary in filtered {
            // Cheap read: pull the first and last turn from persistence
            // and peek at their input/output. We cap `last_turns` at 1
            // for the head and reach for the newest completed turn for
            // the tail. Keeps the digest query lightweight even when
            // there are many sessions.
            let (first_input, last_output) =
                match self.persistence.get_session(&summary.session_id).await {
                    Some(detail) => {
                        let first = detail.turns.first().map(|t| t.input.clone());
                        let last = detail
                            .turns
                            .iter()
                            .rev()
                            .find(|t| t.status == TurnStatus::Completed)
                            .map(|t| t.output.clone());
                        (first, last)
                    }
                    None => (None, None),
                };
            let project_path = summary
                .project_id
                .as_ref()
                .and_then(|pid| project_paths.get(pid).cloned());

            // Resolve host-layer metadata (sidebar titles, project
            // names). Absent provider = None for both; agents fall
            // back to `firstInputPreview`.
            let (title, project_name) = match metadata.as_ref() {
                Some(m) => {
                    let title = m.session_title(&summary.session_id).await;
                    let project_name = match summary.project_id.as_ref() {
                        Some(pid) => m.project_name(pid).await,
                        None => None,
                    };
                    (title, project_name)
                }
                None => (None, None),
            };

            digests.push(zenui_provider_api::SessionDigest::from_parts(
                summary,
                title,
                project_name,
                project_path,
                first_input.as_deref(),
                last_output.as_deref(),
            ));
        }
        Ok(RuntimeCallResult::Sessions { sessions: digests })
    }

    async fn dispatch_list_projects(
        self: Arc<Self>,
    ) -> Result<RuntimeCallResult, RuntimeCallError> {
        let projects = self.persistence.list_projects().await;
        Ok(RuntimeCallResult::Projects { projects })
    }

    async fn dispatch_create_worktree(
        self: Arc<Self>,
        base_project_id: &str,
        branch: &str,
        base_ref: Option<&str>,
        create_branch: bool,
    ) -> Result<RuntimeCallResult, RuntimeCallError> {
        let provisioner =
            self.worktree_provisioner()
                .ok_or_else(|| RuntimeCallError::Internal {
                    message: "worktree support not available on this host".to_string(),
                })?;
        let bp = provisioner
            .create_worktree(base_project_id, branch, base_ref, create_branch)
            .await
            .map_err(|message| RuntimeCallError::Internal { message })?;
        Ok(RuntimeCallResult::Worktree(blueprint_to_summary(bp)))
    }

    async fn dispatch_list_worktrees(
        self: Arc<Self>,
        base_project_id: Option<&str>,
    ) -> Result<RuntimeCallResult, RuntimeCallError> {
        let provisioner =
            self.worktree_provisioner()
                .ok_or_else(|| RuntimeCallError::Internal {
                    message: "worktree support not available on this host".to_string(),
                })?;
        let list = provisioner
            .list_worktrees(base_project_id)
            .await
            .map_err(|message| RuntimeCallError::Internal { message })?;
        Ok(RuntimeCallResult::Worktrees {
            worktrees: list.into_iter().map(blueprint_to_summary).collect(),
        })
    }

    #[allow(clippy::too_many_arguments)]
    async fn dispatch_spawn_in_worktree(
        self: Arc<Self>,
        origin: RuntimeCallOrigin,
        base_project_id: String,
        branch: String,
        base_ref: Option<String>,
        create_branch: bool,
        initial_message: String,
        provider: Option<ProviderKind>,
        model: Option<String>,
        await_reply: bool,
        timeout_secs: Option<u64>,
    ) -> Result<RuntimeCallResult, RuntimeCallError> {
        let provisioner =
            self.worktree_provisioner()
                .ok_or_else(|| RuntimeCallError::Internal {
                    message: "worktree support not available on this host".to_string(),
                })?;
        let bp = provisioner
            .create_worktree(
                &base_project_id,
                &branch,
                base_ref.as_deref(),
                create_branch,
            )
            .await
            .map_err(|message| RuntimeCallError::Internal { message })?;

        // Spawn a session in the new worktree's project. We route
        // through the existing spawn path (`start_session` + scheduled
        // `send_turn`) so permissions, events, and the SessionLinked
        // badge all behave the same as a bare `spawn`/`spawn_and_await`.
        let (resolved_provider, resolved_model) = self
            .clone()
            .resolve_spawn_defaults(&origin, provider, model)
            .await?;
        if !self.is_provider_enabled(resolved_provider) {
            return Err(RuntimeCallError::ProviderDisabled {
                provider: resolved_provider.label().to_string(),
            });
        }
        let new_session = self
            .start_session(
                resolved_provider,
                resolved_model,
                Some(bp.project_id.clone()),
            )
            .await
            .map_err(|e| RuntimeCallError::Internal { message: e })?;
        let new_sid = new_session.summary.session_id.clone();
        self.publish(RuntimeEvent::SessionLinked {
            from_session_id: origin.session_id.clone(),
            to_session_id: new_sid.clone(),
            reason: SessionLinkReason::Spawn,
        });

        let summary = blueprint_to_summary(bp);

        if await_reply {
            self.orchestration_state
                .register_await(&origin.session_id, &new_sid)
                .await?;
            let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
            self.orchestration_state
                .register_reply(
                    &new_sid,
                    PendingReply {
                        after_turn_id: None,
                        sender: reply_tx,
                    },
                )
                .await;

            crate::orchestration::spawn_peer_turn(
                self.clone(),
                new_sid.clone(),
                initial_message,
                "spawn_in_worktree",
            );

            let timeout = clamp_timeout(timeout_secs);
            let outcome = tokio::time::timeout(timeout, reply_rx).await;
            self.orchestration_state
                .unregister_await(&origin.session_id, &new_sid)
                .await;
            match outcome {
                Ok(Ok((_turn_id, reply))) => Ok(RuntimeCallResult::SpawnedInWorktree {
                    worktree: summary,
                    session_id: new_sid,
                    reply: Some(reply),
                }),
                Ok(Err(_)) => Err(RuntimeCallError::Cancelled),
                Err(_) => Err(RuntimeCallError::Timeout {
                    session_id: new_sid,
                }),
            }
        } else {
            crate::orchestration::spawn_peer_turn(
                self.clone(),
                new_sid.clone(),
                initial_message,
                "spawn_in_worktree",
            );
            Ok(RuntimeCallResult::SpawnedInWorktree {
                worktree: summary,
                session_id: new_sid,
                reply: None,
            })
        }
    }

    /// Persist per-session settings into
    /// `provider_state.metadata`. Sparse — only fields the caller
    /// explicitly passed are merged; absent fields keep their prior
    /// value. Settings take effect on the NEXT turn (reading happens
    /// inside the adapter's `execute_turn`); this call returns once
    /// persistence has flushed so a follow-up SendTurn always sees
    /// the new value.
    async fn update_session_settings(
        &self,
        session_id: String,
        compact_custom_instructions: Option<String>,
    ) -> Result<(), String> {
        let mut session = self
            .live_session_detail(&session_id)
            .await
            .ok_or_else(|| format!("Unknown session `{session_id}`."))?;

        // Bump updated_at so list views that sort on it surface the
        // change. We treat session settings as a session-touch event
        // even though no turn was issued.
        session.summary.updated_at = chrono::Utc::now().to_rfc3339();

        // Merge into the existing provider_state, preserving
        // native_thread_id and any other metadata fields. If no
        // provider_state exists yet (settings change before the first
        // turn), seed an empty one so the metadata write has a home.
        let mut state = session
            .provider_state
            .clone()
            .unwrap_or(ProviderSessionState {
                native_thread_id: None,
                metadata: None,
            });
        let mut metadata_obj = state
            .metadata
            .as_ref()
            .and_then(|v| v.as_object().cloned())
            .unwrap_or_default();
        if let Some(text) = compact_custom_instructions {
            // Trim here so persistence carries the canonical form;
            // reading code already collapses empty / whitespace
            // values to `None`. Storing `""` (empty string) is
            // intentional — it's the user's signal to clear the
            // setting and it round-trips cleanly through the JSON
            // blob.
            let trimmed = text.trim();
            metadata_obj.insert(
                "compactCustomInstructions".to_string(),
                serde_json::Value::String(trimmed.to_string()),
            );
        }
        state.metadata = if metadata_obj.is_empty() {
            None
        } else {
            Some(serde_json::Value::Object(metadata_obj))
        };
        session.provider_state = Some(state);

        self.persistence.upsert_session(session).await;
        Ok(())
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
        // Keep the bypass safety net's view of the current mode in
        // sync with any mid-turn toolbar toggle. Only affects the
        // current turn's drain loop — cleared by the RAII guard in
        // `send_turn` once the turn ends.
        if let Ok(mut live) = self.in_flight_permission_mode.write() {
            if live.contains_key(&session_id) {
                live.insert(session_id.clone(), mode);
            }
        }
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

    /// Atomic "steer" — cooperatively interrupt the in-flight turn,
    /// wait for the bridge/adapter to unwind it (i.e. for the existing
    /// `send_turn` to publish `TurnCompleted`), and then dispatch
    /// `input` as the next turn on the same session.
    ///
    /// Why this exists: the frontend used to implement steering as two
    /// back-to-back RPCs (`interrupt_turn` + `send_turn`). Under some
    /// timings the second RPC reached the Claude bridge before its
    /// pump had observed the post-interrupt `result`, so
    /// `bridge.sendPrompt` rejected with "Another turn is already in
    /// flight". Collapsing the sequence into a single daemon-side
    /// operation closes that window — the `send_turn` below only fires
    /// after the finish-turn exit path has published `TurnCompleted`,
    /// which means the bridge's `turnInProgress` flag has cleared.
    ///
    /// If no turn is in flight this degrades to a plain `send_turn`.
    /// A small timeout guards against an adapter that (bug or otherwise)
    /// never publishes completion — we fall through to the `send_turn`
    /// anyway and let it surface whatever error the bridge returns.
    async fn steer_turn(
        &self,
        session_id: String,
        input: String,
        images: Vec<ImageAttachment>,
        permission_mode: PermissionMode,
        reasoning_effort: Option<ReasoningEffort>,
    ) -> Result<String, String> {
        // Is there actually a turn in flight for this session right
        // now? Read the live snapshot map rather than inspecting
        // persistence, so we don't race against a turn that just
        // started but hasn't had its first event written back.
        let turn_in_flight = match self.in_flight_turns.read() {
            Ok(map) => map.contains_key(&session_id),
            Err(_) => false,
        };

        if turn_in_flight {
            // Arm the wakeup FIRST so we can't miss the notification
            // if `finish_turn` happens to publish between our
            // interrupt call and our `notified()` await. `Notify`
            // holds a permit for future waits when nobody is
            // waiting — but we explicitly need `notified()` to be
            // registered before the notifier fires. The
            // `notified()` future registers its waiter when polled,
            // so we construct it and pin it before calling
            // `interrupt_turn`. Even if the interrupt races ahead
            // and finish_turn publishes first, `notify_waiters()`
            // will still wake us on the next `finish_turn` exit if
            // one is in progress — see tokio Notify semantics.
            let notifier = {
                let mut guard = self.turn_finalized_notifiers.lock().await;
                guard
                    .entry(session_id.clone())
                    .or_insert_with(|| Arc::new(Notify::new()))
                    .clone()
            };
            let wait = notifier.notified();
            tokio::pin!(wait);
            // Kick the cooperative interrupt. If the session has
            // disappeared between the flight check and here,
            // `interrupt_turn` will surface that as an error and we
            // bail without ever reaching the send.
            self.interrupt_turn(session_id.clone()).await?;

            // Bounded wait. 10s is generous — the SDK's
            // post-interrupt unwind typically resolves in tens of
            // milliseconds (spike logs: ~4ms from `interrupt()` to
            // the error-result being emitted). The timeout exists
            // purely to avoid pinning a task forever if a provider
            // adapter is broken; on expiry we still fall through to
            // `send_turn`, which will surface the real problem.
            let _ = tokio::time::timeout(std::time::Duration::from_secs(10), &mut wait).await;
        }

        self.send_turn(session_id, input, images, permission_mode, reasoning_effort)
            .await
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
        // Drop the per-session steer-wakeup notifier. Any `steer_turn`
        // currently awaiting on it will be cancelled by its own session
        // lookup failing before it reaches the wait.
        self.turn_finalized_notifiers
            .lock()
            .await
            .remove(&session_id);

        self.publish(RuntimeEvent::SessionDeleted {
            session_id: session_id.clone(),
        });

        Ok(format!("Session {session_id} deleted."))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::session_ops::OrchestrationService;
    use async_trait::async_trait;
    use zenui_persistence::PersistenceService;
    use zenui_provider_api::{
        ClientMessage, ContentBlock, PermissionMode, ProviderAdapter, ProviderKind, ProviderStatus,
        ProviderStatusLevel, ProviderTurnEvent, ProviderTurnOutput, ReasoningEffort, RuntimeEvent,
        SessionDetail, ToolCallStatus, TurnEventSink, TurnStatus, UserInput,
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
                features: Default::default(),
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
                features: Default::default(),
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
                features: Default::default(),
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
            Arc::new(zenui_checkpoints::NoopCheckpointStore),
            None,
            "/tmp/zenui-test/threads".to_string(),
            "test-app".to_string(),
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
        mid_rx.await.expect("adapter should signal mid-turn");

        // While the turn is paused, live_session_detail must already
        // surface the in-flight turn with its still-pending tool call.
        // Persistence has nothing for this turn yet — turn_completed
        // hasn't fired — so without the in-flight tracker this would
        // return an empty turns vec.
        let live = runtime
            .live_session_detail(&session_id)
            .await
            .expect("session should exist");
        assert_eq!(
            live.turns.len(),
            1,
            "in-flight turn must appear in live detail"
        );
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
            Arc::new(zenui_checkpoints::NoopCheckpointStore),
            None,
            "/tmp/zenui-test/threads".to_string(),
            "test-app".to_string(),
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
            Arc::new(zenui_checkpoints::NoopCheckpointStore),
            None,
            "/tmp/zenui-test/threads".to_string(),
            "test-app".to_string(),
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
                features: Default::default(),
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
    ///
    /// Runs on a paused virtual clock so the adapter's deliberate 100ms sleep
    /// auto-advances and the test costs near-zero real time.
    #[tokio::test(start_paused = true)]
    async fn turn_completes_without_subscribers() {
        let runtime = RuntimeCore::new(
            vec![Arc::new(SlowFakeAdapter)],
            Arc::new(OrchestrationService::new()),
            Arc::new(PersistenceService::in_memory().expect("in-memory db should initialize")),
            Arc::new(zenui_checkpoints::NoopCheckpointStore),
            None,
            "/tmp/zenui-test/threads".to_string(),
            "test-app".to_string(),
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
                features: Default::default(),
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
                InterruptOutcome::Err => Err("adapter stdout closed during interrupt".to_string()),
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
                features: Default::default(),
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
            Arc::new(zenui_checkpoints::NoopCheckpointStore),
            None,
            "/tmp/zenui-test/threads".to_string(),
            "test-app".to_string(),
        ));

        // Codex defaults to disabled since the "default only Claude and
        // GitHub Copilot to enabled" change. `send_turn` rejects disabled
        // providers before dispatching to the adapter, which would leave
        // `mid_turn_tx` unfired and the test hanging on `mid_rx.await`.
        // Flip it on for the test runtime so the adapter actually runs.
        runtime
            .set_provider_enabled(ProviderKind::Codex, true)
            .await;

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

    /// What the adapter returns from `execute_turn` after being released
    /// mid-interrupt. Parallels `InterruptOutcome` but adds the
    /// `OkWithState` variant so we can assert the `(true, Ok(output))`
    /// branch's provider_state update semantics from a test.
    enum InterruptStateOutcome {
        Err,
        OkNoState,
        OkWithState(zenui_provider_api::ProviderSessionState),
    }

    /// Same shape as `InterruptingAdapter` but accepts an arbitrary
    /// `InterruptStateOutcome`. Kept separate so adding the new variant
    /// can't regress the existing interrupt tests.
    struct InterruptStateAdapter {
        mid_turn_tx: tokio::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
        resume_rx: tokio::sync::Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
        outcome: tokio::sync::Mutex<Option<InterruptStateOutcome>>,
    }

    #[async_trait]
    impl ProviderAdapter for InterruptStateAdapter {
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
                features: Default::default(),
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
                    delta: "partial".to_string(),
                })
                .await;
            tokio::task::yield_now().await;
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;

            if let Some(tx) = self.mid_turn_tx.lock().await.take() {
                let _ = tx.send(());
            }
            if let Some(rx) = self.resume_rx.lock().await.take() {
                let _ = rx.await;
            }

            let outcome = self
                .outcome
                .lock()
                .await
                .take()
                .expect("outcome must be configured");
            match outcome {
                InterruptStateOutcome::Err => Err("adapter stdout closed".to_string()),
                InterruptStateOutcome::OkNoState => Ok(ProviderTurnOutput {
                    output: "[interrupted]".to_string(),
                    provider_state: None,
                }),
                InterruptStateOutcome::OkWithState(state) => Ok(ProviderTurnOutput {
                    output: "[interrupted]".to_string(),
                    provider_state: Some(state),
                }),
            }
        }
    }

    /// Create a session, seed its `provider_state` via the persistence
    /// service, then run one turn that gets interrupted mid-stream.
    /// Returns the session as persisted AFTER the interrupted turn
    /// finalises, so tests can assert on `provider_state` survival.
    async fn run_interrupt_with_seeded_state(
        seed: Option<zenui_provider_api::ProviderSessionState>,
        outcome: InterruptStateOutcome,
    ) -> SessionDetail {
        let (mid_tx, mid_rx) = tokio::sync::oneshot::channel();
        let (resume_tx, resume_rx) = tokio::sync::oneshot::channel();
        let adapter = Arc::new(InterruptStateAdapter {
            mid_turn_tx: tokio::sync::Mutex::new(Some(mid_tx)),
            resume_rx: tokio::sync::Mutex::new(Some(resume_rx)),
            outcome: tokio::sync::Mutex::new(Some(outcome)),
        });
        let runtime = Arc::new(RuntimeCore::new(
            vec![adapter],
            Arc::new(OrchestrationService::new()),
            Arc::new(PersistenceService::in_memory().expect("in-memory db should initialize")),
            Arc::new(zenui_checkpoints::NoopCheckpointStore),
            None,
            "/tmp/zenui-test/threads".to_string(),
            "test-app".to_string(),
        ));

        // Codex defaults to disabled since the "default only Claude and
        // GitHub Copilot to enabled" change. `send_turn` rejects disabled
        // providers before dispatching to the adapter, which would leave
        // `mid_turn_tx` unfired and the test hanging on `mid_rx.await`.
        // Flip it on for the test runtime so the adapter actually runs.
        runtime
            .set_provider_enabled(ProviderKind::Codex, true)
            .await;

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

        // Seed provider_state directly through the persistence service,
        // mimicking what a prior successfully-completed turn would have
        // written. This is the precondition for the steering bug: the
        // session enters the interrupted turn already carrying a
        // committed native_thread_id.
        if seed.is_some() {
            let mut detail = runtime
                .persistence
                .get_session(&session_id)
                .await
                .expect("seed setup: session detail must exist");
            detail.provider_state = seed;
            runtime.persistence.upsert_session(detail).await;
        }

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

        runtime
            .handle_client_message(ClientMessage::InterruptTurn {
                session_id: session_id.clone(),
            })
            .await;

        let _ = resume_tx.send(());
        turn_task.await.expect("turn task should complete");

        runtime
            .persistence
            .get_session(&session_id)
            .await
            .expect("session must persist after interrupted turn")
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

    /// Regression test for the steering-mode context-loss bug.
    ///
    /// If a session had a committed `provider_state` from a prior
    /// successful turn, an interrupted turn that surfaces as `Err` must
    /// NOT clobber or clear it. Downstream, the provider adapter will
    /// use `provider_state.native_thread_id` to resume the SDK
    /// conversation on the next (steered) turn — losing it here is
    /// exactly the context-loss bug.
    #[tokio::test]
    async fn interrupt_err_preserves_seeded_provider_state() {
        use zenui_provider_api::ProviderSessionState;
        let seeded = ProviderSessionState {
            native_thread_id: Some("sdk-session-ABC".to_string()),
            metadata: None,
        };
        let persisted =
            run_interrupt_with_seeded_state(Some(seeded.clone()), InterruptStateOutcome::Err).await;
        let got = persisted
            .provider_state
            .expect("seeded provider_state must survive Err interrupt");
        assert_eq!(
            got.native_thread_id, seeded.native_thread_id,
            "interrupt-Err path must not clobber committed native_thread_id"
        );
    }

    /// Same invariant for the `Ok(provider_state=None)` interrupt path
    /// (claude-sdk surfaces interrupts as `Ok("[interrupted]")` with
    /// no new provider_state). The seeded id must survive because the
    /// runtime only overwrites on `Some(_)`.
    #[tokio::test]
    async fn interrupt_ok_without_new_state_preserves_seeded_provider_state() {
        use zenui_provider_api::ProviderSessionState;
        let seeded = ProviderSessionState {
            native_thread_id: Some("sdk-session-DEF".to_string()),
            metadata: None,
        };
        let persisted =
            run_interrupt_with_seeded_state(Some(seeded.clone()), InterruptStateOutcome::OkNoState)
                .await;
        let got = persisted
            .provider_state
            .expect("seeded provider_state must survive Ok-no-state interrupt");
        assert_eq!(
            got.native_thread_id, seeded.native_thread_id,
            "interrupt-Ok-no-state path must not clobber committed native_thread_id"
        );
    }

    /// Forward-path semantics: when the adapter explicitly provides a
    /// fresh `provider_state` on an Ok-interrupt (e.g. the bridge's
    /// two-phase scheme promoted a COMPLETED turn's id before the
    /// abort landed), the runtime replaces the prior value. Documents
    /// the non-regression side of the invariant.
    #[tokio::test]
    async fn interrupt_ok_with_new_state_replaces_seeded_provider_state() {
        use zenui_provider_api::ProviderSessionState;
        let seeded = ProviderSessionState {
            native_thread_id: Some("sdk-session-OLD".to_string()),
            metadata: None,
        };
        let refreshed = ProviderSessionState {
            native_thread_id: Some("sdk-session-NEW".to_string()),
            metadata: None,
        };
        let persisted = run_interrupt_with_seeded_state(
            Some(seeded),
            InterruptStateOutcome::OkWithState(refreshed.clone()),
        )
        .await;
        let got = persisted
            .provider_state
            .expect("provider_state must be present after Ok-with-state");
        assert_eq!(
            got.native_thread_id, refreshed.native_thread_id,
            "Ok-with-state must update native_thread_id to the adapter-supplied value"
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
            Arc::new(zenui_checkpoints::NoopCheckpointStore),
            None,
            "/tmp/zenui-test/threads".to_string(),
            "test-app".to_string(),
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
            Arc::new(zenui_checkpoints::NoopCheckpointStore),
            None,
            "/tmp/zenui-test/threads".to_string(),
            "test-app".to_string(),
        ));

        // Codex defaults to disabled since the "default only Claude and
        // GitHub Copilot to enabled" change; `send_turn` would reject
        // the turn before producing a TurnRecord, making the later
        // `turns.last()` assertion fail. Enable it for the test runtime.
        runtime
            .set_provider_enabled(ProviderKind::Codex, true)
            .await;

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
            Arc::new(zenui_checkpoints::NoopCheckpointStore),
            None,
            "/tmp/zenui-test/threads".to_string(),
            "test-app".to_string(),
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
    fn fake_status_with_models(models: Vec<zenui_provider_api::ProviderModel>) -> ProviderStatus {
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
            features: Default::default(),
        }
    }

    fn model(value: &str, label: &str) -> zenui_provider_api::ProviderModel {
        zenui_provider_api::ProviderModel {
            value: value.to_string(),
            label: label.to_string(),
            ..Default::default()
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
            .set_cached_models(ProviderKind::Codex, &[model("fresh-model", "Fresh")])
            .await;

        // Simulate an app restart: brand-new RuntimeCore against the
        // same persistence. FakeAdapter::health() returns empty models,
        // so any non-empty models vec in the payload must come from
        // the cache merge.
        let runtime = Arc::new(RuntimeCore::new(
            vec![Arc::new(FakeAdapter)],
            Arc::new(OrchestrationService::new()),
            persistence.clone(),
            Arc::new(zenui_checkpoints::NoopCheckpointStore),
            None,
            "/tmp/zenui-test/threads".to_string(),
            "test-app".to_string(),
        ));

        let payload = runtime.bootstrap(Some("ws://test".to_string())).await;
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
            Arc::new(zenui_checkpoints::NoopCheckpointStore),
            None,
            "/tmp/zenui-test/threads".to_string(),
            "test-app".to_string(),
        ));

        let payload = runtime.bootstrap(Some("ws://test".to_string())).await;
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
            .set_cached_models_at(ProviderKind::Codex, &[model("old-model", "Old")], &stale)
            .await;

        let adapter = Arc::new(ModelFetchCountingAdapter {
            fetch_count: std::sync::atomic::AtomicUsize::new(0),
        });
        let runtime = Arc::new(RuntimeCore::new(
            vec![adapter.clone()],
            Arc::new(OrchestrationService::new()),
            persistence.clone(),
            Arc::new(zenui_checkpoints::NoopCheckpointStore),
            None,
            "/tmp/zenui-test/threads".to_string(),
            "test-app".to_string(),
        ));

        // Subscribe before bootstrapping so we don't miss the event.
        let mut events = runtime.subscribe();
        let payload = runtime.bootstrap(Some("ws://test".to_string())).await;

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
