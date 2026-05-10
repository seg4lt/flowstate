//! HTTP routes for the kanban orchestrator feature.
//!
//! Mounted on the same loopback transport as the rest of the app-
//! layer surface (see `apps/flowstate/src-tauri/src/loopback_http.rs`
//! — the kanban router is merged in alongside the existing
//! `app_layer_router`).
//!
//! ## Feature flag
//!
//! Every route except `GET/POST /api/orchestrator/status` and
//! `POST /api/orchestrator/feature-flag` returns **404** when
//! `feature_enabled = false`. The feature flag is the only knob a
//! brand-new user touches; everything else (kanban state, settings)
//! is hidden behind it.
//!
//! ## Tick coordination
//!
//! When the kanban state changes in a way that warrants immediate
//! orchestrator attention (new task, new comment, HumanReview
//! approval, NeedsHuman resolve, feature-flag flip), the route
//! calls `OrchestratorTickKick::kick()` so the loop doesn't wait
//! for its next periodic tick. The kick handle is `Option<...>`
//! because the routes are useful on their own (read/write the
//! board, render the UI) even before the tick loop is wired up.
//! Builds without a tick handle simply skip the kick.

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};

use super::model::{
    CommentAuthor, ProjectMemory, SessionRole, Task, TaskComment, TaskSession, TaskState,
    settings_keys,
};
use super::service::{TransitionError, validate_transition};
use super::store::KanbanStore;

/// A control surface the HTTP routes use to talk to the
/// orchestrator tick loop without depending on the loop's
/// concrete type.
///
/// Two operations:
///
/// - `kick()` — wake the loop on the next iteration even if it's
///   mid-sleep. Cheap; safe to spam.
/// - `set_enabled(bool)` — flip the loop's run/pause gate. The
///   loop reads this through a `watch::Receiver`, so a flip
///   immediately unparks a paused loop. Without this, persisting
///   the toggle state to SQLite alone wouldn't actually start the
///   loop — the loop only consults the watch channel, never the
///   SQLite row, after startup.
pub trait OrchestratorTickKick: Send + Sync {
    fn kick(&self);
    fn set_enabled(&self, enabled: bool);
}

/// State handed to every kanban handler.
#[derive(Clone)]
pub struct KanbanApiState {
    pub kanban: KanbanStore,
    /// `None` until the tick loop is wired up in the Tauri shell.
    /// Routes that would kick on state change simply skip when None.
    pub tick: Option<Arc<dyn OrchestratorTickKick>>,
}

impl KanbanApiState {
    fn kick(&self) {
        if let Some(t) = &self.tick {
            t.kick();
        }
    }
    fn set_tick_enabled(&self, enabled: bool) {
        if let Some(t) = &self.tick {
            t.set_enabled(enabled);
        }
    }
}

/// Build the kanban router. Pre-`.with_state()`-stamped so it
/// merges directly into the outer Router via `.merge()`.
pub fn router(state: KanbanApiState) -> Router<()> {
    Router::new()
        // Always-on (even when feature flag is false) so the
        // settings UI can read/flip the flag.
        .route("/api/orchestrator/status", get(status_h))
        .route("/api/orchestrator/feature-flag", post(set_feature_flag_h))
        // Feature-gated below — handlers check `feature_enabled`
        // and return 404 if false.
        .route("/api/orchestrator/toggle", post(set_tick_toggle_h))
        .route("/api/orchestrator/tick-interval", post(set_tick_interval_h))
        .route(
            "/api/orchestrator/max-parallel-tasks",
            post(set_max_parallel_tasks_h),
        )
        // Task CRUD.
        .route("/api/orchestrator/tasks", get(list_tasks_h).post(create_task_h))
        .route(
            "/api/orchestrator/tasks/{task_id}",
            get(get_task_h).delete(delete_task_h),
        )
        // Comments.
        .route(
            "/api/orchestrator/tasks/{task_id}/comments",
            get(list_comments_h).post(post_comment_h),
        )
        // Linked sessions (for the task drawer).
        .route(
            "/api/orchestrator/tasks/{task_id}/sessions",
            get(list_task_sessions_h),
        )
        // Dependency edges. GET returns `{deps:[...], blockers:[...]}`
        // where `deps` is every dep this task has and `blockers`
        // is the subset still unresolved.
        .route(
            "/api/orchestrator/tasks/{task_id}/dependencies",
            get(list_task_dependencies_h).post(add_task_dependency_h),
        )
        .route(
            "/api/orchestrator/tasks/{task_id}/dependencies/{depends_on}",
            post(remove_task_dependency_h),
        )
        // Human-gate transitions.
        .route(
            "/api/orchestrator/tasks/{task_id}/approve",
            post(approve_human_review_h),
        )
        .route(
            "/api/orchestrator/tasks/{task_id}/resolve",
            post(resolve_needs_human_h),
        )
        .route(
            "/api/orchestrator/tasks/{task_id}/cancel",
            post(cancel_task_h),
        )
        // Project memory (user-editable).
        .route("/api/orchestrator/memory", get(list_memory_h))
        .route(
            "/api/orchestrator/memory/{project_id}",
            get(get_memory_h).put(put_memory_h),
        )
        .with_state(state)
}

// ── response helpers ───────────────────────────────────────────────

fn json_ok<T: Serialize>(v: T) -> Response {
    (StatusCode::OK, Json(v)).into_response()
}

fn err(code: StatusCode, msg: impl Into<String>) -> Response {
    (code, msg.into()).into_response()
}

fn into_response<T: Serialize>(r: Result<T, String>) -> Response {
    match r {
        Ok(v) => json_ok(v),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

/// 404 unless the feature flag is true. Centralizes the gating
/// check so every route uses the same shape and so the UI can
/// trust that "404" really means "feature off" (rather than
/// "endpoint missing").
fn require_feature(state: &KanbanApiState) -> Result<(), Response> {
    match state.kanban.feature_enabled() {
        Ok(true) => Ok(()),
        Ok(false) => Err(err(
            StatusCode::NOT_FOUND,
            "orchestrator feature is not enabled",
        )),
        Err(e) => Err(err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("feature flag read failed: {e}"),
        )),
    }
}

// ── status + settings ──────────────────────────────────────────────

#[derive(Serialize)]
struct StatusResponse {
    #[serde(rename = "featureEnabled")]
    feature_enabled: bool,
    #[serde(rename = "tickEnabled")]
    tick_enabled: bool,
    #[serde(rename = "tickIntervalMs")]
    tick_interval_ms: u64,
    #[serde(rename = "maxParallelTasks")]
    max_parallel_tasks: u64,
}

async fn status_h(State(state): State<KanbanApiState>) -> Response {
    let feature_enabled = match state.kanban.feature_enabled() {
        Ok(v) => v,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e),
    };
    let tick_enabled = match state.kanban.tick_enabled() {
        Ok(v) => v,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e),
    };
    let tick_interval_ms = match state.kanban.tick_interval_ms() {
        Ok(v) => v,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e),
    };
    let max_parallel_tasks = match state.kanban.max_parallel_tasks() {
        Ok(v) => v,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e),
    };
    json_ok(StatusResponse {
        feature_enabled,
        tick_enabled,
        tick_interval_ms,
        max_parallel_tasks,
    })
}

#[derive(Deserialize)]
struct EnabledBody {
    enabled: bool,
}

/// Always available — this is how a brand-new user turns the
/// feature on for the first time. Doesn't kick the tick (the tick
/// is gated by a separate toggle); the front-end navigates to
/// `/orchestrator` after this returns 200.
async fn set_feature_flag_h(
    State(state): State<KanbanApiState>,
    Json(body): Json<EnabledBody>,
) -> Response {
    let value = if body.enabled { "true" } else { "false" };
    match state.kanban.set_setting(settings_keys::FEATURE_ENABLED, value) {
        Ok(()) => {
            // Kicking here is harmless — if the feature was just
            // turned on, the tick loop might have actionable rows
            // waiting from a prior session that should resume.
            state.kick();
            json_ok(serde_json::json!({ "ok": true }))
        }
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

/// 404 when feature flag is off — the kanban window isn't even
/// visible in that case, so a route that mutates its state should
/// not be reachable.
///
/// Order of side effects matters:
///   1. Persist the toggle to SQLite first so a daemon restart
///      mid-flip resumes with the user's intended state.
///   2. Flip the in-memory `watch::Sender<bool>` so the parked
///      loop unparks NOW (a `Notify` kick wouldn't unpark a
///      `watch::changed().await` sleep). Skipping this was the
///      bug that left the loop frozen even after the user
///      clicked Loop ON.
///   3. Kick `Notify` so a loop that was mid-sleep on its tick
///      interval also wakes immediately rather than waiting out
///      the remaining seconds.
async fn set_tick_toggle_h(
    State(state): State<KanbanApiState>,
    Json(body): Json<EnabledBody>,
) -> Response {
    if let Err(r) = require_feature(&state) {
        return r;
    }
    let value = if body.enabled { "true" } else { "false" };
    if let Err(e) = state.kanban.set_setting(settings_keys::TICK_ENABLED, value) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, e);
    }
    state.set_tick_enabled(body.enabled);
    state.kick();
    json_ok(serde_json::json!({ "ok": true }))
}

#[derive(Deserialize)]
struct TickIntervalBody {
    ms: u64,
}

async fn set_tick_interval_h(
    State(state): State<KanbanApiState>,
    Json(body): Json<TickIntervalBody>,
) -> Response {
    if let Err(r) = require_feature(&state) {
        return r;
    }
    if body.ms < 1_000 || body.ms > 300_000 {
        return err(
            StatusCode::BAD_REQUEST,
            format!("ms must be in [1000, 300000]; got {}", body.ms),
        );
    }
    match state
        .kanban
        .set_setting(settings_keys::TICK_INTERVAL_MS, &body.ms.to_string())
    {
        Ok(()) => {
            state.kick();
            json_ok(serde_json::json!({ "ok": true }))
        }
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

#[derive(Deserialize)]
struct MaxParallelBody {
    n: u64,
}

async fn set_max_parallel_tasks_h(
    State(state): State<KanbanApiState>,
    Json(body): Json<MaxParallelBody>,
) -> Response {
    if let Err(r) = require_feature(&state) {
        return r;
    }
    // Clamp to a sane range. Zero would deadlock the system
    // (no task could ever leave Ready) and 50+ is more than any
    // single dev workstation can handle in parallel anyway.
    if body.n < 1 || body.n > 50 {
        return err(
            StatusCode::BAD_REQUEST,
            format!("n must be in [1, 50]; got {}", body.n),
        );
    }
    match state
        .kanban
        .set_setting(settings_keys::MAX_PARALLEL_TASKS, &body.n.to_string())
    {
        Ok(()) => {
            // Bumping the cap can unblock waiting tasks; kick the
            // loop so we don't sit on parked-Ready tasks for up
            // to a full tick interval.
            state.kick();
            json_ok(serde_json::json!({ "ok": true }))
        }
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

// ── tasks ──────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct CreateTaskBody {
    /// Free-text. Title is auto-derived by triage; for now we
    /// take the first ~80 chars as the placeholder title so the
    /// UI has something to render before triage runs.
    body: String,
}

async fn list_tasks_h(State(state): State<KanbanApiState>) -> Response {
    if let Err(r) = require_feature(&state) {
        return r;
    }
    into_response(state.kanban.list_tasks())
}

async fn create_task_h(
    State(state): State<KanbanApiState>,
    Json(body): Json<CreateTaskBody>,
) -> Response {
    if let Err(r) = require_feature(&state) {
        return r;
    }
    let trimmed = body.body.trim();
    if trimmed.is_empty() {
        return err(StatusCode::BAD_REQUEST, "body must not be empty");
    }
    // Placeholder title — first non-empty line, truncated. Triage
    // overwrites this with a better one once it runs.
    let title = trimmed
        .lines()
        .find(|l| !l.trim().is_empty())
        .map(|l| l.trim())
        .unwrap_or(trimmed);
    let title: String = title.chars().take(80).collect();
    let title = if title.is_empty() {
        "(untitled)".to_string()
    } else {
        title
    };
    match state.kanban.insert_task(&title, trimmed) {
        Ok(task) => {
            state.kick();
            (StatusCode::CREATED, Json(task)).into_response()
        }
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

async fn get_task_h(
    State(state): State<KanbanApiState>,
    Path(task_id): Path<String>,
) -> Response {
    if let Err(r) = require_feature(&state) {
        return r;
    }
    match state.kanban.get_task(&task_id) {
        Ok(Some(t)) => json_ok(t),
        Ok(None) => err(StatusCode::NOT_FOUND, format!("task {task_id} not found")),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

async fn delete_task_h(
    State(state): State<KanbanApiState>,
    Path(task_id): Path<String>,
) -> Response {
    if let Err(r) = require_feature(&state) {
        return r;
    }
    match state.kanban.delete_task(&task_id) {
        Ok(()) => {
            state.kick();
            json_ok(serde_json::json!({ "ok": true }))
        }
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

// ── comments ───────────────────────────────────────────────────────

#[derive(Deserialize)]
struct PostCommentBody {
    body: String,
    /// Optional. Defaults to `user`. The orchestrator-MCP dispatch
    /// uses different authors when posting on behalf of agents.
    author: Option<String>,
}

async fn list_comments_h(
    State(state): State<KanbanApiState>,
    Path(task_id): Path<String>,
) -> Response {
    if let Err(r) = require_feature(&state) {
        return r;
    }
    into_response(state.kanban.list_comments(&task_id))
}

async fn post_comment_h(
    State(state): State<KanbanApiState>,
    Path(task_id): Path<String>,
    Json(body): Json<PostCommentBody>,
) -> Response {
    if let Err(r) = require_feature(&state) {
        return r;
    }
    if body.body.trim().is_empty() {
        return err(StatusCode::BAD_REQUEST, "comment body must not be empty");
    }
    let author = match body.author.as_deref() {
        None => CommentAuthor::User,
        Some(s) => match CommentAuthor::from_str(s) {
            Some(a) => a,
            None => return err(StatusCode::BAD_REQUEST, format!("unknown author '{s}'")),
        },
    };
    // Confirm the task exists first so we return 404 instead of a
    // FK violation surfaced as a 500.
    match state.kanban.get_task(&task_id) {
        Ok(Some(_)) => {}
        Ok(None) => return err(StatusCode::NOT_FOUND, format!("task {task_id} not found")),
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
    match state.kanban.insert_comment(&task_id, author, body.body.trim()) {
        Ok(c) => {
            state.kick();
            (StatusCode::CREATED, Json(c)).into_response()
        }
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

// ── sessions ───────────────────────────────────────────────────────

async fn list_task_sessions_h(
    State(state): State<KanbanApiState>,
    Path(task_id): Path<String>,
) -> Response {
    if let Err(r) = require_feature(&state) {
        return r;
    }
    into_response(state.kanban.list_task_sessions(&task_id))
}

// ── dependencies ──────────────────────────────────────────────────

#[derive(Serialize)]
struct DependenciesResponse {
    deps: Vec<String>,
    blockers: Vec<String>,
}

async fn list_task_dependencies_h(
    State(state): State<KanbanApiState>,
    Path(task_id): Path<String>,
) -> Response {
    if let Err(r) = require_feature(&state) {
        return r;
    }
    let deps = match state.kanban.list_task_dependencies(&task_id) {
        Ok(d) => d,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e),
    };
    let blockers = match state.kanban.unresolved_dependencies(&task_id) {
        Ok(b) => b,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e),
    };
    json_ok(DependenciesResponse { deps, blockers })
}

#[derive(Deserialize)]
struct AddDepBody {
    depends_on: String,
}

async fn add_task_dependency_h(
    State(state): State<KanbanApiState>,
    Path(task_id): Path<String>,
    Json(body): Json<AddDepBody>,
) -> Response {
    if let Err(r) = require_feature(&state) {
        return r;
    }
    // Sanity-check both ids exist before writing the edge so the
    // user gets a clean 404 instead of a CASCADE FK error later.
    match state.kanban.get_task(&task_id) {
        Ok(Some(_)) => {}
        Ok(None) => return err(StatusCode::NOT_FOUND, format!("task {task_id} not found")),
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
    match state.kanban.get_task(&body.depends_on) {
        Ok(Some(_)) => {}
        Ok(None) => {
            return err(
                StatusCode::NOT_FOUND,
                format!("depends_on task {} not found", body.depends_on),
            );
        }
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
    match state.kanban.add_task_dependency(&task_id, &body.depends_on) {
        Ok(()) => {
            state.kick();
            json_ok(serde_json::json!({ "ok": true }))
        }
        Err(e) => err(StatusCode::BAD_REQUEST, e),
    }
}

async fn remove_task_dependency_h(
    State(state): State<KanbanApiState>,
    Path((task_id, depends_on)): Path<(String, String)>,
) -> Response {
    if let Err(r) = require_feature(&state) {
        return r;
    }
    match state.kanban.remove_task_dependency(&task_id, &depends_on) {
        Ok(()) => {
            state.kick();
            json_ok(serde_json::json!({ "ok": true }))
        }
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

// ── human-gate transitions ─────────────────────────────────────────

/// HumanReview → Merge (auto-merge runs next tick). Idempotent at
/// the SQLite layer because the FSM validator allows self-loops,
/// but we still 409 if the task isn't in HumanReview so a
/// misclick from the UI is obvious.
async fn approve_human_review_h(
    State(state): State<KanbanApiState>,
    Path(task_id): Path<String>,
) -> Response {
    if let Err(r) = require_feature(&state) {
        return r;
    }
    let task = match state.kanban.get_task(&task_id) {
        Ok(Some(t)) => t,
        Ok(None) => return err(StatusCode::NOT_FOUND, format!("task {task_id} not found")),
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e),
    };
    if task.state != TaskState::HumanReview {
        return err(
            StatusCode::CONFLICT,
            format!(
                "task is in {}; approve only valid from HumanReview",
                task.state.as_str()
            ),
        );
    }
    if let Err(e) = validate_transition(task.state, TaskState::Merge) {
        return transition_err(e);
    }
    if let Err(e) = state.kanban.update_task(
        &task_id,
        None,
        None,
        Some(TaskState::Merge),
        None,
        None,
        None,
        None,
        None,
    ) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, e);
    }
    if let Err(e) = state.kanban.insert_comment(
        &task_id,
        CommentAuthor::System,
        "human approved review — moving to Merge",
    ) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, e);
    }
    state.kick();
    json_ok(serde_json::json!({ "ok": true, "state": TaskState::Merge.as_str() }))
}

#[derive(Deserialize, Default)]
struct ResolveBody {
    /// Where to send the task after the human resolved the
    /// blocker. Defaults to `Code` (most common case). The UI
    /// surfaces a dropdown when the task has been through more
    /// than one prior state so the user can pick precisely.
    #[serde(default)]
    next_state: Option<String>,
    /// Optional comment the human leaves when resolving — e.g.
    /// "merged main into the worktree, retry now".
    #[serde(default)]
    comment: Option<String>,
}

async fn resolve_needs_human_h(
    State(state): State<KanbanApiState>,
    Path(task_id): Path<String>,
    Json(body): Json<ResolveBody>,
) -> Response {
    if let Err(r) = require_feature(&state) {
        return r;
    }
    let task = match state.kanban.get_task(&task_id) {
        Ok(Some(t)) => t,
        Ok(None) => return err(StatusCode::NOT_FOUND, format!("task {task_id} not found")),
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e),
    };
    if task.state != TaskState::NeedsHuman {
        return err(
            StatusCode::CONFLICT,
            format!(
                "task is in {}; resolve only valid from NeedsHuman",
                task.state.as_str()
            ),
        );
    }
    let next = match body.next_state.as_deref() {
        Some(s) => match TaskState::from_str(s) {
            Some(s) => s,
            None => return err(StatusCode::BAD_REQUEST, format!("unknown state '{s}'")),
        },
        None => TaskState::Code,
    };
    if let Err(e) = validate_transition(TaskState::NeedsHuman, next) {
        return transition_err(e);
    }
    if let Err(e) = state.kanban.update_task(
        &task_id,
        None,
        None,
        Some(next),
        None,
        None,
        None,
        None,
        Some(None), // clear the needs_human_reason
    ) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, e);
    }
    if let Some(c) = body.comment {
        if !c.trim().is_empty() {
            let _ = state
                .kanban
                .insert_comment(&task_id, CommentAuthor::User, c.trim());
        }
    }
    let _ = state.kanban.insert_comment(
        &task_id,
        CommentAuthor::System,
        &format!("resolved — moving to {}", next.as_str()),
    );
    state.kick();
    json_ok(serde_json::json!({ "ok": true, "state": next.as_str() }))
}

async fn cancel_task_h(
    State(state): State<KanbanApiState>,
    Path(task_id): Path<String>,
) -> Response {
    if let Err(r) = require_feature(&state) {
        return r;
    }
    let task = match state.kanban.get_task(&task_id) {
        Ok(Some(t)) => t,
        Ok(None) => return err(StatusCode::NOT_FOUND, format!("task {task_id} not found")),
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e),
    };
    if let Err(e) = validate_transition(task.state, TaskState::Cancelled) {
        return transition_err(e);
    }
    if let Err(e) = state.kanban.update_task(
        &task_id,
        None,
        None,
        Some(TaskState::Cancelled),
        None,
        None,
        None,
        None,
        None,
    ) {
        return err(StatusCode::INTERNAL_SERVER_ERROR, e);
    }
    let _ = state.kanban.insert_comment(
        &task_id,
        CommentAuthor::System,
        "task cancelled by user",
    );
    // No kick — Cancelled is terminal, nothing for the loop to do.
    json_ok(serde_json::json!({ "ok": true }))
}

fn transition_err(e: TransitionError) -> Response {
    err(StatusCode::CONFLICT, e.to_string())
}

// ── project memory ─────────────────────────────────────────────────

async fn list_memory_h(State(state): State<KanbanApiState>) -> Response {
    if let Err(r) = require_feature(&state) {
        return r;
    }
    into_response(state.kanban.list_project_memory())
}

async fn get_memory_h(
    State(state): State<KanbanApiState>,
    Path(project_id): Path<String>,
) -> Response {
    if let Err(r) = require_feature(&state) {
        return r;
    }
    match state.kanban.get_project_memory(&project_id) {
        Ok(Some(m)) => json_ok(m),
        Ok(None) => err(
            StatusCode::NOT_FOUND,
            format!("no memory for project {project_id}"),
        ),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

#[derive(Deserialize)]
struct PutMemoryBody {
    #[serde(default)]
    purpose: Option<String>,
    #[serde(default)]
    languages: Vec<String>,
    #[serde(default)]
    key_directories: Vec<super::model::KeyDirectory>,
    #[serde(default)]
    conventions: Vec<String>,
    #[serde(default)]
    recent_task_themes: Vec<String>,
}

async fn put_memory_h(
    State(state): State<KanbanApiState>,
    Path(project_id): Path<String>,
    Json(body): Json<PutMemoryBody>,
) -> Response {
    if let Err(r) = require_feature(&state) {
        return r;
    }
    let memory = ProjectMemory {
        project_id: project_id.clone(),
        purpose: body.purpose,
        languages: body.languages,
        key_directories: body.key_directories,
        conventions: body.conventions,
        recent_task_themes: body.recent_task_themes,
        seeded_at: None, // store preserves existing seeded_at via COALESCE
        updated_at: 0,
    };
    match state.kanban.upsert_project_memory(&memory) {
        Ok(()) => json_ok(serde_json::json!({ "ok": true })),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

// ── unused-but-public-ish re-exports ─────────────────────────────
// Compiler hygiene: keep the types we expose through serde wire
// shapes referenced so a future "we never use Task anywhere"
// dead-code warning doesn't blossom unexpectedly.
#[allow(dead_code)]
fn _wire_shape_smoke() -> (Task, TaskComment, TaskSession, SessionRole, TaskState) {
    unreachable!()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{to_bytes, Body};
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    fn app(feature_on: bool) -> Router<()> {
        let kanban = KanbanStore::in_memory().unwrap();
        if feature_on {
            kanban.set_setting(settings_keys::FEATURE_ENABLED, "true").unwrap();
        }
        router(KanbanApiState { kanban, tick: None })
    }

    async fn body_string(resp: axum::response::Response) -> (StatusCode, String) {
        let status = resp.status();
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        (status, String::from_utf8_lossy(&bytes).to_string())
    }

    #[tokio::test]
    async fn status_works_without_feature_flag() {
        let app = app(false);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/orchestrator/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let (status, body) = body_string(resp).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("\"featureEnabled\":false"));
    }

    #[tokio::test]
    async fn tasks_404_when_feature_off() {
        let app = app(false);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/orchestrator/tasks")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn create_and_list_tasks() {
        let app = app(true);
        // Create.
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/orchestrator/tasks")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"body":"fix the typo in README"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        let (status, body) = body_string(resp).await;
        assert_eq!(status, StatusCode::CREATED);
        assert!(body.contains("\"state\":\"Open\""));
        // List.
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/orchestrator/tasks")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let (status, body) = body_string(resp).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("fix the typo in README"));
    }

    #[tokio::test]
    async fn feature_flag_round_trip() {
        let app = app(false);
        // Initially false.
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/orchestrator/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let (_, body) = body_string(resp).await;
        assert!(body.contains("\"featureEnabled\":false"));
        // Flip on.
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/orchestrator/feature-flag")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"enabled":true}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        // Confirmed.
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/orchestrator/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let (_, body) = body_string(resp).await;
        assert!(body.contains("\"featureEnabled\":true"));
    }

    #[tokio::test]
    async fn approve_only_from_human_review() {
        let app_router = app(true);
        // Create task in Open state.
        let kanban = KanbanStore::in_memory().unwrap();
        kanban.set_setting(settings_keys::FEATURE_ENABLED, "true").unwrap();
        let t = kanban.insert_task("title", "body").unwrap();
        let app2 = router(KanbanApiState { kanban: kanban.clone(), tick: None });
        let resp = app2
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(&format!("/api/orchestrator/tasks/{}/approve", t.task_id))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT);
        // Move task to HumanReview manually, then approve.
        kanban
            .update_task(
                &t.task_id,
                None,
                None,
                Some(TaskState::HumanReview),
                None,
                None,
                None,
                None,
                None,
            )
            .unwrap();
        let app3 = router(KanbanApiState { kanban: kanban.clone(), tick: None });
        let resp = app3
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(&format!("/api/orchestrator/tasks/{}/approve", t.task_id))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let got = kanban.get_task(&t.task_id).unwrap().unwrap();
        assert_eq!(got.state, TaskState::Merge);
        // silence unused
        let _ = app_router;
    }
}
