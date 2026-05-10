//! Orchestrator tick loop — the heart of the orchestrator.
//!
//! Runs as a single tokio task spawned at daemon startup. On each
//! tick (default 10s, configurable, kickable from HTTP routes) it
//! scans actionable tasks and advances each by exactly one step.
//!
//! ## How a task moves
//!
//! Each task state has a transition rule that's either
//! **automated** (loop does it) or **gated** (loop waits for an
//! agent or a human):
//!
//! | from         | rule                                                            |
//! |--------------|-----------------------------------------------------------------|
//! | Open         | spawn triage agent if `AgentSpawner` available, else pass through |
//! | Triage       | poll triage agent for marker; on success, set project + advance |
//! | Ready        | seed project memory if absent, then spawn coder agent           |
//! | Code         | poll coder; on `<<<TASK_DONE>>>`, advance to AgentReview        |
//! | AgentReview  | spawn reviewer; on approval, → HumanReview; on changes, → Code  |
//! | HumanReview  | (human action only — loop ignores)                              |
//! | Merge        | run real `git merge`; success → Done, conflict → NeedsHuman     |
//! | terminals    | skipped                                                         |
//!
//! ## AgentSpawner is optional
//!
//! When the spawner is wired (Tauri shell), every "spawn ..."
//! step calls `RuntimeCore::handle_client_message(StartSession,
//! SendTurn)` and records the session in `task_sessions`. The
//! polling step calls `runtime.live_session_detail(sid)` and
//! parses the latest assistant turn for the marker.
//!
//! When the spawner is **None** (unit tests, headless dev), the
//! loop falls back to a synthesized advance with a system comment
//! tracing the transition. This keeps the state machine testable
//! without booting the full provider stack.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Notify, watch};
use tracing::{debug, error, info, warn};

use super::agents::{AgentSpawner, SessionPoll, payload};
use super::http::OrchestratorTickKick;
use super::merge::{MergeError, MergeOutcome, cleanup_worktree, merge_task};
use super::model::{CommentAuthor, SessionRole, Task, TaskState};
use super::service::{WorkerSignal, parse_worker_marker, validate_transition};
use super::store::KanbanStore;

/// Async resolver: project_id → absolute path. Async because the
/// runtime's `snapshot()` is async, and we don't want to wedge
/// the tick task with `block_on` against its own runtime.
pub type ProjectPathResolver = Arc<
    dyn Fn(String) -> Pin<Box<dyn Future<Output = Option<std::path::PathBuf>> + Send>>
        + Send
        + Sync,
>;

/// Hard floor on how often the loop ticks even when the toggle is
/// ON. The HTTP endpoint that sets `tick_interval_ms` already
/// validates [1000, 300000]; this is just defense-in-depth so a
/// corrupted SQLite row can't pin the loop into a hot busy-wait.
const MIN_TICK_INTERVAL_MS: u64 = 1_000;

/// Cheap-to-clone handle the HTTP layer holds. Implements the
/// `OrchestratorTickKick` trait declared in `kanban::http` so the
/// router can poke the loop without depending on this module.
#[derive(Clone)]
pub struct TickHandle {
    enabled_tx: Arc<watch::Sender<bool>>,
    notify: Arc<Notify>,
}

impl TickHandle {
    pub fn enabled(&self) -> bool {
        *self.enabled_tx.borrow()
    }
    pub fn set_enabled(&self, on: bool) {
        let _ = self.enabled_tx.send(on);
    }
    pub fn kick_now(&self) {
        self.notify.notify_one();
    }
}

impl OrchestratorTickKick for TickHandle {
    fn kick(&self) {
        self.kick_now();
    }
    fn set_enabled(&self, enabled: bool) {
        TickHandle::set_enabled(self, enabled);
    }
}

/// Spawn the tick loop. Returns the kick handle for the HTTP
/// routes to use. Task is detached; lifetime equals the daemon's.
///
/// `agent_spawner` is `Option<AgentSpawner>` so headless usages
/// (e.g. CLI smoke tests) can still drive the state machine
/// without booting providers.
pub fn spawn_tick_task(
    kanban: KanbanStore,
    find_project_path: ProjectPathResolver,
    agent_spawner: Option<AgentSpawner>,
) -> TickHandle {
    let initial_enabled = kanban.tick_enabled().unwrap_or(false);
    let (enabled_tx, enabled_rx) = watch::channel(initial_enabled);
    let enabled_tx = Arc::new(enabled_tx);
    let notify = Arc::new(Notify::new());

    let handle = TickHandle {
        enabled_tx: enabled_tx.clone(),
        notify: notify.clone(),
    };

    let kanban_for_task = kanban.clone();
    tokio::spawn(async move {
        run_loop(
            kanban_for_task,
            enabled_rx,
            notify,
            find_project_path,
            agent_spawner,
        )
        .await;
    });

    info!(initial_enabled, "orchestrator tick loop spawned");
    handle
}

async fn run_loop(
    kanban: KanbanStore,
    mut enabled_rx: watch::Receiver<bool>,
    notify: Arc<Notify>,
    find_project_path: ProjectPathResolver,
    agent_spawner: Option<AgentSpawner>,
) {
    info!(
        feature_enabled = kanban.feature_enabled().unwrap_or(false),
        tick_enabled = *enabled_rx.borrow(),
        "orchestrator tick loop running"
    );
    loop {
        // Park whenever the user-facing toggle is OFF. Only a
        // `watch::Sender::send` from `TickHandle::set_enabled`
        // unparks us — Notify pokes do not, by design, so a kick
        // while the toggle is off can't sneak the loop into
        // running.
        if !*enabled_rx.borrow() {
            debug!("tick loop parked (toggle OFF); waiting for set_enabled");
            if enabled_rx.changed().await.is_err() {
                info!("tick loop exiting: enabled channel closed");
                return;
            }
            continue;
        }
        // Feature flag is the upstream gate. We re-check every
        // iteration (cheap) so a flip from feature-on to
        // feature-off causes the loop to back off without needing
        // a separate channel.
        if !kanban.feature_enabled().unwrap_or(false) {
            debug!("tick loop holding (feature flag OFF); checking again in 2s");
            tokio::time::sleep(Duration::from_secs(2)).await;
            continue;
        }

        debug!("tick loop firing");
        if let Err(e) = tick_once(&kanban, &find_project_path, agent_spawner.as_ref()).await {
            warn!(error = %e, "tick errored; continuing");
        }

        let interval = kanban
            .tick_interval_ms()
            .unwrap_or(10_000)
            .max(MIN_TICK_INTERVAL_MS);
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(interval)) => {}
            _ = notify.notified() => debug!("tick loop kicked"),
            _ = enabled_rx.changed() => debug!("tick loop saw enabled change"),
        }
    }
}

async fn tick_once(
    kanban: &KanbanStore,
    find_project_path: &ProjectPathResolver,
    agent_spawner: Option<&AgentSpawner>,
) -> Result<(), String> {
    let tasks = kanban.list_actionable_tasks()?;
    if tasks.is_empty() {
        return Ok(());
    }
    debug!(count = tasks.len(), "tick: advancing actionable tasks");
    for task in tasks {
        let task_id = task.task_id.clone();
        if let Err(e) = advance_task(kanban, &task, find_project_path, agent_spawner).await {
            warn!(%task_id, error = %e, "advance_task failed");
            let _ = kanban.insert_comment(
                &task_id,
                CommentAuthor::System,
                &format!("tick: advance failed: {e}"),
            );
        }
    }
    Ok(())
}

async fn advance_task(
    kanban: &KanbanStore,
    task: &Task,
    find_project_path: &ProjectPathResolver,
    spawner: Option<&AgentSpawner>,
) -> Result<(), String> {
    use TaskState::*;
    match task.state {
        // ── Open: spawn triage if we have a spawner ─────────────
        Open => match spawner {
            Some(s) => spawn_triage_step(kanban, task, s).await,
            None => transition(
                kanban,
                task,
                Triage,
                "automated triage queued (no agent spawner wired)",
            )
            .await,
        },

        // ── Triage: poll triage session for marker ──────────────
        Triage => match (spawner, kanban.find_active_session(&task.task_id, SessionRole::Triage)?) {
            (Some(s), Some(sess)) => poll_triage_step(kanban, task, &sess.session_id, s).await,
            // No spawner OR no triage session row: heuristic fallback.
            // If the user manually tagged a project, advance.
            // Otherwise mark NeedsHuman.
            _ => triage_heuristic_fallback(kanban, task).await,
        },

        // ── Ready: seed memory (if absent) then spawn coder ─────
        // Both branches share the dependency + parallelism gates:
        // a Ready task with unresolved deps holds regardless of
        // whether a spawner is wired, and parallelism applies
        // even to the synthesized no-spawner state-machine demo.
        Ready => {
            if let Some(reason) = check_ready_gates(kanban, task)? {
                debug!(task_id = %task.task_id, %reason, "Ready held by gate");
                return Ok(());
            }
            match spawner {
                Some(s) => ready_step(kanban, task, s).await,
                None => transition(
                    kanban,
                    task,
                    Code,
                    "coding phase begun (no agent spawner wired)",
                )
                .await,
            }
        }

        // ── Code: poll coder for marker ─────────────────────────
        Code => match spawner {
            Some(s) => poll_coder_step(kanban, task, s).await,
            None => transition(
                kanban,
                task,
                AgentReview,
                "code phase complete (no agent spawner wired)",
            )
            .await,
        },

        // ── AgentReview: spawn reviewer if absent, else poll ────
        AgentReview => match spawner {
            Some(s) => agent_review_step(kanban, task, s).await,
            None => transition(
                kanban,
                task,
                HumanReview,
                "agent review complete (no agent spawner wired)",
            )
            .await,
        },

        // HumanReview is human-driven and not in the actionable
        // list; defensive return.
        HumanReview => Ok(()),

        // ── Merge: run real auto-merge ──────────────────────────
        Merge => merge_step(kanban, task, find_project_path, spawner).await,

        NeedsHuman | Done | Cancelled => Ok(()),
    }
}

// ── Open → Triage ──────────────────────────────────────────────────

async fn spawn_triage_step(
    kanban: &KanbanStore,
    task: &Task,
    spawner: &AgentSpawner,
) -> Result<(), String> {
    // Idempotency: don't double-spawn if a triage session already
    // exists. (Open state with an active triage session means the
    // last tick already kicked it off and this tick is racing.)
    if let Some(_existing) = kanban.find_active_session(&task.task_id, SessionRole::Triage)? {
        // Move to Triage state so the next tick polls it instead
        // of trying to spawn again.
        return transition(
            kanban,
            task,
            TaskState::Triage,
            "triage session already exists; advancing to poll",
        )
        .await;
    }
    match spawner.spawn_triage(task).await {
        Ok(session_id) => {
            kanban.insert_comment(
                &task.task_id,
                CommentAuthor::System,
                &format!("triage agent spawned (session {session_id})"),
            )?;
            // Move to Triage state so subsequent ticks poll the
            // session for its marker.
            transition(kanban, task, TaskState::Triage, "triage agent spawned").await
        }
        Err(e) => {
            mark_needs_human(kanban, task, &format!("triage spawn failed: {e}"))?;
            Ok(())
        }
    }
}

async fn poll_triage_step(
    kanban: &KanbanStore,
    task: &Task,
    session_id: &str,
    spawner: &AgentSpawner,
) -> Result<(), String> {
    let runtime_check = poll_session_safely(Some(spawner), session_id).await;
    let output = match runtime_check {
        SessionOutcome::Running => return Ok(()),
        SessionOutcome::Completed(text) => text,
        SessionOutcome::Failed(reason) => {
            kanban.retire_task_session(session_id)?;
            mark_needs_human(kanban, task, &format!("triage session failed: {reason}"))?;
            return Ok(());
        }
        SessionOutcome::Vanished => {
            kanban.retire_task_session(session_id)?;
            mark_needs_human(
                kanban,
                task,
                "triage session vanished; please retry by Resolving",
            )?;
            return Ok(());
        }
    };

    // Parse the marker. Triage embeds JSON in the TASK_DONE payload.
    match parse_worker_marker(&output) {
        WorkerSignal::Done { summary } => match payload::parse_triage(&summary) {
            Ok(payload::TriageDecision::Single { project_id, title }) => {
                kanban.retire_task_session(session_id)?;
                kanban.update_task(
                    &task.task_id,
                    Some(title.as_str()),
                    None,
                    Some(TaskState::Ready),
                    Some(Some(project_id.as_str())),
                    None,
                    None,
                    None,
                    None,
                )?;
                kanban.insert_comment(
                    &task.task_id,
                    CommentAuthor::System,
                    &format!("triage decided project={project_id} title={title}"),
                )?;
                info!(
                    task_id = %task.task_id,
                    %project_id,
                    "triage complete (single)"
                );
                Ok(())
            }
            Ok(payload::TriageDecision::Split { subtasks }) => {
                // Triage decided this is multiple distinct units of
                // work. Create N sibling tasks (Open, will go through
                // their own triage on next tick), wire dependencies
                // between them, and cancel the original umbrella task
                // — keeping it around as Cancelled with a comment so
                // the user can still find it as the historical entry
                // point.
                kanban.retire_task_session(session_id)?;
                let mut new_ids = Vec::with_capacity(subtasks.len());
                for sub in &subtasks {
                    let created = kanban.insert_task(&sub.title, &sub.body)?;
                    // Pre-tag project so the next tick can skip
                    // triage's project-decision step (its prompt
                    // could re-split, which we don't want — the
                    // umbrella already split). Move directly to
                    // Triage with project_id set; the heuristic
                    // fallback path then promotes to Ready.
                    kanban.update_task(
                        &created.task_id,
                        None,
                        None,
                        Some(TaskState::Triage),
                        Some(Some(sub.project_id.as_str())),
                        None,
                        None,
                        None,
                        None,
                    )?;
                    new_ids.push(created.task_id);
                }
                // Wire dependencies. `depends_on` is by index;
                // we resolve to actual task_ids using `new_ids`.
                for (i, sub) in subtasks.iter().enumerate() {
                    for &dep_idx in &sub.depends_on {
                        if let (Some(this), Some(blocker)) =
                            (new_ids.get(i), new_ids.get(dep_idx))
                        {
                            if let Err(e) = kanban.add_task_dependency(this, blocker) {
                                warn!(error = %e, "add_task_dependency failed");
                            }
                        }
                    }
                }
                // Mark the umbrella task as cancelled with a
                // pointer to the children. We don't pick "Done"
                // because no work was actually completed — the
                // original task was a wrapper around the real
                // work that's now elsewhere.
                kanban.update_task(
                    &task.task_id,
                    None,
                    None,
                    Some(TaskState::Cancelled),
                    None,
                    None,
                    None,
                    None,
                    None,
                )?;
                kanban.insert_comment(
                    &task.task_id,
                    CommentAuthor::System,
                    &format!(
                        "triage split into {} subtask(s): {}",
                        new_ids.len(),
                        new_ids.join(", ")
                    ),
                )?;
                info!(
                    task_id = %task.task_id,
                    subtask_ids = ?new_ids,
                    "triage complete (split)"
                );
                Ok(())
            }
            Err(e) => {
                kanban.retire_task_session(session_id)?;
                mark_needs_human(
                    kanban,
                    task,
                    &format!("triage marker payload invalid: {e}"),
                )?;
                Ok(())
            }
        },
        WorkerSignal::Blocked { reason } => {
            kanban.retire_task_session(session_id)?;
            mark_needs_human(kanban, task, &format!("triage blocked: {reason}"))?;
            Ok(())
        }
        WorkerSignal::NeedsInput { question } => {
            // Triage shouldn't need input from the orchestrator —
            // surface as NeedsHuman so the user can clarify.
            kanban.retire_task_session(session_id)?;
            mark_needs_human(
                kanban,
                task,
                &format!("triage needs clarification: {question}"),
            )?;
            Ok(())
        }
        WorkerSignal::None => {
            // Completed turn but no marker — common LLM mistake.
            // Treat as NeedsHuman with the raw output truncated so
            // the user can see what happened.
            kanban.retire_task_session(session_id)?;
            let snippet = truncate(&output, 400);
            mark_needs_human(
                kanban,
                task,
                &format!("triage produced no marker; raw output: {snippet}"),
            )?;
            Ok(())
        }
    }
}

async fn triage_heuristic_fallback(kanban: &KanbanStore, task: &Task) -> Result<(), String> {
    if task.project_id.is_some() {
        transition(
            kanban,
            task,
            TaskState::Ready,
            "user-tagged project; promoting to Ready",
        )
        .await
    } else {
        kanban.update_task(
            &task.task_id,
            None,
            None,
            Some(TaskState::NeedsHuman),
            None,
            None,
            None,
            None,
            Some(Some(
                "no agent spawner and no project tagged. Set a project on the task and Resolve.",
            )),
        )?;
        kanban.insert_comment(
            &task.task_id,
            CommentAuthor::System,
            "triage: no spawner, no project → NeedsHuman",
        )?;
        Ok(())
    }
}

// ── Ready: memory seed if absent, then spawn coder ────────────────

/// Inspect the gates that should hold a Ready task before any
/// further work is started. Returns `Some(reason)` to mean "hold
/// it" (caller logs reason and returns Ok), `None` to mean "OK
/// to proceed".
///
/// Two gates today:
/// - Unresolved task dependencies (any blocker not in
///   Done/Cancelled).
/// - Parallelism cap (active spawn slots vs `max_parallel_tasks`),
///   only applied when memory is already seeded — memory-seeding
///   itself is excluded so it can't deadlock the project.
fn check_ready_gates(kanban: &KanbanStore, task: &Task) -> Result<Option<String>, String> {
    let blockers = kanban.unresolved_dependencies(&task.task_id)?;
    if !blockers.is_empty() {
        // Post a one-time "waiting on" comment so the UI shows
        // the user why nothing is happening. Don't repost on every
        // tick — check the latest system comment first.
        let recent_already_blocked = kanban
            .list_comments(&task.task_id)?
            .into_iter()
            .rev()
            .find(|c| c.author == CommentAuthor::System)
            .map(|c| c.body.contains("waiting for dependencies"))
            .unwrap_or(false);
        if !recent_already_blocked {
            kanban.insert_comment(
                &task.task_id,
                CommentAuthor::System,
                &format!(
                    "waiting for dependencies to complete: {}",
                    blockers.join(", ")
                ),
            )?;
        }
        return Ok(Some(format!(
            "blocked by {} unresolved dep(s)",
            blockers.len()
        )));
    }
    if let Some(project_id) = task.project_id.as_deref() {
        if kanban.get_project_memory(project_id)?.is_some() {
            let cap = kanban.max_parallel_tasks()?;
            let active = kanban.count_active_spawn_slots()?;
            if active >= cap {
                return Ok(Some(format!(
                    "parallelism cap reached ({active}/{cap})"
                )));
            }
        }
    }
    Ok(None)
}

async fn ready_step(
    kanban: &KanbanStore,
    task: &Task,
    spawner: &AgentSpawner,
) -> Result<(), String> {
    let project_id = match task.project_id.as_deref() {
        Some(p) => p,
        None => {
            mark_needs_human(kanban, task, "Ready state without project_id")?;
            return Ok(());
        }
    };
    // Gates already checked by the caller in `advance_task`.

    // First-time-project gate: seed memory before any code work.
    if kanban.get_project_memory(project_id)?.is_none() {
        // Is the seeder already running for this task?
        if let Some(sess) =
            kanban.find_active_session(&task.task_id, SessionRole::MemorySeeder)?
        {
            return poll_memory_seeder_step(kanban, task, &sess.session_id, spawner).await;
        }
        // Spawn the seeder. Resolve project path first.
        let snapshot = spawner.runtime_snapshot_projects().await;
        let project_path = snapshot
            .into_iter()
            .find(|(id, _)| id == project_id)
            .map(|(_, path)| path);
        let project_path = match project_path {
            Some(p) => p,
            None => {
                mark_needs_human(
                    kanban,
                    task,
                    &format!("project {project_id} not found in flowstate snapshot"),
                )?;
                return Ok(());
            }
        };
        match spawner
            .spawn_memory_seeder(task, project_id, &project_path)
            .await
        {
            Ok(sid) => {
                kanban.insert_comment(
                    &task.task_id,
                    CommentAuthor::System,
                    &format!("memory_seeder spawned (session {sid})"),
                )?;
            }
            Err(e) => {
                mark_needs_human(kanban, task, &format!("memory_seeder spawn failed: {e}"))?;
            }
        }
        return Ok(());
    }

    // Memory present — spawn coder. Avoid double-spawning.
    if kanban
        .find_active_session(&task.task_id, SessionRole::Coder)?
        .is_some()
    {
        return transition(
            kanban,
            task,
            TaskState::Code,
            "coder session already exists; advancing to poll",
        )
        .await;
    }
    let memory = kanban.get_project_memory(project_id)?;
    // First-time coder spawn: no prior reviewer feedback yet.
    match spawner.spawn_coder(task, memory.as_ref(), None).await {
        Ok(sid) => {
            kanban.insert_comment(
                &task.task_id,
                CommentAuthor::System,
                &format!("coder spawned (session {sid})"),
            )?;
            transition(kanban, task, TaskState::Code, "coder agent spawned").await
        }
        Err(e) => {
            mark_needs_human(kanban, task, &format!("coder spawn failed: {e}"))?;
            Ok(())
        }
    }
}

async fn poll_memory_seeder_step(
    kanban: &KanbanStore,
    task: &Task,
    session_id: &str,
    spawner: &AgentSpawner,
) -> Result<(), String> {
    match poll_session_safely(Some(spawner), session_id).await {
        SessionOutcome::Running => Ok(()),
        SessionOutcome::Completed(text) => match parse_worker_marker(&text) {
            WorkerSignal::Done { summary } => match payload::parse_memory(&summary) {
                Ok(blob) => {
                    let project_id = task.project_id.as_deref().unwrap_or("");
                    let memory = payload::into_memory(project_id, blob);
                    kanban.upsert_project_memory(&memory)?;
                    kanban.retire_task_session(session_id)?;
                    kanban.insert_comment(
                        &task.task_id,
                        CommentAuthor::System,
                        "memory seeded; coder will spawn next tick",
                    )?;
                    // Don't transition — next tick re-enters
                    // ready_step which finds memory and spawns coder.
                    Ok(())
                }
                Err(e) => {
                    kanban.retire_task_session(session_id)?;
                    mark_needs_human(
                        kanban,
                        task,
                        &format!("memory_seeder produced invalid JSON: {e}"),
                    )?;
                    Ok(())
                }
            },
            other => {
                kanban.retire_task_session(session_id)?;
                mark_needs_human(
                    kanban,
                    task,
                    &format!("memory_seeder did not finish cleanly: {other:?}"),
                )?;
                Ok(())
            }
        },
        SessionOutcome::Failed(r) => {
            kanban.retire_task_session(session_id)?;
            mark_needs_human(kanban, task, &format!("memory_seeder failed: {r}"))?;
            Ok(())
        }
        SessionOutcome::Vanished => {
            kanban.retire_task_session(session_id)?;
            mark_needs_human(kanban, task, "memory_seeder session vanished")?;
            Ok(())
        }
    }
}

// ── Code: poll coder ──────────────────────────────────────────────

async fn poll_coder_step(
    kanban: &KanbanStore,
    task: &Task,
    spawner: &AgentSpawner,
) -> Result<(), String> {
    let sess = match kanban.find_active_session(&task.task_id, SessionRole::Coder)? {
        Some(s) => s,
        None => {
            // No active coder. This happens after a reviewer
            // bounced the task with "changes_requested" — we
            // transitioned AgentReview → Code, retired the prior
            // coder, and now need a fresh one. We CAN'T transition
            // back to Ready (the FSM disallows Code → Ready, and
            // even if it didn't, that would lose the in-flight
            // worktree/branch metadata once we add it). So spawn
            // a fresh coder directly here.
            let project_id = match task.project_id.as_deref() {
                Some(p) => p,
                None => {
                    mark_needs_human(kanban, task, "Code state without project_id")?;
                    return Ok(());
                }
            };
            let memory = kanban.get_project_memory(project_id)?;
            // Pull the most recent reviewer comment as feedback so
            // the new coder knows what to fix. Comments are ordered
            // by created_at ASC; the rev() finds the latest.
            let revision_feedback = kanban
                .list_comments(&task.task_id)?
                .into_iter()
                .rev()
                .find(|c| c.author == CommentAuthor::Reviewer)
                .map(|c| c.body);
            match spawner
                .spawn_coder(task, memory.as_ref(), revision_feedback.as_deref())
                .await
            {
                Ok(sid) => {
                    let note = if revision_feedback.is_some() {
                        format!("respawned coder with reviewer feedback (session {sid})")
                    } else {
                        format!("respawned coder (session {sid})")
                    };
                    kanban.insert_comment(&task.task_id, CommentAuthor::System, &note)?;
                    return Ok(());
                }
                Err(e) => {
                    mark_needs_human(kanban, task, &format!("coder respawn failed: {e}"))?;
                    return Ok(());
                }
            }
        }
    };
    match poll_session_safely(Some(spawner), &sess.session_id).await {
        SessionOutcome::Running => Ok(()),
        SessionOutcome::Failed(r) => {
            kanban.retire_task_session(&sess.session_id)?;
            mark_needs_human(kanban, task, &format!("coder failed: {r}"))?;
            Ok(())
        }
        SessionOutcome::Vanished => {
            kanban.retire_task_session(&sess.session_id)?;
            mark_needs_human(kanban, task, "coder session vanished")?;
            Ok(())
        }
        SessionOutcome::Completed(text) => match parse_worker_marker(&text) {
            WorkerSignal::Done { summary } => {
                kanban.retire_task_session(&sess.session_id)?;
                kanban.insert_comment(
                    &task.task_id,
                    CommentAuthor::Coder,
                    &format!("coder done: {summary}"),
                )?;
                transition(kanban, task, TaskState::AgentReview, &format!("coder: {summary}")).await
            }
            WorkerSignal::Blocked { reason } => {
                kanban.retire_task_session(&sess.session_id)?;
                mark_needs_human(kanban, task, &format!("coder blocked: {reason}"))?;
                Ok(())
            }
            WorkerSignal::NeedsInput { question } => {
                // The orchestrator (this loop) doesn't have a way
                // to answer the coder mid-conversation in v2 — the
                // session was started as a one-shot. Surface as
                // NeedsHuman; in v3 (per-task orchestrator) the
                // orchestrator session will field these.
                kanban.retire_task_session(&sess.session_id)?;
                mark_needs_human(
                    kanban,
                    task,
                    &format!("coder needs clarification: {question}"),
                )?;
                Ok(())
            }
            WorkerSignal::None => {
                kanban.retire_task_session(&sess.session_id)?;
                let snippet = truncate(&text, 400);
                mark_needs_human(
                    kanban,
                    task,
                    &format!("coder produced no marker; raw output: {snippet}"),
                )?;
                Ok(())
            }
        },
    }
}

// ── AgentReview: spawn reviewer or poll ───────────────────────────

async fn agent_review_step(
    kanban: &KanbanStore,
    task: &Task,
    spawner: &AgentSpawner,
) -> Result<(), String> {
    let active = kanban.find_active_session(&task.task_id, SessionRole::Reviewer)?;
    match active {
        None => {
            // Spawn a reviewer. We don't have the coder summary
            // any more (it was retired), so we read the most
            // recent coder comment to feed back as context.
            let coder_summary = kanban
                .list_comments(&task.task_id)?
                .into_iter()
                .rev()
                .find(|c| c.author == CommentAuthor::Coder)
                .map(|c| c.body)
                .unwrap_or_else(|| "(no coder summary on record)".to_string());
            match spawner.spawn_reviewer(task, &coder_summary).await {
                Ok(sid) => {
                    kanban.insert_comment(
                        &task.task_id,
                        CommentAuthor::System,
                        &format!("reviewer spawned (session {sid})"),
                    )?;
                    Ok(())
                }
                Err(e) => {
                    mark_needs_human(kanban, task, &format!("reviewer spawn failed: {e}"))?;
                    Ok(())
                }
            }
        }
        Some(sess) => match poll_session_safely(Some(spawner), &sess.session_id).await {
            SessionOutcome::Running => Ok(()),
            SessionOutcome::Failed(r) => {
                kanban.retire_task_session(&sess.session_id)?;
                mark_needs_human(kanban, task, &format!("reviewer failed: {r}"))?;
                Ok(())
            }
            SessionOutcome::Vanished => {
                kanban.retire_task_session(&sess.session_id)?;
                mark_needs_human(kanban, task, "reviewer session vanished")?;
                Ok(())
            }
            SessionOutcome::Completed(text) => match parse_worker_marker(&text) {
                WorkerSignal::Done { summary } => {
                    kanban.retire_task_session(&sess.session_id)?;
                    let verdict = payload::parse_review_verdict(&summary);
                    match verdict {
                        payload::ReviewVerdict::Approved { rationale } => {
                            kanban.insert_comment(
                                &task.task_id,
                                CommentAuthor::Reviewer,
                                &format!("approved: {rationale}"),
                            )?;
                            transition(
                                kanban,
                                task,
                                TaskState::HumanReview,
                                "reviewer approved",
                            )
                            .await
                        }
                        payload::ReviewVerdict::ChangesRequested { rationale } => {
                            kanban.insert_comment(
                                &task.task_id,
                                CommentAuthor::Reviewer,
                                &format!("changes requested: {rationale}"),
                            )?;
                            // Send back to Code so the next tick
                            // re-enters ready/code path and a fresh
                            // coder is spawned with the rejection
                            // context in the most recent comments.
                            transition(
                                kanban,
                                task,
                                TaskState::Code,
                                "reviewer requested changes; respawning coder",
                            )
                            .await
                        }
                    }
                }
                WorkerSignal::Blocked { reason } => {
                    kanban.retire_task_session(&sess.session_id)?;
                    mark_needs_human(kanban, task, &format!("reviewer blocked: {reason}"))?;
                    Ok(())
                }
                WorkerSignal::NeedsInput { question } => {
                    kanban.retire_task_session(&sess.session_id)?;
                    mark_needs_human(
                        kanban,
                        task,
                        &format!("reviewer needs input: {question}"),
                    )?;
                    Ok(())
                }
                WorkerSignal::None => {
                    kanban.retire_task_session(&sess.session_id)?;
                    let snippet = truncate(&text, 400);
                    mark_needs_human(
                        kanban,
                        task,
                        &format!("reviewer produced no marker: {snippet}"),
                    )?;
                    Ok(())
                }
            },
        },
    }
}

// ── Merge: real auto-merge with optional memory updater fan-out ───

async fn merge_step(
    kanban: &KanbanStore,
    task: &Task,
    find_project_path: &ProjectPathResolver,
    spawner: Option<&AgentSpawner>,
) -> Result<(), String> {
    // No branch ⇒ v2 task with no real worktree (e.g. a coder
    // that edited the parent project directly). Mark Done and
    // optionally spin a memory_updater so the project's memory
    // captures what was learned.
    let branch = match task.branch.as_deref() {
        Some(b) if !b.is_empty() => b,
        _ => {
            kanban.update_task(
                &task.task_id,
                None,
                None,
                Some(TaskState::Done),
                None,
                None,
                None,
                None,
                None,
            )?;
            kanban.insert_comment(
                &task.task_id,
                CommentAuthor::System,
                "merge: no branch on task → marking Done (no real worktree)",
            )?;
            spawn_memory_updater_if_possible(kanban, task, spawner).await;
            return Ok(());
        }
    };

    let project_id = match task.project_id.as_deref() {
        Some(p) => p,
        None => {
            mark_needs_human(
                kanban,
                task,
                "merge requires project_id but task has none",
            )?;
            return Ok(());
        }
    };
    let parent_path = match find_project_path(project_id.to_string()).await {
        Some(p) => p,
        None => {
            mark_needs_human(
                kanban,
                task,
                &format!("project {project_id} no longer exists in flowstate"),
            )?;
            return Ok(());
        }
    };

    match merge_task(&parent_path, branch).await {
        Ok(MergeOutcome::Merged { sha }) => {
            kanban.insert_comment(
                &task.task_id,
                CommentAuthor::System,
                &format!("merged {branch} into parent: {sha}"),
            )?;
            if let Some(wt_id) = task.worktree_project_id.as_deref() {
                if let Some(wt_path) = find_project_path(wt_id.to_string()).await {
                    if let Err(e) = cleanup_worktree(&parent_path, &wt_path, branch).await {
                        kanban.insert_comment(
                            &task.task_id,
                            CommentAuthor::System,
                            &format!("worktree cleanup failed: {e}"),
                        )?;
                    }
                }
            }
            kanban.update_task(
                &task.task_id,
                None,
                None,
                Some(TaskState::Done),
                None,
                None,
                None,
                None,
                None,
            )?;
            spawn_memory_updater_if_possible(kanban, task, spawner).await;
            info!(task_id = %task.task_id, %sha, "task merged → Done");
            Ok(())
        }
        Ok(MergeOutcome::Conflict { files }) => {
            let reason = format!(
                "merge conflict on: {} — resolve manually, then Resolve",
                files.join(", ")
            );
            mark_needs_human(kanban, task, &reason)?;
            warn!(task_id = %task.task_id, ?files, "task hit merge conflict");
            Ok(())
        }
        Err(MergeError::MissingBranch | MergeError::MissingParentPath(_)) => {
            Err("merge precondition unexpectedly failed".to_string())
        }
        Err(e) => {
            mark_needs_human(kanban, task, &format!("merge failed: {e}"))?;
            error!(task_id = %task.task_id, error = %e, "merge errored");
            Ok(())
        }
    }
}

async fn spawn_memory_updater_if_possible(
    kanban: &KanbanStore,
    task: &Task,
    spawner: Option<&AgentSpawner>,
) {
    let Some(spawner) = spawner else {
        return;
    };
    let Some(project_id) = task.project_id.as_deref() else {
        return;
    };
    let memory = match kanban.get_project_memory(project_id) {
        Ok(Some(m)) => m,
        _ => return,
    };
    match spawner.spawn_memory_updater(task, &memory).await {
        Ok(sid) => {
            let _ = kanban.insert_comment(
                &task.task_id,
                CommentAuthor::System,
                &format!("memory_updater spawned (session {sid})"),
            );
            // The updater is fire-and-forget — its result is
            // applied by a tiny polling helper that runs
            // out-of-band on a separate one-shot task. We don't
            // block Done on it.
            let kanban = kanban.clone();
            let spawner = spawner.clone();
            let project_id = project_id.to_string();
            let session_id = sid;
            tokio::spawn(async move {
                drain_memory_updater(kanban, spawner, project_id, session_id).await;
            });
        }
        Err(e) => {
            let _ = kanban.insert_comment(
                &task.task_id,
                CommentAuthor::System,
                &format!("memory_updater spawn failed: {e}"),
            );
        }
    }
}

async fn drain_memory_updater(
    kanban: KanbanStore,
    spawner: AgentSpawner,
    project_id: String,
    session_id: String,
) {
    // Poll the updater session up to ~2 minutes for a result.
    // The task is already Done; this is best-effort memory
    // refinement. Failures are logged, not surfaced.
    for _ in 0..24 {
        match spawner.poll_session(&session_id).await {
            SessionPoll::StillRunning => {
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }
            SessionPoll::Completed { output } => {
                if let WorkerSignal::Done { summary } = parse_worker_marker(&output) {
                    if let Ok(blob) = payload::parse_memory(&summary) {
                        let memory = payload::into_memory(&project_id, blob);
                        if let Err(e) = kanban.upsert_project_memory(&memory) {
                            warn!(error = %e, "memory_updater upsert failed");
                        } else {
                            info!(%project_id, "memory updated");
                        }
                    }
                }
                let _ = kanban.retire_task_session(&session_id);
                return;
            }
            SessionPoll::Failed { reason } => {
                warn!(%reason, "memory_updater session failed");
                let _ = kanban.retire_task_session(&session_id);
                return;
            }
            SessionPoll::Vanished => {
                let _ = kanban.retire_task_session(&session_id);
                return;
            }
        }
    }
    warn!(%session_id, "memory_updater drain timed out");
    let _ = kanban.retire_task_session(&session_id);
}

// ── helpers ────────────────────────────────────────────────────────

/// Lightweight SessionPoll variant that doesn't require the
/// `AgentSpawner` to be present (so the loop can degrade
/// gracefully when there's no spawner wired). When spawner is
/// `None`, treats every poll as "still running" — but the
/// upstream `advance_task` only enters polling branches when a
/// spawner exists, so this is just a defensive shape.
enum SessionOutcome {
    Running,
    Completed(String),
    Failed(String),
    Vanished,
}

async fn poll_session_safely(
    spawner: Option<&AgentSpawner>,
    session_id: &str,
) -> SessionOutcome {
    match spawner {
        None => SessionOutcome::Running,
        Some(s) => match s.poll_session(session_id).await {
            SessionPoll::StillRunning => SessionOutcome::Running,
            SessionPoll::Completed { output } => SessionOutcome::Completed(output),
            SessionPoll::Failed { reason } => SessionOutcome::Failed(reason),
            SessionPoll::Vanished => SessionOutcome::Vanished,
        },
    }
}

async fn transition(
    kanban: &KanbanStore,
    task: &Task,
    next: TaskState,
    why: &str,
) -> Result<(), String> {
    if let Err(e) = validate_transition(task.state, next) {
        return Err(format!("illegal transition: {e}"));
    }
    kanban.update_task(
        &task.task_id,
        None,
        None,
        Some(next),
        None,
        None,
        None,
        None,
        None,
    )?;
    kanban.insert_comment(
        &task.task_id,
        CommentAuthor::System,
        &format!("{} → {}: {}", task.state.as_str(), next.as_str(), why),
    )?;
    info!(
        task_id = %task.task_id,
        from = %task.state.as_str(),
        to = %next.as_str(),
        "task transitioned"
    );
    Ok(())
}

fn mark_needs_human(kanban: &KanbanStore, task: &Task, reason: &str) -> Result<(), String> {
    kanban.update_task(
        &task.task_id,
        None,
        None,
        Some(TaskState::NeedsHuman),
        None,
        None,
        None,
        None,
        Some(Some(reason)),
    )?;
    kanban.insert_comment(
        &task.task_id,
        CommentAuthor::System,
        &format!("→ NeedsHuman: {reason}"),
    )?;
    Ok(())
}

fn truncate(s: &str, n: usize) -> String {
    let trimmed = s.trim();
    if trimmed.chars().count() <= n {
        trimmed.to_string()
    } else {
        let prefix: String = trimmed.chars().take(n).collect();
        format!("{prefix}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_projects() -> ProjectPathResolver {
        Arc::new(|_| Box::pin(async { None }))
    }

    #[tokio::test]
    async fn open_advances_to_triage_when_no_spawner() {
        let kanban = KanbanStore::in_memory().unwrap();
        let t = kanban.insert_task("title", "body").unwrap();
        tick_once(&kanban, &no_projects(), None).await.unwrap();
        let got = kanban.get_task(&t.task_id).unwrap().unwrap();
        assert_eq!(got.state, TaskState::Triage);
    }

    #[tokio::test]
    async fn triage_without_project_or_spawner_goes_to_needs_human() {
        let kanban = KanbanStore::in_memory().unwrap();
        let t = kanban.insert_task("title", "body").unwrap();
        kanban
            .update_task(
                &t.task_id,
                None,
                None,
                Some(TaskState::Triage),
                None,
                None,
                None,
                None,
                None,
            )
            .unwrap();
        tick_once(&kanban, &no_projects(), None).await.unwrap();
        let got = kanban.get_task(&t.task_id).unwrap().unwrap();
        assert_eq!(got.state, TaskState::NeedsHuman);
    }

    #[tokio::test]
    async fn triage_with_project_and_no_spawner_advances() {
        let kanban = KanbanStore::in_memory().unwrap();
        let t = kanban.insert_task("title", "body").unwrap();
        kanban
            .update_task(
                &t.task_id,
                None,
                None,
                Some(TaskState::Triage),
                Some(Some("proj_abc")),
                None,
                None,
                None,
                None,
            )
            .unwrap();
        tick_once(&kanban, &no_projects(), None).await.unwrap();
        let got = kanban.get_task(&t.task_id).unwrap().unwrap();
        assert_eq!(got.state, TaskState::Ready);
    }

    #[tokio::test]
    async fn merge_without_branch_goes_to_done() {
        let kanban = KanbanStore::in_memory().unwrap();
        let t = kanban.insert_task("title", "body").unwrap();
        kanban
            .update_task(
                &t.task_id,
                None,
                None,
                Some(TaskState::Merge),
                Some(Some("proj_abc")),
                None,
                None,
                None,
                None,
            )
            .unwrap();
        tick_once(&kanban, &no_projects(), None).await.unwrap();
        let got = kanban.get_task(&t.task_id).unwrap().unwrap();
        assert_eq!(got.state, TaskState::Done);
    }

    #[tokio::test]
    async fn toggle_unparks_a_running_loop() {
        // Regression test for the bug where flipping the loop
        // toggle persisted to SQLite but never unparked the loop —
        // the watch channel was the only signal that mattered, and
        // the HTTP path wasn't touching it. After the fix
        // `TickHandle::set_enabled` is what the HTTP layer must
        // call.
        use std::time::Duration;
        let kanban = KanbanStore::in_memory().unwrap();
        kanban
            .set_setting(crate::kanban::model::settings_keys::FEATURE_ENABLED, "true")
            .unwrap();
        let t = kanban.insert_task("title", "body").unwrap();

        // Spawn the real tick task with the toggle starting OFF.
        let handle = spawn_tick_task(
            kanban.clone(),
            no_projects(),
            None, // no agent spawner — synthesized transitions
        );
        // Give the spawned task a moment to park.
        tokio::time::sleep(Duration::from_millis(50)).await;

        // While parked, an Open task should NOT advance, even
        // though we kick — the bug was that kick-only didn't unpark.
        handle.kick_now();
        tokio::time::sleep(Duration::from_millis(100)).await;
        let still_open = kanban.get_task(&t.task_id).unwrap().unwrap();
        assert_eq!(still_open.state, TaskState::Open, "parked loop must not advance");

        // Now flip the toggle. This should unpark and the loop
        // should run at least one tick — under the no-spawner
        // fallback that advances Open → Triage on tick 1, and
        // Triage (no project_id) → NeedsHuman on tick 2. Either
        // landing state proves the loop is running; the bug
        // we're guarding against is "still Open forever".
        handle.set_enabled(true);
        // Give the loop time to wake and run at least one tick.
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let got = kanban.get_task(&t.task_id).unwrap().unwrap();
            if got.state != TaskState::Open {
                return; // success — loop ran
            }
        }
        panic!("loop never advanced task; still Open after toggle ON");
    }

    #[tokio::test]
    async fn ready_task_holds_when_dependency_unresolved() {
        // Ready task with an unresolved dep should hold (not
        // advance to Code) regardless of spawner. We seed a fake
        // memory row so `ready_step` doesn't try to spawn the
        // memory_seeder either — this isolates the dep gate.
        let kanban = KanbanStore::in_memory().unwrap();
        kanban
            .set_setting(crate::kanban::model::settings_keys::FEATURE_ENABLED, "true")
            .unwrap();
        let blocker = kanban.insert_task("blocker", "first").unwrap();
        let dependent = kanban.insert_task("dependent", "second").unwrap();
        // Memory present so memory-seeder isn't triggered.
        kanban
            .upsert_project_memory(&crate::kanban::model::ProjectMemory {
                project_id: "p1".into(),
                purpose: None,
                languages: vec![],
                key_directories: vec![],
                conventions: vec![],
                recent_task_themes: vec![],
                seeded_at: Some(1),
                updated_at: 1,
            })
            .unwrap();
        // Move dependent to Ready, blocker stays Open.
        kanban
            .update_task(
                &dependent.task_id,
                None,
                None,
                Some(TaskState::Ready),
                Some(Some("p1")),
                None,
                None,
                None,
                None,
            )
            .unwrap();
        kanban
            .add_task_dependency(&dependent.task_id, &blocker.task_id)
            .unwrap();

        // First tick. Dependent should NOT advance — its blocker
        // is still Open (not Done/Cancelled).
        tick_once(&kanban, &no_projects(), None).await.unwrap();
        let dep_after = kanban.get_task(&dependent.task_id).unwrap().unwrap();
        assert_eq!(
            dep_after.state,
            TaskState::Ready,
            "dep should hold at Ready while blocker is unresolved"
        );

        // Resolve the blocker (mark Done). Tick again. Dependent
        // advances.
        kanban
            .update_task(
                &blocker.task_id,
                None,
                None,
                Some(TaskState::Done),
                None,
                None,
                None,
                None,
                None,
            )
            .unwrap();
        tick_once(&kanban, &no_projects(), None).await.unwrap();
        let dep_after2 = kanban.get_task(&dependent.task_id).unwrap().unwrap();
        assert_eq!(
            dep_after2.state,
            TaskState::Code,
            "dep should advance once blocker resolved"
        );
    }

    #[tokio::test]
    async fn parallelism_cap_holds_excess_tasks_at_ready() {
        // With max_parallel_tasks=1 and one task already in Code,
        // a second task in Ready should NOT advance. Memory is
        // seeded so the memory-seeder branch is skipped.
        let kanban = KanbanStore::in_memory().unwrap();
        kanban
            .set_setting(crate::kanban::model::settings_keys::FEATURE_ENABLED, "true")
            .unwrap();
        kanban
            .set_setting(crate::kanban::model::settings_keys::MAX_PARALLEL_TASKS, "1")
            .unwrap();
        kanban
            .upsert_project_memory(&crate::kanban::model::ProjectMemory {
                project_id: "p1".into(),
                purpose: None,
                languages: vec![],
                key_directories: vec![],
                conventions: vec![],
                recent_task_themes: vec![],
                seeded_at: Some(1),
                updated_at: 1,
            })
            .unwrap();
        let in_code = kanban.insert_task("running", "").unwrap();
        let waiting = kanban.insert_task("waiting", "").unwrap();
        kanban
            .update_task(
                &in_code.task_id,
                None,
                None,
                Some(TaskState::Code),
                Some(Some("p1")),
                None,
                None,
                None,
                None,
            )
            .unwrap();
        kanban
            .update_task(
                &waiting.task_id,
                None,
                None,
                Some(TaskState::Ready),
                Some(Some("p1")),
                None,
                None,
                None,
                None,
            )
            .unwrap();
        tick_once(&kanban, &no_projects(), None).await.unwrap();
        let still_ready = kanban.get_task(&waiting.task_id).unwrap().unwrap();
        assert_eq!(
            still_ready.state,
            TaskState::Ready,
            "ready task should hold when parallelism cap is reached"
        );
    }

    #[tokio::test]
    async fn full_state_machine_walk_no_spawner() {
        let kanban = KanbanStore::in_memory().unwrap();
        let t = kanban.insert_task("title", "body").unwrap();

        tick_once(&kanban, &no_projects(), None).await.unwrap();
        assert_eq!(
            kanban.get_task(&t.task_id).unwrap().unwrap().state,
            TaskState::Triage
        );

        kanban
            .update_task(
                &t.task_id,
                None,
                None,
                None,
                Some(Some("proj_abc")),
                None,
                None,
                None,
                None,
            )
            .unwrap();

        tick_once(&kanban, &no_projects(), None).await.unwrap();
        assert_eq!(
            kanban.get_task(&t.task_id).unwrap().unwrap().state,
            TaskState::Ready
        );

        tick_once(&kanban, &no_projects(), None).await.unwrap();
        assert_eq!(
            kanban.get_task(&t.task_id).unwrap().unwrap().state,
            TaskState::Code
        );

        tick_once(&kanban, &no_projects(), None).await.unwrap();
        assert_eq!(
            kanban.get_task(&t.task_id).unwrap().unwrap().state,
            TaskState::AgentReview
        );

        tick_once(&kanban, &no_projects(), None).await.unwrap();
        assert_eq!(
            kanban.get_task(&t.task_id).unwrap().unwrap().state,
            TaskState::HumanReview
        );

        kanban
            .update_task(
                &t.task_id,
                None,
                None,
                Some(TaskState::Merge),
                None,
                None,
                None,
                None,
                None,
            )
            .unwrap();
        tick_once(&kanban, &no_projects(), None).await.unwrap();
        assert_eq!(
            kanban.get_task(&t.task_id).unwrap().unwrap().state,
            TaskState::Done
        );
    }
}
