//! Typed HTTP client the Tauri shell uses to talk to the daemon.
//!
//! Phase 5 of the architecture plan ("flip webview to HTTP-via-
//! proxy") plus Phase 6 prep: the `#[tauri::command]` bodies for
//! the app-layer commands (user_config / usage / …) route through
//! this client instead of calling `UserConfigStore` / `UsageStore`
//! directly.
//!
//! Why route via HTTP even when the daemon is still embedded in the
//! Tauri process:
//!
//! - Single code path: the frontend sees the same wire shape whether
//!   the daemon is in-process (Phase 1 embedded loopback) or out-of-
//!   process (Phase 6 detached daemon). We flip the base URL source
//!   at startup and nothing downstream notices.
//! - The HTTP surface we built in Phase 4 (`flowstate_app_layer::http`)
//!   is the permanent home for this logic; duplicating it in both
//!   Tauri command bodies and axum handlers would mean two places to
//!   audit whenever the schema changes.
//! - Forces us to keep the JSON shapes consistent as tested surfaces.
//!
//! The small overhead (~0.5 ms per call over loopback TCP) is
//! negligible versus typical UI interaction latencies.

use std::sync::Arc;

use reqwest::Client;
use serde::{de::DeserializeOwned, Serialize};
use serde_json::Value;
use tokio::sync::watch;

use flowstate_app_layer::usage::{
    TopSessionRow, UsageAgentPayload, UsageBucket, UsageGroupBy, UsageRange, UsageSummaryPayload,
    UsageTimeseriesPayload,
};
use flowstate_app_layer::user_config::{ProjectDisplay, ProjectWorktree, SessionDisplay};
use zenui_provider_api::RateLimitInfo;
use std::collections::HashMap;

/// Shared client. Holds a `reqwest::Client` + a `watch::Receiver`
/// over the base URL so a daemon respawn (Phase 6) can publish a
/// new URL and every in-flight and future call picks it up.
///
/// Methods return `Result<T, String>` matching the exact shape the
/// Tauri commands expose to the webview. An HTTP-level failure
/// (connect refused, 5xx, deserialize error) gets stringified and
/// returned as `Err` — the webview handles it the same way it
/// handles a command that returned `Err` today.
#[derive(Clone)]
pub struct DaemonClient {
    http: Client,
    base_url: watch::Receiver<Option<String>>,
}

impl DaemonClient {
    pub fn new(base_url: watch::Receiver<Option<String>>) -> Self {
        // `reqwest::Client` is internally Arc'd; cloning is cheap
        // (bumps a reference count). No custom timeout — callers
        // inherit reqwest's default (no request timeout, only a
        // connect timeout). App-layer reads are sub-millisecond;
        // a missing timeout is safer than a wrong one.
        Self {
            http: Client::new(),
            base_url,
        }
    }

    fn url(&self, path: &str) -> Result<String, String> {
        let base = self.base_url.borrow().clone().ok_or_else(|| {
            "daemon base URL not yet available; loopback transport \
             may still be starting"
                .to_string()
        })?;
        Ok(format!("{base}{path}"))
    }

    /// POST `body` as JSON to `path`, return the JSON response body
    /// deserialized as `T`. Private helper — every typed method
    /// below wraps it.
    async fn post<B: Serialize, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T, String> {
        let url = self.url(path)?;
        let resp = self
            .http
            .post(&url)
            .json(body)
            .send()
            .await
            .map_err(|e| format!("daemon request {path}: {e}"))?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(if text.is_empty() {
                format!("daemon returned HTTP {status} for {path}")
            } else {
                text
            });
        }
        resp.json::<T>()
            .await
            .map_err(|e| format!("daemon response from {path} was not valid JSON: {e}"))
    }

    async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T, String> {
        let url = self.url(path)?;
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| format!("daemon request {path}: {e}"))?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(if text.is_empty() {
                format!("daemon returned HTTP {status} for {path}")
            } else {
                text
            });
        }
        resp.json::<T>()
            .await
            .map_err(|e| format!("daemon response from {path} was not valid JSON: {e}"))
    }

    // ── user_config kv ─────────────────────────────────────────

    pub async fn get_user_config(&self, key: String) -> Result<Option<String>, String> {
        self.post("/api/user_config/get", &serde_json::json!({ "key": key }))
            .await
    }

    pub async fn set_user_config(&self, key: String, value: String) -> Result<(), String> {
        let _: Value = self
            .post(
                "/api/user_config/set",
                &serde_json::json!({ "key": key, "value": value }),
            )
            .await?;
        Ok(())
    }

    // ── session display ────────────────────────────────────────

    pub async fn set_session_display(
        &self,
        session_id: String,
        display: SessionDisplay,
    ) -> Result<(), String> {
        let _: Value = self
            .post(
                "/api/session_display/set",
                &serde_json::json!({ "session_id": session_id, "display": display }),
            )
            .await?;
        Ok(())
    }

    pub async fn get_session_display(
        &self,
        session_id: String,
    ) -> Result<Option<SessionDisplay>, String> {
        self.post(
            "/api/session_display/get",
            &serde_json::json!({ "session_id": session_id }),
        )
        .await
    }

    pub async fn list_session_display(&self) -> Result<HashMap<String, SessionDisplay>, String> {
        self.get("/api/session_display/list").await
    }

    pub async fn delete_session_display(&self, session_id: String) -> Result<(), String> {
        let _: Value = self
            .post(
                "/api/session_display/delete",
                &serde_json::json!({ "session_id": session_id }),
            )
            .await?;
        Ok(())
    }

    // ── project display ────────────────────────────────────────

    pub async fn set_project_display(
        &self,
        project_id: String,
        display: ProjectDisplay,
    ) -> Result<(), String> {
        let _: Value = self
            .post(
                "/api/project_display/set",
                &serde_json::json!({ "project_id": project_id, "display": display }),
            )
            .await?;
        Ok(())
    }

    pub async fn get_project_display(
        &self,
        project_id: String,
    ) -> Result<Option<ProjectDisplay>, String> {
        self.post(
            "/api/project_display/get",
            &serde_json::json!({ "project_id": project_id }),
        )
        .await
    }

    pub async fn list_project_display(&self) -> Result<HashMap<String, ProjectDisplay>, String> {
        self.get("/api/project_display/list").await
    }

    pub async fn delete_project_display(&self, project_id: String) -> Result<(), String> {
        let _: Value = self
            .post(
                "/api/project_display/delete",
                &serde_json::json!({ "project_id": project_id }),
            )
            .await?;
        Ok(())
    }

    // ── project worktree ───────────────────────────────────────

    pub async fn set_project_worktree(
        &self,
        project_id: String,
        parent_project_id: String,
        branch: Option<String>,
    ) -> Result<(), String> {
        let _: Value = self
            .post(
                "/api/project_worktree/set",
                &serde_json::json!({
                    "project_id": project_id,
                    "parent_project_id": parent_project_id,
                    "branch": branch,
                }),
            )
            .await?;
        Ok(())
    }

    pub async fn get_project_worktree(
        &self,
        project_id: String,
    ) -> Result<Option<ProjectWorktree>, String> {
        self.post(
            "/api/project_worktree/get",
            &serde_json::json!({ "project_id": project_id }),
        )
        .await
    }

    pub async fn list_project_worktree(&self) -> Result<HashMap<String, ProjectWorktree>, String> {
        self.get("/api/project_worktree/list").await
    }

    pub async fn delete_project_worktree(&self, project_id: String) -> Result<(), String> {
        let _: Value = self
            .post(
                "/api/project_worktree/delete",
                &serde_json::json!({ "project_id": project_id }),
            )
            .await?;
        Ok(())
    }

    // ── usage analytics ────────────────────────────────────────

    pub async fn get_usage_summary(
        &self,
        range: UsageRange,
        group_by: Option<UsageGroupBy>,
    ) -> Result<UsageSummaryPayload, String> {
        self.post(
            "/api/usage/summary",
            &serde_json::json!({ "range": range, "group_by": group_by }),
        )
        .await
    }

    pub async fn get_usage_timeseries(
        &self,
        range: UsageRange,
        bucket: UsageBucket,
        split_by: Option<UsageGroupBy>,
    ) -> Result<UsageTimeseriesPayload, String> {
        self.post(
            "/api/usage/timeseries",
            &serde_json::json!({ "range": range, "bucket": bucket, "split_by": split_by }),
        )
        .await
    }

    pub async fn get_top_sessions(
        &self,
        range: UsageRange,
        limit: Option<u32>,
    ) -> Result<Vec<TopSessionRow>, String> {
        self.post(
            "/api/usage/top_sessions",
            &serde_json::json!({ "range": range, "limit": limit }),
        )
        .await
    }

    pub async fn get_usage_by_agent(&self, range: UsageRange) -> Result<UsageAgentPayload, String> {
        self.post(
            "/api/usage/by_agent",
            &serde_json::json!({ "range": range }),
        )
        .await
    }

    pub async fn get_usage_by_agent_role(
        &self,
        range: UsageRange,
    ) -> Result<UsageAgentPayload, String> {
        self.post(
            "/api/usage/by_agent_role",
            &serde_json::json!({ "range": range }),
        )
        .await
    }

    /// Last-seen snapshot of every rate-limit bucket. Called once
    /// on app boot to seed `state.rateLimits` so the chat-toolbar
    /// chips render their persisted values immediately instead of
    /// staying blank until the user sends their first message.
    pub async fn get_rate_limit_cache(&self) -> Result<Vec<RateLimitInfo>, String> {
        self.get("/api/usage/rate_limit_cache").await
    }
}

/// Cheap-to-clone publisher used by the loopback transport bringup
/// to tell the client "here's the URL to hit." Separate type from
/// `DaemonClient` so the Tauri state holds one sender and hands out
/// receivers to every command that needs one.
#[derive(Clone)]
pub struct DaemonBaseUrl {
    tx: Arc<watch::Sender<Option<String>>>,
    rx: watch::Receiver<Option<String>>,
}

impl DaemonBaseUrl {
    pub fn new() -> Self {
        let (tx, rx) = watch::channel(None);
        Self {
            tx: Arc::new(tx),
            rx,
        }
    }

    pub fn publish(&self, base_url: String) {
        let _ = self.tx.send(Some(base_url));
    }

    pub fn client(&self) -> DaemonClient {
        DaemonClient::new(self.rx.clone())
    }
}

impl Default for DaemonBaseUrl {
    fn default() -> Self {
        Self::new()
    }
}
