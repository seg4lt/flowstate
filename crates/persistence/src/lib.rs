use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use zenui_provider_api::{
    FileChangeRecord, PermissionMode, PlanRecord, ProviderKind, ProviderModel, SessionDetail,
    SessionStatus, SessionSummary, SubagentRecord, ToolCall, TurnRecord, TurnStatus,
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
                    session_id, provider, title, status, created_at, updated_at, last_turn_preview, turn_count, provider_state_json, model
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
                 ON CONFLICT(session_id) DO UPDATE SET
                    provider = excluded.provider,
                    title = excluded.title,
                    status = excluded.status,
                    created_at = excluded.created_at,
                    updated_at = excluded.updated_at,
                    last_turn_preview = excluded.last_turn_preview,
                    turn_count = excluded.turn_count,
                    provider_state_json = excluded.provider_state_json,
                    model = excluded.model",
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
            if transaction
                .execute(
                    "INSERT INTO turns (
                        turn_id, session_id, input, output, status, created_at, updated_at,
                        reasoning_json, tool_calls_json, file_changes_json, subagents_json,
                        plan_json, permission_mode
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
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
        load_session(&connection, session_id).ok().flatten()
    }

    pub async fn list_sessions(&self) -> Vec<SessionDetail> {
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        let session_ids = match list_session_ids(&connection) {
            Ok(session_ids) => session_ids,
            Err(_) => return Vec::new(),
        };

        session_ids
            .into_iter()
            .filter_map(|session_id| load_session(&connection, &session_id).ok().flatten())
            .collect()
    }

    pub fn delete_session(&self, session_id: &str) -> bool {
        let connection = self.connection.lock().expect("sqlite mutex poisoned");
        connection
            .execute("DELETE FROM sessions WHERE session_id = ?1", params![session_id])
            .map(|affected| affected > 0)
            .unwrap_or(false)
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
            ",
            )
            .context("failed to run sqlite migrations")?;

        // Idempotent column additions — ignore errors if the column already exists.
        let _ = connection.execute("ALTER TABLE sessions ADD COLUMN provider_state_json TEXT", []);
        let _ = connection.execute("ALTER TABLE sessions ADD COLUMN model TEXT", []);
        let _ = connection.execute("ALTER TABLE turns ADD COLUMN reasoning_json TEXT", []);
        let _ = connection.execute("ALTER TABLE turns ADD COLUMN tool_calls_json TEXT", []);
        let _ = connection.execute("ALTER TABLE turns ADD COLUMN file_changes_json TEXT", []);
        let _ = connection.execute("ALTER TABLE turns ADD COLUMN subagents_json TEXT", []);
        let _ = connection.execute("ALTER TABLE turns ADD COLUMN plan_json TEXT", []);
        let _ = connection.execute("ALTER TABLE turns ADD COLUMN permission_mode TEXT", []);

        Ok(())
    }
}

fn list_session_ids(connection: &Connection) -> Result<Vec<String>> {
    let mut statement = connection
        .prepare("SELECT session_id FROM sessions ORDER BY updated_at DESC")
        .context("failed to prepare session list query")?;
    let rows = statement
        .query_map([], |row| row.get::<_, String>(0))
        .context("failed to query session ids")?;

    rows.collect::<rusqlite::Result<Vec<_>>>()
        .context("failed to collect session ids")
}

fn load_session(connection: &Connection, session_id: &str) -> Result<Option<SessionDetail>> {
    let summary = connection
        .query_row(
            "SELECT session_id, provider, title, status, created_at, updated_at, last_turn_preview, turn_count, provider_state_json, model
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
                })
            },
        )
        .optional()
        .context("failed to load session summary")?;

    let Some(summary) = summary else {
        return Ok(None);
    };

    let mut statement = connection
        .prepare(
            "SELECT turn_id, input, output, status, created_at, updated_at, reasoning_json,
                    tool_calls_json, file_changes_json, subagents_json, plan_json, permission_mode
             FROM turns WHERE session_id = ?1 ORDER BY created_at ASC",
        )
        .context("failed to prepare turn query")?;
    let turns = statement
        .query_map(params![session_id], |row| {
            let reasoning: Option<String> = row.get(6)?;
            let tool_calls_json: Option<String> = row.get(7)?;
            let file_changes_json: Option<String> = row.get(8)?;
            let subagents_json: Option<String> = row.get(9)?;
            let plan_json: Option<String> = row.get(10)?;
            let permission_mode_str: Option<String> = row.get(11)?;
            let tool_calls: Vec<ToolCall> = tool_calls_json
                .and_then(|j| serde_json::from_str(&j).ok())
                .unwrap_or_default();
            let file_changes: Vec<FileChangeRecord> = file_changes_json
                .and_then(|j| serde_json::from_str(&j).ok())
                .unwrap_or_default();
            let subagents: Vec<SubagentRecord> = subagents_json
                .and_then(|j| serde_json::from_str(&j).ok())
                .unwrap_or_default();
            let plan: Option<PlanRecord> =
                plan_json.and_then(|j| serde_json::from_str(&j).ok());
            let permission_mode: Option<PermissionMode> =
                permission_mode_str.as_deref().map(permission_mode_from_str);
            Ok(TurnRecord {
                turn_id: row.get(0)?,
                input: row.get(1)?,
                output: row.get(2)?,
                status: turn_status_from_str(&row.get::<_, String>(3)?),
                created_at: row.get(4)?,
                updated_at: row.get(5)?,
                reasoning,
                tool_calls,
                file_changes,
                subagents,
                plan,
                permission_mode,
            })
        })
        .context("failed to query turns")?
        .collect::<rusqlite::Result<Vec<_>>>()
        .context("failed to collect turns")?;

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
    }))
}

fn provider_kind_to_str(kind: ProviderKind) -> &'static str {
    match kind {
        ProviderKind::Codex => "codex",
        ProviderKind::Claude => "claude",
        ProviderKind::GitHubCopilot => "github_copilot",
    }
}

fn provider_kind_from_str(value: &str) -> ProviderKind {
    match value {
        "claude" => ProviderKind::Claude,
        "github_copilot" => ProviderKind::GitHubCopilot,
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
