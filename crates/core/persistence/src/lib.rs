use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use chrono::Utc;
use uuid::Uuid;
use zenui_provider_api::{
    ContentBlock, FileChangeRecord, PermissionMode, PlanRecord, ProjectRecord, ProviderKind,
    ProviderModel, ProviderStatus, ReasoningEffort, SessionDetail, SessionStatus, SessionSummary,
    SubagentRecord, ToolCall, TurnRecord, TurnStatus,
};

#[derive(Debug)]
pub struct PersistenceService {
    connection: Mutex<Connection>,
}

impl PersistenceService {
    pub fn new(path: impl AsRef<Path>) -> Result<Self> {
        if let Some(parent) = path.as_ref().parent() {
            std::fs::create_dir_all(parent).context("failed to create persistence directory")?;
        }

        let connection = Connection::open(path).context("failed to open sqlite database")?;
        let service = Self {
            connection: Mutex::new(connection),
        };
        service.migrate()?;
        Ok(service)
    }

    pub fn in_memory() -> Result<Self> {
        let connection = Connection::open_in_memory().context("failed to open in-memory sqlite")?;
        let service = Self {
            connection: Mutex::new(connection),
        };
        service.migrate()?;
        Ok(service)
    }

    pub async fn upsert_session(&self, session: SessionDetail) {
        let mut connection = self.connection.lock().expect("sqlite mutex poisoned");
        let transaction = match connection.transaction() {
            Ok(transaction) => transaction,
            Err(_) => return,
        };

        if transaction
            .execute(
                "INSERT INTO sessions (
                    session_id, provider, title, status, created_at, updated_at, last_turn_preview, turn_count, provider_state_json, model, project_id
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
                 ON CONFLICT(session_id) DO UPDATE SET
                    provider = excluded.provider,
                    title = excluded.title,
                    status = excluded.status,
                    created_at = excluded.created_at,
                    updated_at = excluded.updated_at,
                    last_turn_preview = excluded.last_turn_preview,
                    turn_count = excluded.turn_count,
                    provider_state_json = excluded.provider_state_json,
                    model = excluded.model,
                    project_id = excluded.project_id",
                params![
                    session.summary.session_id,
                    provider_kind_to_str(session.summary.provider),
                    session.summary.title,
                    session_status_to_str(session.summary.status),
                    session.summary.created_at,
                    session.summary.updated_at,
                    session.summary.last_turn_preview,
                    session.summary.turn_count as i64,
                    session
                        .provider_state
                        .as_ref()
                        .and_then(|state| serde_json::to_string(state).ok()),
                    session.summary.model,
                    session.summary.project_id,
                ],
            )
            .is_err()
        {
            return;
        }

        if transaction
            .execute(
                "DELETE FROM turns WHERE session_id = ?1",
                params![session.summary.session_id],
            )
            .is_err()
        {
            return;
        }

        for turn in session.turns {
            let reasoning_json: Option<String> = turn.reasoning.clone();
            let tool_calls_json: Option<String> = if turn.tool_calls.is_empty() {
                None
            } else {
                serde_json::to_string(&turn.tool_calls).ok()
            };
            let file_changes_json: Option<String> = if turn.file_changes.is_empty() {
                None
            } else {
                serde_json::to_string(&turn.file_changes).ok()
            };
            let subagents_json: Option<String> = if turn.subagents.is_empty() {
                None
            } else {
                serde_json::to_string(&turn.subagents).ok()
            };
            let plan_json: Option<String> = turn
                .plan
                .as_ref()
                .and_then(|plan| serde_json::to_string(plan).ok());
            let permission_mode_str: Option<String> =
                turn.permission_mode.map(permission_mode_to_str);
            let reasoning_effort_str: Option<String> =
                turn.reasoning_effort.map(|e| e.as_str().to_string());
            let blocks_json: Option<String> = if turn.blocks.is_empty() {
                None
            } else {
                serde_json::to_string(&turn.blocks).ok()
            };
            if transaction
                .execute(
                    "INSERT INTO turns (
                        turn_id, session_id, input, output, status, created_at, updated_at,
                        reasoning_json, tool_calls_json, file_changes_json, subagents_json,
                        plan_json, permission_mode, reasoning_effort, blocks_json
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
                    params![
                        turn.turn_id,
                        session.summary.session_id,
                        turn.input,
                        turn.output,
                        turn_status_to_str(turn.status),
                        turn.created_at,
                        turn.updated_at,
                        reasoning_json,
                        tool_calls_json,
                        file_changes_json,
                        subagents_json,
                        plan_json,
                        permission_mode_str,
                        reasoning_effort_str,
                        blocks_json,
                    ],
                )
                .is_err()
            {
                return;
            }
        }

        let _ = transaction.commit();
    }

    pub async fn get_session(&self, session_id: &str) -> Option<SessionDetail> {
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        load_session(&connection, session_id, None).ok().flatten()
    }

    /// Like `get_session` but returns only the most recent `limit` turns
    /// (in ascending order) when `limit` is `Some`. Used by the paginated
    /// `LoadSession` path so opening a long thread doesn't pay for the
    /// full history on the first render. The `summary.turn_count` stays
    /// set to the session's true turn count so callers can tell when more
    /// older turns exist server-side.
    pub async fn get_session_limited(
        &self,
        session_id: &str,
        limit: Option<usize>,
    ) -> Option<SessionDetail> {
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        load_session(&connection, session_id, limit).ok().flatten()
    }

    pub async fn list_sessions(&self) -> Vec<SessionDetail> {
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        let session_ids = match list_session_ids(&connection) {
            Ok(session_ids) => session_ids,
            Err(_) => return Vec::new(),
        };

        session_ids
            .into_iter()
            .filter_map(|session_id| load_session(&connection, &session_id, None).ok().flatten())
            .collect()
    }

    /// Like `list_sessions` but skips the per-session turns query and JSON
    /// deserialization. Used by the bootstrap path so the sidebar can render
    /// instantly regardless of how much history the user has — the full
    /// turn list for a session is loaded lazily via `get_session` when the
    /// user actually opens it.
    pub async fn list_session_summaries(&self) -> Vec<SessionSummary> {
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        let mut statement = match connection.prepare(
            "SELECT session_id, provider, title, status, created_at, updated_at,
                    last_turn_preview, turn_count, model, project_id
             FROM sessions ORDER BY created_at DESC",
        ) {
            Ok(statement) => statement,
            Err(_) => return Vec::new(),
        };

        let rows = match statement.query_map([], |row| {
            Ok(SessionSummary {
                session_id: row.get(0)?,
                provider: provider_kind_from_str(&row.get::<_, String>(1)?),
                title: row.get(2)?,
                status: session_status_from_str(&row.get::<_, String>(3)?),
                created_at: row.get(4)?,
                updated_at: row.get(5)?,
                last_turn_preview: row.get(6)?,
                turn_count: row.get::<_, i64>(7)? as usize,
                model: row.get(8)?,
                project_id: row.get(9)?,
            })
        }) {
            Ok(rows) => rows,
            Err(_) => return Vec::new(),
        };

        rows.filter_map(Result::ok).collect()
    }

    pub fn delete_session(&self, session_id: &str) -> bool {
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        connection
            .execute("DELETE FROM sessions WHERE session_id = ?1", params![session_id])
            .map(|affected| affected > 0)
            .unwrap_or(false)
    }

    pub fn delete_archived_session(&self, session_id: &str) -> bool {
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        let tx = match connection.unchecked_transaction() {
            Ok(tx) => tx,
            Err(_) => return false,
        };
        let _ = tx.execute(
            "DELETE FROM archived_turns WHERE session_id = ?1",
            params![session_id],
        );
        let removed = tx
            .execute(
                "DELETE FROM archived_sessions WHERE session_id = ?1",
                params![session_id],
            )
            .map(|affected| affected > 0)
            .unwrap_or(false);
        if removed {
            tx.commit().is_ok()
        } else {
            false
        }
    }

    /// Returns the cached models for a provider along with the ISO-8601 timestamp
    /// they were fetched at, or None if no entry exists.
    pub async fn get_cached_models(
        &self,
        kind: ProviderKind,
    ) -> Option<(String, Vec<ProviderModel>)> {
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        connection
            .query_row(
                "SELECT fetched_at, models_json FROM provider_model_cache WHERE provider = ?1",
                params![provider_kind_to_str(kind)],
                |row| {
                    let fetched_at: String = row.get(0)?;
                    let models_json: String = row.get(1)?;
                    Ok((fetched_at, models_json))
                },
            )
            .optional()
            .ok()
            .flatten()
            .and_then(|(fetched_at, json)| {
                serde_json::from_str::<Vec<ProviderModel>>(&json)
                    .ok()
                    .map(|models| (fetched_at, models))
            })
    }

    /// Persist the model list for a provider with `now` as the fetched_at timestamp.
    pub async fn set_cached_models(&self, kind: ProviderKind, models: &[ProviderModel]) {
        let json = match serde_json::to_string(models) {
            Ok(s) => s,
            Err(_) => return,
        };
        let now = chrono::Utc::now().to_rfc3339();
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        let _ = connection.execute(
            "INSERT INTO provider_model_cache (provider, fetched_at, models_json)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(provider) DO UPDATE SET
                fetched_at = excluded.fetched_at,
                models_json = excluded.models_json",
            params![provider_kind_to_str(kind), now, json],
        );
    }

    /// Returns the cached health status for a provider along with the ISO-8601
    /// timestamp it was checked at, or None if no entry exists.
    pub async fn get_cached_health(
        &self,
        kind: ProviderKind,
    ) -> Option<(String, ProviderStatus)> {
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        connection
            .query_row(
                "SELECT checked_at, status_json FROM provider_health_cache WHERE provider = ?1",
                params![provider_kind_to_str(kind)],
                |row| {
                    let checked_at: String = row.get(0)?;
                    let status_json: String = row.get(1)?;
                    Ok((checked_at, status_json))
                },
            )
            .optional()
            .ok()
            .flatten()
            .and_then(|(checked_at, json)| {
                serde_json::from_str::<ProviderStatus>(&json)
                    .ok()
                    .map(|status| (checked_at, status))
            })
    }

    /// Load the full provider-enablement map. Keys are every row in
    /// `provider_enablement`; providers whose kind has no row are treated
    /// as enabled by the caller (runtime-core defaults to `true` on miss).
    pub async fn get_provider_enablement(&self) -> HashMap<ProviderKind, bool> {
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        let mut statement = match connection
            .prepare("SELECT provider, enabled FROM provider_enablement")
        {
            Ok(s) => s,
            Err(_) => return HashMap::new(),
        };
        let rows = statement.query_map([], |row| {
            let provider: String = row.get(0)?;
            let enabled: i64 = row.get(1)?;
            Ok((provider, enabled != 0))
        });
        match rows {
            Ok(iter) => iter
                .filter_map(|r| r.ok())
                .map(|(provider, enabled)| (provider_kind_from_str(&provider), enabled))
                .collect(),
            Err(_) => HashMap::new(),
        }
    }

    /// Upsert a provider's runtime-enabled flag. Called from the
    /// `SetProviderEnabled` handler.
    pub async fn set_provider_enabled(&self, kind: ProviderKind, enabled: bool) {
        let now = chrono::Utc::now().to_rfc3339();
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        let _ = connection.execute(
            "INSERT INTO provider_enablement (provider, enabled, updated_at)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(provider) DO UPDATE SET
                enabled = excluded.enabled,
                updated_at = excluded.updated_at",
            params![provider_kind_to_str(kind), enabled as i64, now],
        );
    }

    /// Persist the health status for a provider with `now` as the checked_at timestamp.
    pub async fn set_cached_health(&self, kind: ProviderKind, status: &ProviderStatus) {
        let json = match serde_json::to_string(status) {
            Ok(s) => s,
            Err(_) => return,
        };
        let now = chrono::Utc::now().to_rfc3339();
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        let _ = connection.execute(
            "INSERT INTO provider_health_cache (provider, checked_at, status_json)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(provider) DO UPDATE SET
                checked_at = excluded.checked_at,
                status_json = excluded.status_json",
            params![provider_kind_to_str(kind), now, json],
        );
    }

    pub async fn list_projects(&self) -> Vec<ProjectRecord> {
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        let mut statement = match connection.prepare(
            "SELECT project_id, name, path, created_at, updated_at, sort_order
             FROM projects
             ORDER BY sort_order ASC, created_at ASC",
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let rows = statement.query_map([], |row| {
            Ok(ProjectRecord {
                project_id: row.get(0)?,
                name: row.get(1)?,
                path: row.get(2)?,
                created_at: row.get(3)?,
                updated_at: row.get(4)?,
                sort_order: row.get(5)?,
            })
        });
        match rows {
            Ok(iter) => iter.filter_map(|r| r.ok()).collect(),
            Err(_) => Vec::new(),
        }
    }

    pub async fn get_project(&self, project_id: &str) -> Option<ProjectRecord> {
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        connection
            .query_row(
                "SELECT project_id, name, path, created_at, updated_at, sort_order
                 FROM projects WHERE project_id = ?1",
                params![project_id],
                |row| {
                    Ok(ProjectRecord {
                        project_id: row.get(0)?,
                        name: row.get(1)?,
                        path: row.get(2)?,
                        created_at: row.get(3)?,
                        updated_at: row.get(4)?,
                        sort_order: row.get(5)?,
                    })
                },
            )
            .optional()
            .ok()
            .flatten()
    }

    pub async fn create_project(&self, name: String, path: Option<String>) -> Option<ProjectRecord> {
        let trimmed = name.trim().to_string();
        if trimmed.is_empty() {
            return None;
        }
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        let project_id = Uuid::new_v4().to_string();
        let now = Utc::now().to_rfc3339();
        // Place new projects at the end.
        let next_order: i32 = connection
            .query_row(
                "SELECT COALESCE(MAX(sort_order), -1) + 1 FROM projects",
                [],
                |row| row.get(0),
            )
            .unwrap_or(0);
        let result = connection.execute(
            "INSERT INTO projects (project_id, name, path, created_at, updated_at, sort_order)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![project_id, trimmed, path, now, now, next_order],
        );
        if result.is_err() {
            return None;
        }
        Some(ProjectRecord {
            project_id,
            name: trimmed,
            path,
            created_at: now.clone(),
            updated_at: now,
            sort_order: next_order,
        })
    }

    pub async fn rename_session(&self, session_id: &str, title: String) -> Option<String> {
        let trimmed = title.trim().to_string();
        if trimmed.is_empty() {
            return None;
        }
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        let now = Utc::now().to_rfc3339();
        let affected = connection
            .execute(
                "UPDATE sessions SET title = ?1, updated_at = ?2 WHERE session_id = ?3",
                params![trimmed, now, session_id],
            )
            .unwrap_or(0);
        if affected == 0 { None } else { Some(now) }
    }

    pub async fn rename_project(&self, project_id: &str, name: String) -> Option<String> {
        let trimmed = name.trim().to_string();
        if trimmed.is_empty() {
            return None;
        }
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        let now = Utc::now().to_rfc3339();
        let affected = connection
            .execute(
                "UPDATE projects SET name = ?1, updated_at = ?2 WHERE project_id = ?3",
                params![trimmed, now, project_id],
            )
            .unwrap_or(0);
        if affected == 0 { None } else { Some(now) }
    }

    /// Deletes a project and null-outs the `project_id` of any session pointing
    /// at it. Returns the list of session IDs that were re-assigned to "unassigned".
    pub async fn delete_project(&self, project_id: &str) -> Option<Vec<String>> {
        let mut connection = self.connection.lock().expect("sqlite mutex poisoned");
        let transaction = connection.transaction().ok()?;
        let reassigned: Vec<String> = {
            let mut stmt = transaction
                .prepare("SELECT session_id FROM sessions WHERE project_id = ?1")
                .ok()?;
            let rows = stmt
                .query_map(params![project_id], |row| row.get::<_, String>(0))
                .ok()?;
            rows.filter_map(|r| r.ok()).collect()
        };
        transaction
            .execute(
                "UPDATE sessions SET project_id = NULL WHERE project_id = ?1",
                params![project_id],
            )
            .ok()?;
        let deleted = transaction
            .execute(
                "DELETE FROM projects WHERE project_id = ?1",
                params![project_id],
            )
            .ok()?;
        if deleted == 0 {
            return None;
        }
        transaction.commit().ok()?;
        Some(reassigned)
    }

    pub async fn assign_session_to_project(
        &self,
        session_id: &str,
        project_id: Option<&str>,
    ) -> bool {
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        connection
            .execute(
                "UPDATE sessions SET project_id = ?1 WHERE session_id = ?2",
                params![project_id, session_id],
            )
            .map(|affected| affected > 0)
            .unwrap_or(false)
    }

    fn migrate(&self) -> Result<()> {
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        connection
            .execute_batch(
                "
            PRAGMA journal_mode = WAL;

            CREATE TABLE IF NOT EXISTS sessions (
                session_id TEXT PRIMARY KEY,
                provider TEXT NOT NULL,
                title TEXT NOT NULL,
                status TEXT NOT NULL,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                last_turn_preview TEXT,
                turn_count INTEGER NOT NULL,
                provider_state_json TEXT
            );

            CREATE TABLE IF NOT EXISTS turns (
                turn_id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                input TEXT NOT NULL,
                output TEXT NOT NULL,
                status TEXT NOT NULL,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                FOREIGN KEY(session_id) REFERENCES sessions(session_id) ON DELETE CASCADE
            );

            CREATE INDEX IF NOT EXISTS idx_turns_session_id ON turns(session_id);

            CREATE TABLE IF NOT EXISTS provider_model_cache (
                provider TEXT PRIMARY KEY,
                fetched_at TEXT NOT NULL,
                models_json TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS projects (
                project_id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                sort_order INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS provider_health_cache (
                provider TEXT PRIMARY KEY,
                checked_at TEXT NOT NULL,
                status_json TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS provider_enablement (
                provider TEXT PRIMARY KEY,
                enabled INTEGER NOT NULL DEFAULT 1,
                updated_at TEXT NOT NULL
            );
            ",
            )
            .context("failed to run sqlite migrations")?;

        // Idempotent column additions — ignore errors if the column already exists.
        let _ = connection.execute("ALTER TABLE sessions ADD COLUMN provider_state_json TEXT", []);
        let _ = connection.execute("ALTER TABLE sessions ADD COLUMN model TEXT", []);
        let _ = connection.execute("ALTER TABLE sessions ADD COLUMN project_id TEXT", []);
        let _ = connection.execute("ALTER TABLE turns ADD COLUMN reasoning_json TEXT", []);
        let _ = connection.execute("ALTER TABLE turns ADD COLUMN tool_calls_json TEXT", []);
        let _ = connection.execute("ALTER TABLE turns ADD COLUMN file_changes_json TEXT", []);
        let _ = connection.execute("ALTER TABLE turns ADD COLUMN subagents_json TEXT", []);
        let _ = connection.execute("ALTER TABLE turns ADD COLUMN plan_json TEXT", []);
        let _ = connection.execute("ALTER TABLE turns ADD COLUMN permission_mode TEXT", []);
        let _ = connection.execute("ALTER TABLE turns ADD COLUMN reasoning_effort TEXT", []);
        let _ = connection.execute("ALTER TABLE turns ADD COLUMN blocks_json TEXT", []);
        let _ = connection.execute("ALTER TABLE archived_turns ADD COLUMN blocks_json TEXT", []);
        let _ = connection.execute("ALTER TABLE projects ADD COLUMN path TEXT", []);

        // Archived session/turn tables — same schema, plus archived_at timestamp.
        let _ = connection.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS archived_sessions (
                session_id TEXT PRIMARY KEY,
                provider TEXT NOT NULL,
                title TEXT NOT NULL,
                status TEXT NOT NULL,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                last_turn_preview TEXT,
                turn_count INTEGER NOT NULL,
                provider_state_json TEXT,
                model TEXT,
                project_id TEXT,
                archived_at TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS archived_turns (
                turn_id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                input TEXT NOT NULL,
                output TEXT NOT NULL,
                status TEXT NOT NULL,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                reasoning_json TEXT,
                tool_calls_json TEXT,
                file_changes_json TEXT,
                subagents_json TEXT,
                plan_json TEXT,
                permission_mode TEXT,
                reasoning_effort TEXT,
                blocks_json TEXT,
                FOREIGN KEY(session_id) REFERENCES archived_sessions(session_id) ON DELETE CASCADE
            );

            CREATE INDEX IF NOT EXISTS idx_archived_turns_session_id ON archived_turns(session_id);
            ",
        );

        Ok(())
    }

    pub async fn archive_session(&self, session_id: &str) -> bool {
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        let now = Utc::now().to_rfc3339();
        let tx = match connection.unchecked_transaction() {
            Ok(tx) => tx,
            Err(_) => return false,
        };

        let moved = tx
            .execute(
                "INSERT INTO archived_sessions
                    (session_id, provider, title, status, created_at, updated_at,
                     last_turn_preview, turn_count, provider_state_json, model, project_id, archived_at)
                 SELECT session_id, provider, title, status, created_at, updated_at,
                        last_turn_preview, turn_count, provider_state_json, model, project_id, ?1
                 FROM sessions WHERE session_id = ?2",
                params![now, session_id],
            )
            .unwrap_or(0);

        if moved == 0 {
            return false;
        }

        let _ = tx.execute(
            "INSERT INTO archived_turns
                (turn_id, session_id, input, output, status, created_at, updated_at,
                 reasoning_json, tool_calls_json, file_changes_json, subagents_json,
                 plan_json, permission_mode, reasoning_effort, blocks_json)
             SELECT turn_id, session_id, input, output, status, created_at, updated_at,
                    reasoning_json, tool_calls_json, file_changes_json, subagents_json,
                    plan_json, permission_mode, reasoning_effort, blocks_json
             FROM turns WHERE session_id = ?1",
            params![session_id],
        );
        let _ = tx.execute("DELETE FROM turns WHERE session_id = ?1", params![session_id]);
        let _ = tx.execute("DELETE FROM sessions WHERE session_id = ?1", params![session_id]);
        let _ = tx.commit();
        true
    }

    pub async fn unarchive_session(&self, session_id: &str) -> Option<SessionDetail> {
        let success = {
            let connection = self.connection.lock().expect("sqlite mutex poisoned");
            let tx = match connection.unchecked_transaction() {
                Ok(tx) => tx,
                Err(_) => return None,
            };

            let moved = tx
                .execute(
                    "INSERT INTO sessions
                        (session_id, provider, title, status, created_at, updated_at,
                         last_turn_preview, turn_count, provider_state_json, model, project_id)
                     SELECT session_id, provider, title, status, created_at, updated_at,
                            last_turn_preview, turn_count, provider_state_json, model, project_id
                     FROM archived_sessions WHERE session_id = ?1",
                    params![session_id],
                )
                .unwrap_or(0);

            if moved == 0 {
                return None;
            }

            let _ = tx.execute(
                "INSERT INTO turns
                    (turn_id, session_id, input, output, status, created_at, updated_at,
                     reasoning_json, tool_calls_json, file_changes_json, subagents_json,
                     plan_json, permission_mode, reasoning_effort, blocks_json)
                 SELECT turn_id, session_id, input, output, status, created_at, updated_at,
                        reasoning_json, tool_calls_json, file_changes_json, subagents_json,
                        plan_json, permission_mode, reasoning_effort, blocks_json
                 FROM archived_turns WHERE session_id = ?1",
                params![session_id],
            );
            let _ = tx.execute("DELETE FROM archived_turns WHERE session_id = ?1", params![session_id]);
            let _ = tx.execute("DELETE FROM archived_sessions WHERE session_id = ?1", params![session_id]);
            tx.commit().is_ok()
        }; // connection + tx dropped here

        if success {
            self.get_session(session_id).await
        } else {
            None
        }
    }

    pub async fn list_archived_session_summaries(&self) -> Vec<SessionSummary> {
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        let mut stmt = match connection.prepare(
            "SELECT session_id, provider, title, status, created_at, updated_at,
                    last_turn_preview, turn_count, model, project_id
             FROM archived_sessions ORDER BY created_at DESC",
        ) {
            Ok(stmt) => stmt,
            Err(_) => return Vec::new(),
        };

        stmt.query_map([], |row| {
            Ok(SessionSummary {
                session_id: row.get(0)?,
                provider: provider_kind_from_str(&row.get::<_, String>(1)?),
                title: row.get(2)?,
                status: session_status_from_str(&row.get::<_, String>(3)?),
                created_at: row.get(4)?,
                updated_at: row.get(5)?,
                last_turn_preview: row.get(6)?,
                turn_count: row.get::<_, i64>(7)? as usize,
                model: row.get(8)?,
                project_id: row.get(9)?,
            })
        })
        .map(|rows| rows.filter_map(|r| r.ok()).collect())
        .unwrap_or_default()
    }
}

fn list_session_ids(connection: &Connection) -> Result<Vec<String>> {
    let mut statement = connection
        .prepare("SELECT session_id FROM sessions ORDER BY created_at DESC")
        .context("failed to prepare session list query")?;
    let rows = statement
        .query_map([], |row| row.get::<_, String>(0))
        .context("failed to query session ids")?;

    rows.collect::<rusqlite::Result<Vec<_>>>()
        .context("failed to collect session ids")
}

fn load_session(
    connection: &Connection,
    session_id: &str,
    limit: Option<usize>,
) -> Result<Option<SessionDetail>> {
    let summary = connection
        .query_row(
            "SELECT session_id, provider, title, status, created_at, updated_at, last_turn_preview, turn_count, provider_state_json, model, project_id
             FROM sessions WHERE session_id = ?1",
            params![session_id],
            |row| {
                Ok(SessionSummary {
                    session_id: row.get(0)?,
                    provider: provider_kind_from_str(&row.get::<_, String>(1)?),
                    title: row.get(2)?,
                    status: session_status_from_str(&row.get::<_, String>(3)?),
                    created_at: row.get(4)?,
                    updated_at: row.get(5)?,
                    last_turn_preview: row.get(6)?,
                    turn_count: row.get::<_, i64>(7)? as usize,
                    model: row.get(9)?,
                    project_id: row.get(10)?,
                })
            },
        )
        .optional()
        .context("failed to load session summary")?;

    let Some(summary) = summary else {
        return Ok(None);
    };

    // When limit is absent we load the full history in ascending order
    // (what the rest of the runtime expects). When limit is present we
    // flip the SQL to `ORDER BY created_at DESC LIMIT n` — this is the
    // only way sqlite will actually touch only the tail of the `turns`
    // table — and then reverse the resulting Vec so the caller still
    // sees turns in chronological order.
    let (sql, turn_limit_param): (String, Option<i64>) = if let Some(n) = limit {
        (
            "SELECT turn_id, input, output, status, created_at, updated_at, reasoning_json,
                    tool_calls_json, file_changes_json, subagents_json, plan_json, permission_mode,
                    reasoning_effort, blocks_json
             FROM turns WHERE session_id = ?1 ORDER BY created_at DESC LIMIT ?2"
                .to_string(),
            Some(n as i64),
        )
    } else {
        (
            "SELECT turn_id, input, output, status, created_at, updated_at, reasoning_json,
                    tool_calls_json, file_changes_json, subagents_json, plan_json, permission_mode,
                    reasoning_effort, blocks_json
             FROM turns WHERE session_id = ?1 ORDER BY created_at ASC"
                .to_string(),
            None,
        )
    };
    let mut statement = connection
        .prepare(&sql)
        .context("failed to prepare turn query")?;
    let row_mapper = |row: &rusqlite::Row<'_>| -> rusqlite::Result<TurnRecord> {
        let output: String = row.get(2)?;
        let reasoning: Option<String> = row.get(6)?;
        let tool_calls_json: Option<String> = row.get(7)?;
        let file_changes_json: Option<String> = row.get(8)?;
        let subagents_json: Option<String> = row.get(9)?;
        let plan_json: Option<String> = row.get(10)?;
        let permission_mode_str: Option<String> = row.get(11)?;
        let reasoning_effort_str: Option<String> = row.get(12)?;
        let blocks_json: Option<String> = row.get(13)?;
        let tool_calls: Vec<ToolCall> = tool_calls_json
            .and_then(|j| serde_json::from_str(&j).ok())
            .unwrap_or_default();
        let file_changes: Vec<FileChangeRecord> = file_changes_json
            .and_then(|j| serde_json::from_str(&j).ok())
            .unwrap_or_default();
        let subagents: Vec<SubagentRecord> = subagents_json
            .and_then(|j| serde_json::from_str(&j).ok())
            .unwrap_or_default();
        let plan: Option<PlanRecord> = plan_json.and_then(|j| serde_json::from_str(&j).ok());
        let permission_mode: Option<PermissionMode> =
            permission_mode_str.as_deref().map(permission_mode_from_str);
        let reasoning_effort: Option<ReasoningEffort> = reasoning_effort_str
            .as_deref()
            .and_then(reasoning_effort_from_str);
        // Prefer the persisted ordered blocks. For historical rows
        // (no blocks_json), synthesize a plausible block list from
        // the legacy columns: reasoning, then text, then tool calls,
        // matching the old text-then-tools UI so historic turns keep
        // rendering through the same code path as new ones.
        let blocks: Vec<ContentBlock> = blocks_json
            .and_then(|j| serde_json::from_str(&j).ok())
            .unwrap_or_else(|| synthesize_blocks(reasoning.as_deref(), &output, &tool_calls));
        Ok(TurnRecord {
            turn_id: row.get(0)?,
            input: row.get(1)?,
            output,
            status: turn_status_from_str(&row.get::<_, String>(3)?),
            created_at: row.get(4)?,
            updated_at: row.get(5)?,
            reasoning,
            tool_calls,
            file_changes,
            subagents,
            plan,
            permission_mode,
            reasoning_effort,
            blocks,
        })
    };
    let mut turns: Vec<TurnRecord> = match turn_limit_param {
        Some(n) => statement
            .query_map(params![session_id, n], row_mapper)
            .context("failed to query turns")?
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("failed to collect turns")?,
        None => statement
            .query_map(params![session_id], row_mapper)
            .context("failed to query turns")?
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("failed to collect turns")?,
    };
    // Limited queries come out of sqlite in descending order so the
    // LIMIT clause picks the tail. Flip to ascending here so the
    // caller sees a single consistent ordering regardless of which
    // SQL branch ran.
    if turn_limit_param.is_some() {
        turns.reverse();
    }

    let provider_state = connection
        .query_row(
            "SELECT provider_state_json FROM sessions WHERE session_id = ?1",
            params![session_id],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()
        .context("failed to load provider session state")?
        .flatten()
        .and_then(|json| serde_json::from_str(&json).ok());

    Ok(Some(SessionDetail {
        summary,
        turns,
        provider_state,
        cwd: None,
    }))
}

fn provider_kind_to_str(kind: ProviderKind) -> &'static str {
    match kind {
        ProviderKind::Codex => "codex",
        ProviderKind::Claude => "claude",
        ProviderKind::GitHubCopilot => "github_copilot",
        ProviderKind::ClaudeCli => "claude_cli",
        ProviderKind::GitHubCopilotCli => "github_copilot_cli",
    }
}

fn provider_kind_from_str(value: &str) -> ProviderKind {
    match value {
        "claude" => ProviderKind::Claude,
        "github_copilot" => ProviderKind::GitHubCopilot,
        "claude_cli" => ProviderKind::ClaudeCli,
        "github_copilot_cli" => ProviderKind::GitHubCopilotCli,
        _ => ProviderKind::Codex,
    }
}

fn session_status_to_str(status: SessionStatus) -> &'static str {
    match status {
        SessionStatus::Ready => "ready",
        SessionStatus::Running => "running",
        SessionStatus::Interrupted => "interrupted",
    }
}

fn session_status_from_str(value: &str) -> SessionStatus {
    match value {
        "running" => SessionStatus::Running,
        "interrupted" => SessionStatus::Interrupted,
        _ => SessionStatus::Ready,
    }
}

fn turn_status_to_str(status: TurnStatus) -> &'static str {
    match status {
        TurnStatus::Running => "running",
        TurnStatus::Completed => "completed",
        TurnStatus::Interrupted => "interrupted",
        TurnStatus::Failed => "failed",
    }
}

fn turn_status_from_str(value: &str) -> TurnStatus {
    match value {
        "running" => TurnStatus::Running,
        "interrupted" => TurnStatus::Interrupted,
        "failed" => TurnStatus::Failed,
        _ => TurnStatus::Completed,
    }
}

fn permission_mode_to_str(mode: PermissionMode) -> String {
    match mode {
        PermissionMode::Default => "default",
        PermissionMode::AcceptEdits => "accept_edits",
        PermissionMode::Plan => "plan",
        PermissionMode::Bypass => "bypass",
    }
    .to_string()
}

fn permission_mode_from_str(value: &str) -> PermissionMode {
    match value {
        "default" => PermissionMode::Default,
        "plan" => PermissionMode::Plan,
        "bypass" => PermissionMode::Bypass,
        _ => PermissionMode::AcceptEdits,
    }
}

/// Reconstruct an ordered block list for a historical turn that was
/// persisted before `blocks_json` existed. Layout matches the old UI:
/// reasoning fold-open first, then the text body, then any tool calls.
/// Not perfect, but stable and consistent across reloads.
fn synthesize_blocks(
    reasoning: Option<&str>,
    output: &str,
    tool_calls: &[ToolCall],
) -> Vec<ContentBlock> {
    let mut blocks: Vec<ContentBlock> = Vec::new();
    if let Some(text) = reasoning {
        if !text.is_empty() {
            blocks.push(ContentBlock::Reasoning {
                text: text.to_string(),
            });
        }
    }
    if !output.is_empty() {
        blocks.push(ContentBlock::Text {
            text: output.to_string(),
        });
    }
    for tc in tool_calls {
        blocks.push(ContentBlock::ToolCall {
            call_id: tc.call_id.clone(),
        });
    }
    blocks
}

fn reasoning_effort_from_str(value: &str) -> Option<ReasoningEffort> {
    match value {
        "minimal" => Some(ReasoningEffort::Minimal),
        "low" => Some(ReasoningEffort::Low),
        "medium" => Some(ReasoningEffort::Medium),
        "high" => Some(ReasoningEffort::High),
        _ => None,
    }
}
