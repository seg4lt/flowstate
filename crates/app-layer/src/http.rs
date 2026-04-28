//! HTTP surface for the app-layer commands.
//!
//! Phase 4 of the architecture plan. Every `#[tauri::command]` that
//! reads/writes `UserConfigStore` or `UsageStore` has a matching
//! route here. The Tauri shell mounts this `Router` into the
//! existing loopback HTTP server (via `HttpTransport::with_extra_router`)
//! so the app offers BOTH:
//!
//! - Tauri IPC (the webview hits these today).
//! - Loopback HTTP (the future daemon will serve these; Phase 5
//!   flips the Tauri commands to proxy via reqwest).
//!
//! Both surfaces call the same methods on the same stores — no
//! duplication, no divergence. Return types are JSON equivalents of
//! the Tauri command outputs (serde handles the conversion; the
//! structs re-export identical shapes).
//!
//! # Route shape
//!
//! REST-ish, but kept strictly in sync with the Tauri command
//! parameter lists. When the webview flips to the HTTP surface
//! (Phase 5), the reqwest calls hand the same arguments in the same
//! order — easier to review, and a curl sanity check can exercise
//! the surface by hand.
//!
//! # Errors
//!
//! On store failures we return HTTP 500 with the underlying
//! `String` error in the body. The Tauri commands forwarded their
//! `Result<T, String>` to the webview; we preserve that.

use std::collections::HashMap;
use std::sync::Arc;

use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};

use crate::usage::{
    TopSessionRow, UsageAgentPayload, UsageBucket, UsageGroupBy, UsageRange, UsageStore,
    UsageSummaryPayload, UsageTimeseriesPayload,
};
use crate::user_config::{ProjectDisplay, ProjectWorktree, SessionDisplay, UserConfigStore};

/// Sender side of the open-project signal. The HTTP route
/// `/api/open-project` (called by the `flow` CLI) pushes paths
/// onto this channel; the Tauri shell's setup task drains it and
/// forwards each path to the webview as an `"open-project"`
/// event. We use an `UnboundedSender` rather than `watch` so two
/// rapid `flow .` invocations don't coalesce — each one should
/// open a thread.
///
/// The receiver lives in `apps/flowstate/src-tauri/src/lib.rs`;
/// the sender is cloned into `AppLayerApiState` and held by the
/// axum handler. Embedders that don't care about open-project
/// (headless tests, the future standalone daemon) can pass a
/// dropped receiver — the send becomes a cheap no-op error and
/// is logged.
pub type OpenProjectSender = tokio::sync::mpsc::UnboundedSender<String>;

/// Bundled state handed to every handler. `user_config` is cheap
/// to `Clone` (Arc<Mutex<Connection>> inside). `usage` is wrapped in
/// `Arc` because `UsageStore` is not Clone, and `Option` because
/// embedders whose usage store failed to open can still expose the
/// config surface. `open_project` is `Option` for embedders that
/// don't wire the CLI bridge — a `None` returns 503 from the route.
#[derive(Clone)]
pub struct AppLayerApiState {
    pub user_config: UserConfigStore,
    pub usage: Option<Arc<UsageStore>>,
    pub open_project: Option<OpenProjectSender>,
}

/// Construct the router. Already `.with_state()`-stamped so the
/// caller merges it into their outer Router without needing to
/// thread the state type through.
pub fn router(state: AppLayerApiState) -> Router<()> {
    Router::new()
        // user config kv
        .route("/api/user_config/get", post(get_user_config_h))
        .route("/api/user_config/set", post(set_user_config_h))
        // session display
        .route("/api/session_display/set", post(set_session_display_h))
        .route("/api/session_display/get", post(get_session_display_h))
        .route("/api/session_display/list", get(list_session_display_h))
        .route(
            "/api/session_display/delete",
            post(delete_session_display_h),
        )
        // project display
        .route("/api/project_display/set", post(set_project_display_h))
        .route("/api/project_display/get", post(get_project_display_h))
        .route("/api/project_display/list", get(list_project_display_h))
        .route(
            "/api/project_display/delete",
            post(delete_project_display_h),
        )
        // project worktree links
        .route("/api/project_worktree/set", post(set_project_worktree_h))
        .route("/api/project_worktree/get", post(get_project_worktree_h))
        .route("/api/project_worktree/list", get(list_project_worktree_h))
        .route(
            "/api/project_worktree/delete",
            post(delete_project_worktree_h),
        )
        // usage analytics
        .route("/api/usage/summary", post(usage_summary_h))
        .route("/api/usage/timeseries", post(usage_timeseries_h))
        .route("/api/usage/top_sessions", post(usage_top_sessions_h))
        .route("/api/usage/by_agent", post(usage_by_agent_h))
        .route("/api/usage/by_agent_role", post(usage_by_agent_role_h))
        // CLI bridge — the `flow` binary POSTs the user's project
        // path here. The Tauri shell drains the channel and emits
        // an `open-project` event the webview consumes to spawn
        // a new thread on the project.
        .route("/api/open-project", post(open_project_h))
        .with_state(state)
}

/// Map `Result<T, String>` from the app-layer stores into an HTTP
/// response, uniformly. 200 on success, 500 on error with the raw
/// error string in the body — same shape the frontend handles today
/// for Tauri command errors.
fn into_response<T: Serialize>(r: Result<T, String>) -> Response {
    match r {
        Ok(v) => (StatusCode::OK, Json(v)).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

/// Wrap `Option<UsageStore>` access with a uniform 503 when the
/// analytics db failed to open. Today the Tauri commands would
/// return `Err("usage store not initialized")` in this case; the
/// HTTP surface preserves that shape.
fn usage_store(state: &AppLayerApiState) -> Result<Arc<UsageStore>, Response> {
    state.usage.as_ref().cloned().ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            "usage store not initialized".to_string(),
        )
            .into_response()
    })
}

// ─── user_config kv ──────────────────────────────────────────────

#[derive(Deserialize)]
struct KeyBody {
    key: String,
}
#[derive(Deserialize)]
struct KeyValueBody {
    key: String,
    value: String,
}

async fn get_user_config_h(
    State(state): State<AppLayerApiState>,
    Json(body): Json<KeyBody>,
) -> Response {
    into_response(state.user_config.get(&body.key))
}

async fn set_user_config_h(
    State(state): State<AppLayerApiState>,
    Json(body): Json<KeyValueBody>,
) -> Response {
    into_response(state.user_config.set(&body.key, &body.value))
}

// ─── session display ────────────────────────────────────────────

#[derive(Deserialize)]
struct SessionDisplayBody {
    session_id: String,
    display: SessionDisplay,
}
#[derive(Deserialize)]
struct SessionIdBody {
    session_id: String,
}

async fn set_session_display_h(
    State(state): State<AppLayerApiState>,
    Json(body): Json<SessionDisplayBody>,
) -> Response {
    into_response(
        state
            .user_config
            .set_session_display(&body.session_id, &body.display),
    )
}

async fn get_session_display_h(
    State(state): State<AppLayerApiState>,
    Json(body): Json<SessionIdBody>,
) -> Response {
    into_response(state.user_config.get_session_display(&body.session_id))
}

async fn list_session_display_h(State(state): State<AppLayerApiState>) -> Response {
    let r: Result<HashMap<String, SessionDisplay>, String> =
        state.user_config.list_session_display();
    into_response(r)
}

async fn delete_session_display_h(
    State(state): State<AppLayerApiState>,
    Json(body): Json<SessionIdBody>,
) -> Response {
    into_response(state.user_config.delete_session_display(&body.session_id))
}

// ─── project display ────────────────────────────────────────────

#[derive(Deserialize)]
struct ProjectDisplayBody {
    project_id: String,
    display: ProjectDisplay,
}
#[derive(Deserialize)]
struct ProjectIdBody {
    project_id: String,
}

async fn set_project_display_h(
    State(state): State<AppLayerApiState>,
    Json(body): Json<ProjectDisplayBody>,
) -> Response {
    into_response(
        state
            .user_config
            .set_project_display(&body.project_id, &body.display),
    )
}

async fn get_project_display_h(
    State(state): State<AppLayerApiState>,
    Json(body): Json<ProjectIdBody>,
) -> Response {
    into_response(state.user_config.get_project_display(&body.project_id))
}

async fn list_project_display_h(State(state): State<AppLayerApiState>) -> Response {
    let r: Result<HashMap<String, ProjectDisplay>, String> =
        state.user_config.list_project_display();
    into_response(r)
}

async fn delete_project_display_h(
    State(state): State<AppLayerApiState>,
    Json(body): Json<ProjectIdBody>,
) -> Response {
    into_response(state.user_config.delete_project_display(&body.project_id))
}

// ─── project worktree ───────────────────────────────────────────

#[derive(Deserialize)]
struct ProjectWorktreeBody {
    project_id: String,
    parent_project_id: String,
    branch: Option<String>,
}

async fn set_project_worktree_h(
    State(state): State<AppLayerApiState>,
    Json(body): Json<ProjectWorktreeBody>,
) -> Response {
    into_response(state.user_config.set_project_worktree(
        &body.project_id,
        &body.parent_project_id,
        body.branch.as_deref(),
    ))
}

async fn get_project_worktree_h(
    State(state): State<AppLayerApiState>,
    Json(body): Json<ProjectIdBody>,
) -> Response {
    into_response(state.user_config.get_project_worktree(&body.project_id))
}

async fn list_project_worktree_h(State(state): State<AppLayerApiState>) -> Response {
    let r: Result<HashMap<String, ProjectWorktree>, String> =
        state.user_config.list_project_worktree();
    into_response(r)
}

async fn delete_project_worktree_h(
    State(state): State<AppLayerApiState>,
    Json(body): Json<ProjectIdBody>,
) -> Response {
    into_response(state.user_config.delete_project_worktree(&body.project_id))
}

// ─── usage analytics ────────────────────────────────────────────

#[derive(Deserialize)]
struct UsageSummaryBody {
    range: UsageRange,
    group_by: Option<UsageGroupBy>,
}
#[derive(Deserialize)]
struct UsageTimeseriesBody {
    range: UsageRange,
    bucket: UsageBucket,
    split_by: Option<UsageGroupBy>,
}
#[derive(Deserialize)]
struct UsageTopSessionsBody {
    range: UsageRange,
    limit: Option<u32>,
}
#[derive(Deserialize)]
struct UsageRangeBody {
    range: UsageRange,
}

async fn usage_summary_h(
    State(state): State<AppLayerApiState>,
    Json(body): Json<UsageSummaryBody>,
) -> Response {
    let store = match usage_store(&state) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let r: Result<UsageSummaryPayload, String> =
        store.summary(body.range, body.group_by.unwrap_or_default());
    into_response(r)
}

async fn usage_timeseries_h(
    State(state): State<AppLayerApiState>,
    Json(body): Json<UsageTimeseriesBody>,
) -> Response {
    let store = match usage_store(&state) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let r: Result<UsageTimeseriesPayload, String> =
        store.timeseries(body.range, body.bucket, body.split_by);
    into_response(r)
}

async fn usage_top_sessions_h(
    State(state): State<AppLayerApiState>,
    Json(body): Json<UsageTopSessionsBody>,
) -> Response {
    let store = match usage_store(&state) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let r: Result<Vec<TopSessionRow>, String> =
        store.top_sessions(body.range, body.limit.unwrap_or(10));
    into_response(r)
}

async fn usage_by_agent_h(
    State(state): State<AppLayerApiState>,
    Json(body): Json<UsageRangeBody>,
) -> Response {
    let store = match usage_store(&state) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let r: Result<UsageAgentPayload, String> = store.summary_by_agent(body.range);
    into_response(r)
}

async fn usage_by_agent_role_h(
    State(state): State<AppLayerApiState>,
    Json(body): Json<UsageRangeBody>,
) -> Response {
    let store = match usage_store(&state) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let r: Result<UsageAgentPayload, String> = store.summary_by_agent_role(body.range);
    into_response(r)
}

// ─── CLI bridge ─────────────────────────────────────────────────

#[derive(Deserialize)]
struct OpenProjectBody {
    /// Absolute, canonical path to the project directory the user
    /// ran `flow` against. The CLI canonicalizes before sending so
    /// the daemon's project-list dedupe (keyed by path) is reliable.
    path: String,
}

/// Hand the path off to the Tauri shell. The route returns as soon
/// as the path is queued — the actual project-creation /
/// session-spawn work happens in the webview, which the shell will
/// nudge via a `"open-project"` event. Returning early means a
/// rapid `flow .` doesn't block the user's terminal while the app
/// brings its window forward.
async fn open_project_h(
    State(state): State<AppLayerApiState>,
    Json(body): Json<OpenProjectBody>,
) -> Response {
    let path = body.path.trim().to_string();
    if path.is_empty() {
        return (StatusCode::BAD_REQUEST, "path must not be empty".to_string()).into_response();
    }
    let Some(tx) = state.open_project.as_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "open-project bridge not wired in this embedder".to_string(),
        )
            .into_response();
    };
    if let Err(e) = tx.send(path) {
        // The receiver was dropped — likely means the Tauri shell
        // is mid-shutdown. Surface a 503 so the CLI prints a clean
        // "could not reach Flowstate" rather than a generic 500.
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            format!("open-project receiver gone: {e}"),
        )
            .into_response();
    }
    (StatusCode::OK, Json(serde_json::json!({}))).into_response()
}
