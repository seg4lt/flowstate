use std::collections::HashMap;
use std::path::{Path, PathBuf};

// `parking_lot::Mutex` has no poisoning semantics, so a panic inside a
// query callback no longer wedges every subsequent `.lock()` call on
// the sqlite connection. Previously a single bad row or a malformed
// JSON blob in one record could take the whole daemon down via the
// cascade of `.expect("sqlite mutex poisoned")` calls that used to
// line this file.
use parking_lot::Mutex;

use anyhow::{Context, Result};
use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use chrono::Utc;
use rusqlite::{Connection, OptionalExtension, params};
use uuid::Uuid;
use zenui_provider_api::{
    AttachmentData, AttachmentRef, ContentBlock, FileChangeRecord, PermissionMode, PlanRecord,
    ProjectRecord, ProviderKind, ProviderModel, ProviderStatus, ReasoningEffort, SessionDetail,
    SessionSummary, SubagentRecord, ToolCall, TurnRecord,
};

/// Runtime-scoped file I/O used by the rewind / file-checkpoint flow.
///
/// Extracted as a trait so `runtime-core` can express the intent
/// ("restore this file to its earlier contents") without depending on
/// `tokio::fs` directly. The concrete impl on [`PersistenceService`]
/// is the only consumer today; tests can plug in a mock to assert
/// which paths would have been written without actually touching the
/// filesystem.
#[async_trait]
pub trait FileCheckpointStore: Send + Sync {
    async fn write_file(&self, path: &Path, contents: &[u8]) -> std::io::Result<()>;
    async fn remove_file(&self, path: &Path) -> std::io::Result<()>;
}

/// Hard cap on the size of a single image attachment, in bytes. Mirrors
/// the frontend cap so an oversized payload is rejected before we touch
/// the disk.
pub const ATTACHMENT_MAX_BYTES: usize = 5 * 1024 * 1024;

/// Allowed image MIME types. Anything else is rejected at the runtime
/// boundary so the on-disk file extension is always one of these.
pub const ATTACHMENT_ALLOWED_MEDIA_TYPES: &[&str] = &[
    "image/png",
    "image/jpeg",
    "image/gif",
    "image/webp",
];

mod codecs;

use codecs::{
    ext_for_media_type, permission_mode_from_str, permission_mode_to_str, provider_kind_from_str,
    reasoning_effort_from_str, session_status_from_str, session_status_to_str, synthesize_blocks,
    turn_status_from_str, turn_status_to_str,
};

#[derive(Debug)]
pub struct PersistenceService {
    connection: Mutex<Connection>,
    attachments_dir: PathBuf,
}

#[async_trait]
impl FileCheckpointStore for PersistenceService {
    async fn write_file(&self, path: &Path, contents: &[u8]) -> std::io::Result<()> {
        tokio::fs::write(path, contents).await
    }

    async fn remove_file(&self, path: &Path) -> std::io::Result<()> {
        tokio::fs::remove_file(path).await
    }
}

impl PersistenceService {
    pub fn new(path: impl AsRef<Path>) -> Result<Self> {
        let parent = path
            .as_ref()
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        std::fs::create_dir_all(&parent).context("failed to create persistence directory")?;
        let attachments_dir = parent.join("attachments");
        std::fs::create_dir_all(&attachments_dir)
            .context("failed to create attachments directory")?;

        let connection = Connection::open(path).context("failed to open sqlite database")?;
        let service = Self {
            connection: Mutex::new(connection),
            attachments_dir,
        };
        service.migrate()?;
        Ok(service)
    }

    pub fn in_memory() -> Result<Self> {
        // Tests don't typically exercise the file-backed attachment
        // path; give them a unique tempdir so any test that does write
        // an attachment doesn't collide with sibling tests.
        let attachments_dir = std::env::temp_dir()
            .join(format!("zenui-test-attachments-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&attachments_dir)
            .context("failed to create in-memory attachments directory")?;
        let connection = Connection::open_in_memory().context("failed to open in-memory sqlite")?;
        let service = Self {
            connection: Mutex::new(connection),
            attachments_dir,
        };
        service.migrate()?;
        Ok(service)
    }

    /// Directory where attachment files are written. Located alongside
    /// the SQLite database; created on construction.
    pub fn attachments_dir(&self) -> &Path {
        &self.attachments_dir
    }

    pub async fn upsert_session(&self, session: SessionDetail) {
        let mut connection = self.connection.lock();
        let transaction = match connection.transaction() {
            Ok(transaction) => transaction,
            Err(_) => return,
        };

        if transaction
            .execute(
                "INSERT INTO sessions (
                    session_id, provider, status, created_at, updated_at, turn_count, provider_state_json, model, project_id
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                 ON CONFLICT(session_id) DO UPDATE SET
                    provider = excluded.provider,
                    status = excluded.status,
                    created_at = excluded.created_at,
                    updated_at = excluded.updated_at,
                    turn_count = excluded.turn_count,
                    provider_state_json = excluded.provider_state_json,
                    model = excluded.model,
                    project_id = excluded.project_id",
                params![
                    session.summary.session_id,
                    session.summary.provider.as_tag(),
                    session_status_to_str(session.summary.status),
                    session.summary.created_at,
                    session.summary.updated_at,
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
        let connection = self.connection.lock();
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
        let connection = self.connection.lock();
        load_session(&connection, session_id, limit).ok().flatten()
    }

    pub async fn list_sessions(&self) -> Vec<SessionDetail> {
        let connection = self.connection.lock();
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
        let connection = self.connection.lock();
        let mut statement = match connection.prepare(
            "SELECT session_id, provider, status, created_at, updated_at,
                    turn_count, model, project_id
             FROM sessions ORDER BY created_at DESC",
        ) {
            Ok(statement) => statement,
            Err(_) => return Vec::new(),
        };

        let rows = match statement.query_map([], |row| {
            Ok(SessionSummary {
                session_id: row.get(0)?,
                provider: provider_kind_from_str(&row.get::<_, String>(1)?),
                status: session_status_from_str(&row.get::<_, String>(2)?),
                created_at: row.get(3)?,
                updated_at: row.get(4)?,
                turn_count: row.get::<_, i64>(5)? as usize,
                model: row.get(6)?,
                project_id: row.get(7)?,
            })
        }) {
            Ok(rows) => rows,
            Err(_) => return Vec::new(),
        };

        rows.filter_map(Result::ok).collect()
    }

    pub fn delete_session(&self, session_id: &str) -> bool {
        // Best-effort attachment cleanup before the session row delete.
        // The `turns` rows cascade off the FK so they're handled by
        // sqlite below; attachment files don't have a FK so we delete
        // them here explicitly.
        self.delete_attachments_for_session_blocking(session_id);
        let connection = self.connection.lock();
        connection
            .execute("DELETE FROM sessions WHERE session_id = ?1", params![session_id])
            .map(|affected| affected > 0)
            .unwrap_or(false)
    }

    pub fn delete_archived_session(&self, session_id: &str) -> bool {
        // Best-effort attachment cleanup before the row delete.
        self.delete_attachments_for_session_blocking(session_id);
        let connection = self.connection.lock();
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

    /// Persist a single image attachment to disk and record its
    /// metadata in `turn_attachments`. Decodes the base64 payload,
    /// validates the size + media type, writes the file, then inserts
    /// the row. Returns the lightweight reference the frontend will
    /// see on session load.
    pub async fn write_attachment(
        &self,
        session_id: &str,
        turn_id: &str,
        media_type: &str,
        name: Option<&str>,
        data_base64: &str,
    ) -> Result<AttachmentRef, String> {
        if !ATTACHMENT_ALLOWED_MEDIA_TYPES.contains(&media_type) {
            return Err(format!("unsupported image media type: {media_type}"));
        }
        let bytes = BASE64_STANDARD
            .decode(data_base64.as_bytes())
            .map_err(|e| format!("base64 decode failed: {e}"))?;
        if bytes.len() > ATTACHMENT_MAX_BYTES {
            return Err(format!(
                "attachment exceeds {} byte limit ({} bytes)",
                ATTACHMENT_MAX_BYTES,
                bytes.len()
            ));
        }

        let id = Uuid::new_v4().to_string();
        let ext = ext_for_media_type(media_type);
        let file_path = self.attachments_dir.join(format!("{id}.{ext}"));
        std::fs::write(&file_path, &bytes)
            .map_err(|e| format!("failed to write attachment file: {e}"))?;

        let now = Utc::now().to_rfc3339();
        let size_bytes = bytes.len() as i64;
        {
            let connection = self.connection.lock();
            if let Err(e) = connection.execute(
                "INSERT INTO turn_attachments
                    (id, turn_id, session_id, media_type, name, size_bytes, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![id, turn_id, session_id, media_type, name, size_bytes, now],
            ) {
                // Roll back the on-disk file so we don't leak it.
                let _ = std::fs::remove_file(&file_path);
                return Err(format!("failed to insert attachment row: {e}"));
            }
        }

        Ok(AttachmentRef {
            id,
            media_type: media_type.to_string(),
            name: name.map(str::to_string),
            size_bytes: size_bytes as u64,
        })
    }

    /// Read the full bytes of a persisted attachment back from disk.
    /// Called only on user click — never on session load.
    pub async fn read_attachment(&self, attachment_id: &str) -> Result<AttachmentData, String> {
        let (media_type, name) = {
            let connection = self.connection.lock();
            connection
                .query_row(
                    "SELECT media_type, name FROM turn_attachments WHERE id = ?1",
                    params![attachment_id],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, Option<String>>(1)?,
                        ))
                    },
                )
                .optional()
                .map_err(|e| format!("failed to look up attachment: {e}"))?
                .ok_or_else(|| format!("attachment {attachment_id} not found"))?
        };

        let ext = ext_for_media_type(&media_type);
        let file_path = self.attachments_dir.join(format!("{attachment_id}.{ext}"));
        let bytes = std::fs::read(&file_path)
            .map_err(|e| format!("failed to read attachment file: {e}"))?;
        let data_base64 = BASE64_STANDARD.encode(&bytes);
        Ok(AttachmentData {
            media_type,
            data_base64,
            name,
        })
    }

    /// Fetch all attachment refs for a single turn, ordered oldest
    /// first. Used by the session-load path to hydrate
    /// `TurnRecord.input_attachments` without reading any file bytes.
    pub fn list_attachments_for_turn(&self, turn_id: &str) -> Vec<AttachmentRef> {
        let connection = self.connection.lock();
        let mut stmt = match connection.prepare(
            "SELECT id, media_type, name, size_bytes
             FROM turn_attachments WHERE turn_id = ?1 ORDER BY created_at ASC",
        ) {
            Ok(stmt) => stmt,
            Err(_) => return Vec::new(),
        };
        let rows = match stmt.query_map(params![turn_id], |row| {
            Ok(AttachmentRef {
                id: row.get(0)?,
                media_type: row.get(1)?,
                name: row.get(2)?,
                size_bytes: row.get::<_, i64>(3)? as u64,
            })
        }) {
            Ok(rows) => rows,
            Err(_) => return Vec::new(),
        };
        rows.filter_map(Result::ok).collect()
    }

    /// Synchronous best-effort cleanup of all attachment files + rows
    /// for a session. Called from the synchronous `delete_session` /
    /// `delete_archived_session` paths. Failed unlinks are logged and
    /// the DB row delete proceeds regardless — a stranded file on
    /// disk is preferable to a half-deleted session.
    fn delete_attachments_for_session_blocking(&self, session_id: &str) {
        let rows: Vec<(String, String)> = {
            let connection = self.connection.lock();
            let mut stmt = match connection.prepare(
                "SELECT id, media_type FROM turn_attachments WHERE session_id = ?1",
            ) {
                Ok(stmt) => stmt,
                Err(_) => return,
            };
            let mapped = stmt.query_map(params![session_id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            });
            match mapped {
                Ok(iter) => iter.filter_map(Result::ok).collect(),
                Err(_) => return,
            }
        };
        for (id, media_type) in &rows {
            let ext = ext_for_media_type(media_type);
            let file_path = self.attachments_dir.join(format!("{id}.{ext}"));
            if let Err(e) = std::fs::remove_file(&file_path) {
                tracing::warn!(
                    attachment_id = %id,
                    path = %file_path.display(),
                    error = %e,
                    "failed to unlink attachment file during session delete"
                );
            }
        }
        let connection = self.connection.lock();
        let _ = connection.execute(
            "DELETE FROM turn_attachments WHERE session_id = ?1",
            params![session_id],
        );
    }

    /// Returns the cached models for a provider along with the ISO-8601 timestamp
    /// they were fetched at, or None if no entry exists.
    pub async fn get_cached_models(
        &self,
        kind: ProviderKind,
    ) -> Option<(String, Vec<ProviderModel>)> {
        let connection = self.connection.lock();
        connection
            .query_row(
                "SELECT fetched_at, models_json FROM provider_model_cache WHERE provider = ?1",
                params![kind.as_tag()],
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
        self.set_cached_models_at(kind, models, &chrono::Utc::now().to_rfc3339())
            .await;
    }

    /// Like `set_cached_models` but takes an explicit `fetched_at` timestamp.
    /// Used by tests that need to forge a stale cache entry; production code
    /// should stick to `set_cached_models`.
    pub async fn set_cached_models_at(
        &self,
        kind: ProviderKind,
        models: &[ProviderModel],
        fetched_at: &str,
    ) {
        let json = match serde_json::to_string(models) {
            Ok(s) => s,
            Err(_) => return,
        };
        let connection = self.connection.lock();
        let _ = connection.execute(
            "INSERT INTO provider_model_cache (provider, fetched_at, models_json)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(provider) DO UPDATE SET
                fetched_at = excluded.fetched_at,
                models_json = excluded.models_json",
            params![kind.as_tag(), fetched_at, json],
        );
    }

    /// Returns the cached health status for a provider along with the ISO-8601
    /// timestamp it was checked at, or None if no entry exists.
    ///
    /// The `features` field on the returned status is always overwritten
    /// from `zenui_provider_api::features_for_kind` — it is not a
    /// persisted value. See `set_cached_health` for the rationale.
    pub async fn get_cached_health(
        &self,
        kind: ProviderKind,
    ) -> Option<(String, ProviderStatus)> {
        let connection = self.connection.lock();
        connection
            .query_row(
                "SELECT checked_at, status_json FROM provider_health_cache WHERE provider = ?1",
                params![kind.as_tag()],
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
                    .map(|mut status| {
                        // Features are code-derived; never trust the
                        // value that came out of the JSON column. A row
                        // written by an older daemon build is missing
                        // any flag added since, and `#[serde(default)]`
                        // silently defaults it to `false`. Recomputing
                        // here makes that impossible.
                        status.features =
                            zenui_provider_api::features_for_kind(status.kind);
                        (checked_at, status)
                    })
            })
    }

    /// Load the full provider-enablement map. Keys are every row in
    /// `provider_enablement`; providers whose kind has no row are treated
    /// as enabled by the caller (runtime-core defaults to `true` on miss).
    pub async fn get_provider_enablement(&self) -> HashMap<ProviderKind, bool> {
        let connection = self.connection.lock();
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
        let connection = self.connection.lock();
        let _ = connection.execute(
            "INSERT INTO provider_enablement (provider, enabled, updated_at)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(provider) DO UPDATE SET
                enabled = excluded.enabled,
                updated_at = excluded.updated_at",
            params![kind.as_tag(), enabled as i64, now],
        );
    }

    /// Persist the health status for a provider with `now` as the checked_at timestamp.
    ///
    /// The `features` field is intentionally **not** persisted — it's a
    /// pure function of the provider kind and the daemon build, so
    /// caching it would let an older row serve stale capability flags
    /// to a newer daemon (e.g. a row written before a new flag existed
    /// reads back with that flag defaulted to `false`). We zero the
    /// field before serialising; `get_cached_health` repopulates it
    /// from `zenui_provider_api::features_for_kind` on every read.
    pub async fn set_cached_health(&self, kind: ProviderKind, status: &ProviderStatus) {
        let mut to_store = status.clone();
        to_store.features = zenui_provider_api::ProviderFeatures::default();
        let json = match serde_json::to_string(&to_store) {
            Ok(s) => s,
            Err(_) => return,
        };
        let now = chrono::Utc::now().to_rfc3339();
        let connection = self.connection.lock();
        let _ = connection.execute(
            "INSERT INTO provider_health_cache (provider, checked_at, status_json)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(provider) DO UPDATE SET
                checked_at = excluded.checked_at,
                status_json = excluded.status_json",
            params![kind.as_tag(), now, json],
        );
    }

    pub async fn list_projects(&self) -> Vec<ProjectRecord> {
        let connection = self.connection.lock();
        let mut statement = match connection.prepare(
            "SELECT project_id, path, created_at, updated_at
             FROM projects
             WHERE deleted_at IS NULL
             ORDER BY created_at ASC",
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let rows = statement.query_map([], |row| {
            Ok(ProjectRecord {
                project_id: row.get(0)?,
                path: row.get(1)?,
                created_at: row.get(2)?,
                updated_at: row.get(3)?,
            })
        });
        match rows {
            Ok(iter) => iter.filter_map(|r| r.ok()).collect(),
            Err(_) => Vec::new(),
        }
    }

    pub async fn get_project(&self, project_id: &str) -> Option<ProjectRecord> {
        let connection = self.connection.lock();
        connection
            .query_row(
                "SELECT project_id, path, created_at, updated_at
                 FROM projects WHERE project_id = ?1",
                params![project_id],
                |row| {
                    Ok(ProjectRecord {
                        project_id: row.get(0)?,
                        path: row.get(1)?,
                        created_at: row.get(2)?,
                        updated_at: row.get(3)?,
                    })
                },
            )
            .optional()
            .ok()
            .flatten()
    }

    pub async fn create_project(&self, path: Option<String>) -> Option<ProjectRecord> {
        let connection = self.connection.lock();
        let now = Utc::now().to_rfc3339();

        // Resurrection path. If we have a path AND there's an existing
        // tombstoned project with the exact same path, un-tombstone it
        // instead of inserting a new row. Reusing the same project_id
        // means every session that was previously attached to this
        // project automatically reappears under it — no UPDATE on the
        // sessions table is needed because their project_id never
        // changed when the project was deleted in the first place.
        if let Some(p) = path.as_deref() {
            let existing: Option<(String, String)> = connection
                .query_row(
                    "SELECT project_id, created_at FROM projects
                     WHERE path = ?1 AND deleted_at IS NOT NULL
                     ORDER BY deleted_at DESC LIMIT 1",
                    params![p],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                        ))
                    },
                )
                .optional()
                .ok()
                .flatten();
            if let Some((existing_id, created_at)) = existing {
                let restored = connection.execute(
                    "UPDATE projects
                     SET deleted_at = NULL, updated_at = ?1
                     WHERE project_id = ?2",
                    params![now, existing_id],
                );
                if restored.is_ok() {
                    return Some(ProjectRecord {
                        project_id: existing_id,
                        path,
                        created_at,
                        updated_at: now,
                    });
                }
            }
        }

        let project_id = Uuid::new_v4().to_string();
        let result = connection.execute(
            "INSERT INTO projects (project_id, path, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![project_id, path, now, now],
        );
        if result.is_err() {
            return None;
        }
        Some(ProjectRecord {
            project_id,
            path,
            created_at: now.clone(),
            updated_at: now,
        })
    }

    /// Tombstones a project — sets `deleted_at` to now instead of
    /// removing the row. Sessions that pointed at it keep their
    /// `project_id`, which is critical for the resurrection flow:
    /// re-creating a project with the same path un-tombstones the
    /// existing row (same uuid), so every session that was attached
    /// to it before reattaches automatically without any UPDATE on
    /// the sessions table.
    ///
    /// `list_projects` filters tombstoned rows out so the UI sidebar
    /// stops showing the project; the frontend additionally hides
    /// any session whose `project_id` doesn't match a live project,
    /// so the user doesn't see a flood of orphans dumped into the
    /// unassigned bucket.
    ///
    /// Returns `Some(empty)` on success (the empty-vec wire shape is
    /// preserved for backwards compatibility with old clients that
    /// expected `reassigned_session_ids`) or `None` if no live row
    /// matched the id.
    pub async fn delete_project(&self, project_id: &str) -> Option<Vec<String>> {
        let connection = self.connection.lock();
        let now = Utc::now().to_rfc3339();
        let updated = connection
            .execute(
                "UPDATE projects SET deleted_at = ?1
                 WHERE project_id = ?2 AND deleted_at IS NULL",
                params![now, project_id],
            )
            .ok()?;
        if updated == 0 {
            return None;
        }
        Some(Vec::new())
    }

    pub async fn assign_session_to_project(
        &self,
        session_id: &str,
        project_id: Option<&str>,
    ) -> bool {
        let connection = self.connection.lock();
        connection
            .execute(
                "UPDATE sessions SET project_id = ?1 WHERE session_id = ?2",
                params![project_id, session_id],
            )
            .map(|affected| affected > 0)
            .unwrap_or(false)
    }

    fn migrate(&self) -> Result<()> {
        let connection = self.connection.lock();
        connection
            .execute_batch(
                "
            PRAGMA journal_mode = WAL;

            -- Display-only fields (`title`, `last_turn_preview`)
            -- deliberately do NOT exist here: they're app concerns,
            -- persisted by consuming apps in their own stores. See
            -- `CLAUDE.md` in this directory for the boundary rule.
            CREATE TABLE IF NOT EXISTS sessions (
                session_id TEXT PRIMARY KEY,
                provider TEXT NOT NULL,
                status TEXT NOT NULL,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
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

            -- Display-only fields (`name`, `sort_order`) deliberately
            -- do NOT exist here — see `CLAUDE.md` in this directory.
            CREATE TABLE IF NOT EXISTS projects (
                project_id TEXT PRIMARY KEY,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
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

            -- Image attachments pasted by the user on a turn. The bytes
            -- live on disk under <data_dir>/attachments/<id>.<ext>;
            -- this table holds only metadata so opening a thread doesn't
            -- pull MBs of binary into memory. Cascade is handled
            -- explicitly in delete_session / delete_archived_session
            -- (no FOREIGN KEY because turn_id may live in either
            -- `turns` or `archived_turns`).
            CREATE TABLE IF NOT EXISTS turn_attachments (
                id TEXT PRIMARY KEY,
                turn_id TEXT NOT NULL,
                session_id TEXT NOT NULL,
                media_type TEXT NOT NULL,
                name TEXT,
                size_bytes INTEGER NOT NULL,
                created_at TEXT NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_turn_attachments_turn_id
                ON turn_attachments(turn_id);
            CREATE INDEX IF NOT EXISTS idx_turn_attachments_session_id
                ON turn_attachments(session_id);
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
        let _ = connection.execute("ALTER TABLE projects ADD COLUMN deleted_at TEXT", []);

        // Archived session/turn tables — same schema, plus archived_at timestamp.
        let _ = connection.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS archived_sessions (
                session_id TEXT PRIMARY KEY,
                provider TEXT NOT NULL,
                status TEXT NOT NULL,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
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
        let connection = self.connection.lock();
        let now = Utc::now().to_rfc3339();
        let tx = match connection.unchecked_transaction() {
            Ok(tx) => tx,
            Err(_) => return false,
        };

        let moved = tx
            .execute(
                "INSERT INTO archived_sessions
                    (session_id, provider, status, created_at, updated_at,
                     turn_count, provider_state_json, model, project_id, archived_at)
                 SELECT session_id, provider, status, created_at, updated_at,
                        turn_count, provider_state_json, model, project_id, ?1
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
            let connection = self.connection.lock();
            let tx = match connection.unchecked_transaction() {
                Ok(tx) => tx,
                Err(_) => return None,
            };

            let moved = tx
                .execute(
                    "INSERT INTO sessions
                        (session_id, provider, status, created_at, updated_at,
                         turn_count, provider_state_json, model, project_id)
                     SELECT session_id, provider, status, created_at, updated_at,
                            turn_count, provider_state_json, model, project_id
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
        let connection = self.connection.lock();
        let mut stmt = match connection.prepare(
            "SELECT session_id, provider, status, created_at, updated_at,
                    turn_count, model, project_id
             FROM archived_sessions ORDER BY created_at DESC",
        ) {
            Ok(stmt) => stmt,
            Err(_) => return Vec::new(),
        };

        stmt.query_map([], |row| {
            Ok(SessionSummary {
                session_id: row.get(0)?,
                provider: provider_kind_from_str(&row.get::<_, String>(1)?),
                status: session_status_from_str(&row.get::<_, String>(2)?),
                created_at: row.get(3)?,
                updated_at: row.get(4)?,
                turn_count: row.get::<_, i64>(5)? as usize,
                model: row.get(6)?,
                project_id: row.get(7)?,
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
            "SELECT session_id, provider, status, created_at, updated_at, turn_count, provider_state_json, model, project_id
             FROM sessions WHERE session_id = ?1",
            params![session_id],
            |row| {
                Ok(SessionSummary {
                    session_id: row.get(0)?,
                    provider: provider_kind_from_str(&row.get::<_, String>(1)?),
                    status: session_status_from_str(&row.get::<_, String>(2)?),
                    created_at: row.get(3)?,
                    updated_at: row.get(4)?,
                    turn_count: row.get::<_, i64>(5)? as usize,
                    model: row.get(7)?,
                    project_id: row.get(8)?,
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
            // Filled in by the per-session JOIN below so we don't pay
            // an extra query per turn.
            input_attachments: Vec::new(),
            // Usage is transient — we don't persist it to sqlite yet.
            // Historical turns reload with no usage; fresh turns
            // populate it via ProviderTurnEvent::TurnUsage.
            usage: None,
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

    // Hydrate input_attachments for all turns in this session with one
    // query, then distribute by turn_id. Cheap even for long sessions
    // because turn_attachments rows carry only metadata (no bytes).
    {
        let mut attach_stmt = connection
            .prepare(
                "SELECT id, turn_id, media_type, name, size_bytes
                 FROM turn_attachments WHERE session_id = ?1
                 ORDER BY created_at ASC",
            )
            .context("failed to prepare turn_attachments query")?;
        let attach_iter = attach_stmt
            .query_map(params![session_id], |row| {
                let turn_id: String = row.get(1)?;
                Ok((
                    turn_id,
                    AttachmentRef {
                        id: row.get(0)?,
                        media_type: row.get(2)?,
                        name: row.get(3)?,
                        size_bytes: row.get::<_, i64>(4)? as u64,
                    },
                ))
            })
            .context("failed to query turn_attachments")?;
        let mut by_turn: HashMap<String, Vec<AttachmentRef>> = HashMap::new();
        for entry in attach_iter.flatten() {
            by_turn.entry(entry.0).or_default().push(entry.1);
        }
        for turn in &mut turns {
            if let Some(atts) = by_turn.remove(&turn.turn_id) {
                turn.input_attachments = atts;
            }
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use zenui_provider_api::ProviderStatusLevel;

    /// A row written by a daemon that predates a feature flag stores
    /// `{}` (or an incomplete object) for `features`. After the fix,
    /// `get_cached_health` must repopulate from
    /// `features_for_kind(kind)` so the UI sees the current
    /// capability set — not whatever the old daemon happened to know
    /// about.
    #[tokio::test]
    async fn get_cached_health_recomputes_features_from_registry() {
        let service = PersistenceService::in_memory().expect("in_memory service");

        // Write a raw row simulating an old-daemon payload: empty
        // features object, so every flag deserialises to its
        // serde default (`false`).
        let stale_json = r#"{
            "kind":"claude",
            "label":"Claude",
            "installed":true,
            "authenticated":true,
            "version":null,
            "status":"ready",
            "message":null,
            "models":[],
            "enabled":true,
            "features":{}
        }"#;
        {
            let connection = service.connection.lock();
            connection
                .execute(
                    "INSERT INTO provider_health_cache (provider, checked_at, status_json)
                     VALUES (?1, ?2, ?3)",
                    params!["claude", "2020-01-01T00:00:00Z", stale_json],
                )
                .unwrap();
        }

        let (_checked_at, status) = service
            .get_cached_health(ProviderKind::Claude)
            .await
            .expect("cached row");

        // Features come back from the registry even though the row
        // had `features:{}` — this is the regression guard.
        let expected = zenui_provider_api::features_for_kind(ProviderKind::Claude);
        assert_eq!(
            status.features, expected,
            "features must be recomputed from the registry on read"
        );
        assert!(
            status.features.supports_auto_permission_mode,
            "claude should advertise auto permission mode"
        );
        assert!(
            status.features.thinking_effort,
            "claude should advertise thinking effort"
        );
    }

    /// Round-trip: writing a status with specific features and
    /// reading it back yields the registry's features (not the
    /// caller's), because features are not the source of truth at
    /// the persistence layer.
    #[tokio::test]
    async fn set_cached_health_does_not_persist_features() {
        let service = PersistenceService::in_memory().expect("in_memory service");

        // Caller passes a deliberately-wrong features value; the
        // write path must ignore it.
        let mut bogus_features = zenui_provider_api::ProviderFeatures::default();
        bogus_features.thinking_effort = false; // wrong for Claude
        let status = ProviderStatus {
            kind: ProviderKind::Claude,
            label: "Claude".into(),
            installed: true,
            authenticated: true,
            version: None,
            status: ProviderStatusLevel::Ready,
            message: None,
            models: Vec::new(),
            enabled: true,
            features: bogus_features,
        };
        service.set_cached_health(ProviderKind::Claude, &status).await;

        // Confirm the persisted JSON carries default features
        // (stripped on write), proving the on-disk row never
        // becomes the source of truth.
        let raw_json: String = {
            let connection = service.connection.lock();
            connection
                .query_row(
                    "SELECT status_json FROM provider_health_cache WHERE provider = 'claude'",
                    [],
                    |row| row.get(0),
                )
                .unwrap()
        };
        let persisted: ProviderStatus = serde_json::from_str(&raw_json).unwrap();
        assert_eq!(
            persisted.features,
            zenui_provider_api::ProviderFeatures::default(),
            "set_cached_health must strip features before writing"
        );

        // And the reader repopulates from the registry, so callers
        // see a correct status regardless of what was written.
        let (_checked_at, roundtripped) = service
            .get_cached_health(ProviderKind::Claude)
            .await
            .expect("cached row");
        assert_eq!(
            roundtripped.features,
            zenui_provider_api::features_for_kind(ProviderKind::Claude)
        );
    }
}
