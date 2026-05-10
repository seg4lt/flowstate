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

/// Hard cap on the size of a single media attachment, in bytes.
/// Raised from the original 5 MB image cap to 50 MB so short audio
/// clips and small screen recordings can ride through the same
/// pipeline as images. The frontend enforces a matching limit so an
/// oversized payload is rejected before we touch the disk.
pub const ATTACHMENT_MAX_BYTES: usize = 50 * 1024 * 1024;

/// Allowed attachment MIME types. Images were the only supported
/// category initially; audio and video were added for the drag-and-
/// drop feature — the user drags an `.mp3` / `.mp4` and it rides
/// through the same base64-inline pipeline as an image. Anything
/// else is rejected at the runtime boundary so the on-disk file
/// extension is always one of a known set.
pub const ATTACHMENT_ALLOWED_MEDIA_TYPES: &[&str] = &[
    // Images.
    "image/png",
    "image/jpeg",
    "image/gif",
    "image/webp",
    // Audio.
    "audio/mpeg",
    "audio/wav",
    "audio/ogg",
    "audio/mp4",
    "audio/flac",
    "audio/aac",
    "audio/opus",
    "audio/webm",
    // Video.
    "video/mp4",
    "video/quicktime",
    "video/webm",
    "video/x-matroska",
    "video/x-msvideo",
];

mod codecs;

use codecs::{
    ext_for_media_type, permission_mode_from_str, permission_mode_to_str, provider_kind_from_str,
    reasoning_effort_from_str, session_status_from_str, session_status_to_str, synthesize_blocks,
    turn_source_from_str, turn_source_to_str, turn_status_from_str, turn_status_to_str,
};

#[derive(Debug)]
pub struct PersistenceService {
    connection: Mutex<Connection>,
    attachments_dir: PathBuf,
}

/// A row in the `checkpoints` table. Consumed exclusively by the
/// `zenui-checkpoints` crate; exposed here because that crate doesn't
/// talk to sqlite directly.
#[derive(Debug, Clone)]
pub struct CheckpointRow {
    pub checkpoint_id: String,
    pub session_id: String,
    pub turn_id: String,
    /// RFC 3339.
    pub created_at: String,
    /// Filename (not full path) relative to
    /// `<data_dir>/checkpoints/manifests/`. The checkpoints crate
    /// resolves it against its own base dir.
    pub manifest_path: String,
}

/// Row decoder for `scheduled_wakeups`. Extracted as a free function
/// so both `list_pending_wakeups` and the per-id accessor share the
/// same column order without duplicating the SELECT shape.
fn wakeup_row_from_sqlite(r: &rusqlite::Row<'_>) -> rusqlite::Result<ScheduledWakeupRow> {
    let status_str: String = r.get(6)?;
    let status =
        ScheduledWakeupStatus::from_str(&status_str).unwrap_or(ScheduledWakeupStatus::Pending);
    Ok(ScheduledWakeupRow {
        wakeup_id: r.get(0)?,
        session_id: r.get(1)?,
        origin_turn_id: r.get(2)?,
        fire_at_unix: r.get(3)?,
        prompt: r.get(4)?,
        reason: r.get(5)?,
        status,
        created_at_unix: r.get(7)?,
    })
}

/// A row in the `scheduled_wakeups` table. The runtime's wakeup
/// scheduler OBSERVES Claude Code's native `ScheduleWakeup` tool call
/// (emitted by the model via the Claude Code harness inside the SDK
/// subprocess) and writes a row here so it can fire the follow-up
/// turn even after flowstate's `ProcessCache` reaps the bridge (which
/// takes Claude Code's internal timer down with it).
///
/// Field names match Claude Code's tool spec (`delaySeconds`,
/// `prompt`, `reason`) so the observer can decode the tool args
/// verbatim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScheduledWakeupRow {
    pub wakeup_id: String,
    pub session_id: String,
    pub origin_turn_id: Option<String>,
    pub fire_at_unix: i64,
    /// The `prompt` argument the agent passed to `ScheduleWakeup`.
    /// When the timer fires, the runtime injects this as a user turn
    /// on `session_id` — the respawned bridge resumes Claude Code via
    /// `native_thread_id`, so the model sees it as a normal
    /// continuation of its prior conversation.
    pub prompt: String,
    pub reason: Option<String>,
    pub status: ScheduledWakeupStatus,
    pub created_at_unix: i64,
}

/// State machine for [`ScheduledWakeupRow::status`]. Stored as a
/// lowercase string in SQLite (`pending` | `fired` | `cancelled`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScheduledWakeupStatus {
    Pending,
    Fired,
    Cancelled,
}

impl ScheduledWakeupStatus {
    fn as_str(&self) -> &'static str {
        match self {
            ScheduledWakeupStatus::Pending => "pending",
            ScheduledWakeupStatus::Fired => "fired",
            ScheduledWakeupStatus::Cancelled => "cancelled",
        }
    }

    fn from_str(raw: &str) -> Option<Self> {
        match raw {
            "pending" => Some(ScheduledWakeupStatus::Pending),
            "fired" => Some(ScheduledWakeupStatus::Fired),
            "cancelled" => Some(ScheduledWakeupStatus::Cancelled),
            _ => None,
        }
    }
}

/// A row in the `file_state` cache. One per canonicalized absolute path
/// we've ever captured; updated in place when a file's mtime/size moves.
#[derive(Debug, Clone)]
pub struct FileStateRow {
    pub abs_path: String,
    pub mtime_ns: i64,
    pub size_bytes: i64,
    /// Scheme-prefixed blake3 hash (e.g. `blake3:<hex>`). Stored as a
    /// string because this crate has no dependency on the checkpoints
    /// crate's `BlobHash` newtype.
    pub blob_hash: String,
    pub updated_at: String,
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

    /// Test-only helper: insert a skeletal `sessions` row so cross-crate
    /// tests can exercise code paths that join against it (e.g. the
    /// wakeup scheduler's FK against `scheduled_wakeups.session_id`)
    /// without reaching for the full `upsert_session` payload surface.
    #[doc(hidden)]
    pub fn insert_session_row_for_tests(&self, session_id: &str) -> Result<()> {
        let connection = self.connection.lock();
        connection
            .execute(
                "INSERT INTO sessions
                    (session_id, provider, status, created_at, updated_at, turn_count)
                 VALUES (?1, 'claude', 'idle', '2024-01-01T00:00:00Z', '2024-01-01T00:00:00Z', 0)",
                params![session_id],
            )
            .context("failed to insert test session row")?;
        Ok(())
    }

    pub fn in_memory() -> Result<Self> {
        // Tests don't typically exercise the file-backed attachment
        // path; give them a unique tempdir so any test that does write
        // an attachment doesn't collide with sibling tests.
        let attachments_dir =
            std::env::temp_dir().join(format!("zenui-test-attachments-{}", Uuid::new_v4()));
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
                        plan_json, permission_mode, reasoning_effort, blocks_json, source
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
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
                        turn_source_to_str(turn.source),
                    ],
                )
                .is_err()
            {
                return;
            }
        }

        let _ = transaction.commit();
    }

    /// Atomically append a single completed turn to an existing session.
    ///
    /// Unlike `upsert_session`, this does NOT delete-and-reinsert all turns.
    /// It executes two SQL statements in a transaction:
    ///
    /// 1. `INSERT INTO turns …` — inserts exactly the one new turn
    /// 2. `UPDATE sessions SET turn_count = turn_count + 1, … WHERE session_id = ?`
    ///
    /// This makes it safe to call concurrently with an in-flight `upsert_session`
    /// (e.g. from `send_turn`) because each call only touches the rows it owns.
    /// The primary use-case is `deliver_spontaneous_turn`, which fires outside a
    /// `send_turn` context and must not race-overwrite turns the active turn added.
    pub async fn append_turn(&self, session_id: &str, turn: &zenui_provider_api::TurnRecord) {
        let mut connection = self.connection.lock();
        let Ok(transaction) = connection.transaction() else {
            return;
        };
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
            .and_then(|p| serde_json::to_string(p).ok());
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
                    plan_json, permission_mode, reasoning_effort, blocks_json, source
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
                params![
                    turn.turn_id,
                    session_id,
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
                    turn_source_to_str(turn.source),
                ],
            )
            .is_err()
        {
            return;
        }
        // Increment the session's turn_count and bump updated_at so callers
        // querying the summary see an accurate count without a full reload.
        let _ = transaction.execute(
            "UPDATE sessions SET turn_count = turn_count + 1, status = 'ready', updated_at = ?1 WHERE session_id = ?2",
            params![turn.updated_at, session_id],
        );
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

    /// Read-only counterpart to [`get_session`] that resolves against the
    /// `archived_sessions` / `archived_turns` tables. Exists so the
    /// `LoadSession` wire handler can render an archived thread when the
    /// live lookup misses, without physically unarchiving the rows.
    /// Callers that mutate state (SendTurn, checkpoints, branch ops) keep
    /// using the live-only `get_session` path so archived threads remain
    /// read-only by construction.
    pub async fn get_archived_session(&self, session_id: &str) -> Option<SessionDetail> {
        let connection = self.connection.lock();
        load_archived_session(&connection, session_id, None)
            .ok()
            .flatten()
    }

    /// Paginated read-only counterpart to [`get_session_limited`] that
    /// resolves against the archive tables. See [`get_archived_session`]
    /// for the wider rationale.
    pub async fn get_archived_session_limited(
        &self,
        session_id: &str,
        limit: Option<usize>,
    ) -> Option<SessionDetail> {
        let connection = self.connection.lock();
        load_archived_session(&connection, session_id, limit)
            .ok()
            .flatten()
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
            .execute(
                "DELETE FROM sessions WHERE session_id = ?1",
                params![session_id],
            )
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
        if removed { tx.commit().is_ok() } else { false }
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
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
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
            let mut stmt = match connection
                .prepare("SELECT id, media_type FROM turn_attachments WHERE session_id = ?1")
            {
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
    pub async fn get_cached_health(&self, kind: ProviderKind) -> Option<(String, ProviderStatus)> {
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
                        status.features = zenui_provider_api::features_for_kind(status.kind);
                        (checked_at, status)
                    })
            })
    }

    /// Load the full provider-enablement map. Keys are every row in
    /// `provider_enablement`; providers whose kind has no row are treated
    /// as enabled by the caller (runtime-core defaults to `true` on miss).
    pub async fn get_provider_enablement(&self) -> HashMap<ProviderKind, bool> {
        let connection = self.connection.lock();
        let mut statement =
            match connection.prepare("SELECT provider, enabled FROM provider_enablement") {
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

    // ------------------------------------------------------------------
    // Checkpoint index and file-state cache.
    //
    // The `checkpoints` table tracks which session/turn pairs have an
    // on-disk manifest. The `file_state` table is a persistent
    // (path, mtime, size) -> blob_hash cache that powers the
    // "only hash files that actually changed" fast path at turn end.
    //
    // Both tables are consumed exclusively by the `zenui-checkpoints`
    // crate. This crate owns the SQL because persistence is the one
    // place that opens the sqlite connection; checkpoints calls typed
    // methods here to avoid a second connection to the same file.
    // ------------------------------------------------------------------

    pub async fn insert_checkpoint(&self, row: CheckpointRow) -> Result<()> {
        let connection = self.connection.lock();
        // Idempotent on (session_id, turn_id) — capture is called once
        // per turn-end but a redelivery (daemon restart mid-turn) must
        // not explode with UNIQUE constraint errors. `INSERT OR IGNORE`
        // sidesteps both the PK and UNIQUE constraints in one clause.
        connection
            .execute(
                "INSERT OR IGNORE INTO checkpoints
                    (checkpoint_id, session_id, turn_id, created_at, manifest_path)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    row.checkpoint_id,
                    row.session_id,
                    row.turn_id,
                    row.created_at,
                    row.manifest_path,
                ],
            )
            .with_context(|| {
                format!(
                    "insert checkpoint row (session={}, turn={})",
                    row.session_id, row.turn_id
                )
            })?;
        Ok(())
    }

    pub async fn get_checkpoint_by_turn(
        &self,
        session_id: &str,
        turn_id: &str,
    ) -> Option<CheckpointRow> {
        let connection = self.connection.lock();
        connection
            .query_row(
                "SELECT checkpoint_id, session_id, turn_id, created_at, manifest_path
                 FROM checkpoints WHERE session_id = ?1 AND turn_id = ?2",
                params![session_id, turn_id],
                |r| {
                    Ok(CheckpointRow {
                        checkpoint_id: r.get(0)?,
                        session_id: r.get(1)?,
                        turn_id: r.get(2)?,
                        created_at: r.get(3)?,
                        manifest_path: r.get(4)?,
                    })
                },
            )
            .optional()
            .ok()
            .flatten()
    }

    /// Return all checkpoints for this session whose `created_at` is >=
    /// the target turn's `created_at`, ordered chronologically (ASC).
    ///
    /// The ordering is by `created_at` because capture runs at turn end,
    /// so that timestamp is an accurate proxy for "turn order."
    pub async fn list_session_checkpoints_from(
        &self,
        session_id: &str,
        target_turn_id: &str,
    ) -> Vec<CheckpointRow> {
        let connection = self.connection.lock();
        let mut statement = match connection.prepare(
            "SELECT checkpoint_id, session_id, turn_id, created_at, manifest_path
             FROM checkpoints
             WHERE session_id = ?1
               AND created_at >= (
                    SELECT created_at FROM checkpoints
                    WHERE session_id = ?1 AND turn_id = ?2
               )
             ORDER BY created_at ASC",
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let rows = statement.query_map(params![session_id, target_turn_id], |r| {
            Ok(CheckpointRow {
                checkpoint_id: r.get(0)?,
                session_id: r.get(1)?,
                turn_id: r.get(2)?,
                created_at: r.get(3)?,
                manifest_path: r.get(4)?,
            })
        });
        match rows {
            Ok(iter) => iter.filter_map(|r| r.ok()).collect(),
            Err(_) => Vec::new(),
        }
    }

    /// Delete all checkpoint rows for a session and return the manifest
    /// paths they pointed at so the caller can remove them from disk.
    pub async fn delete_checkpoints_for_session(&self, session_id: &str) -> Vec<String> {
        let connection = self.connection.lock();
        let manifest_paths: Vec<String> = {
            let mut statement = match connection
                .prepare("SELECT manifest_path FROM checkpoints WHERE session_id = ?1")
            {
                Ok(s) => s,
                Err(_) => return Vec::new(),
            };
            let rows = statement.query_map(params![session_id], |r| r.get::<_, String>(0));
            match rows {
                Ok(iter) => iter.filter_map(|r| r.ok()).collect(),
                Err(_) => Vec::new(),
            }
        };
        let _ = connection.execute(
            "DELETE FROM checkpoints WHERE session_id = ?1",
            params![session_id],
        );
        manifest_paths
    }

    /// Return all checkpoint rows. Used by GC to compute the set of
    /// "live" manifest paths (every blob referenced by one of these
    /// manifests is reachable and must not be reclaimed).
    pub async fn list_all_checkpoints(&self) -> Vec<CheckpointRow> {
        let connection = self.connection.lock();
        let mut statement = match connection.prepare(
            "SELECT checkpoint_id, session_id, turn_id, created_at, manifest_path FROM checkpoints",
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let rows = statement.query_map([], |r| {
            Ok(CheckpointRow {
                checkpoint_id: r.get(0)?,
                session_id: r.get(1)?,
                turn_id: r.get(2)?,
                created_at: r.get(3)?,
                manifest_path: r.get(4)?,
            })
        });
        match rows {
            Ok(iter) => iter.filter_map(|r| r.ok()).collect(),
            Err(_) => Vec::new(),
        }
    }

    // ---------- scheduled_wakeups -----------------------------------

    /// Insert a freshly-observed pending wakeup. Fails with UNIQUE on
    /// `wakeup_id` collisions — callers generate a fresh UUID per
    /// observed tool call (we key on flowstate's own id, not Claude
    /// Code's tool-call id, so we retain ownership even if the tool
    /// call id scheme changes).
    pub async fn insert_wakeup(&self, row: ScheduledWakeupRow) -> Result<()> {
        let connection = self.connection.lock();
        connection
            .execute(
                "INSERT INTO scheduled_wakeups
                    (wakeup_id, session_id, origin_turn_id, fire_at_unix, prompt,
                     reason, status, created_at_unix)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    row.wakeup_id,
                    row.session_id,
                    row.origin_turn_id,
                    row.fire_at_unix,
                    row.prompt,
                    row.reason,
                    row.status.as_str(),
                    row.created_at_unix,
                ],
            )
            .with_context(|| {
                format!(
                    "insert wakeup row (session={}, wakeup={})",
                    row.session_id, row.wakeup_id
                )
            })?;
        Ok(())
    }

    /// Every `Pending` wakeup, oldest-first by fire time. Used at
    /// daemon startup to rehydrate the scheduler's in-memory heap.
    pub async fn list_pending_wakeups(&self) -> Vec<ScheduledWakeupRow> {
        let connection = self.connection.lock();
        let mut statement = match connection.prepare(
            "SELECT wakeup_id, session_id, origin_turn_id, fire_at_unix, prompt,
                    reason, status, created_at_unix
             FROM scheduled_wakeups
             WHERE status = 'pending'
             ORDER BY fire_at_unix ASC",
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let rows = statement.query_map([], wakeup_row_from_sqlite);
        match rows {
            Ok(iter) => iter.filter_map(|r| r.ok()).collect(),
            Err(_) => Vec::new(),
        }
    }

    /// Count pending wakeups for one session. Used by the observer
    /// to enforce a per-session cap so a runaway agent can't queue
    /// unlimited wakeups.
    pub async fn count_pending_wakeups_for_session(&self, session_id: &str) -> i64 {
        let connection = self.connection.lock();
        connection
            .query_row(
                "SELECT COUNT(*) FROM scheduled_wakeups
                 WHERE session_id = ?1 AND status = 'pending'",
                params![session_id],
                |r| r.get::<_, i64>(0),
            )
            .optional()
            .ok()
            .flatten()
            .unwrap_or(0)
    }

    /// Flip a wakeup from `pending` to `fired`. Returns `true` iff a
    /// row was actually updated — the scheduler uses this to
    /// distinguish "fired for real" from "raced with a cancel" and
    /// skip the turn dispatch in the latter case.
    pub async fn mark_wakeup_fired(&self, wakeup_id: &str) -> bool {
        let connection = self.connection.lock();
        connection
            .execute(
                "UPDATE scheduled_wakeups SET status = 'fired'
                 WHERE wakeup_id = ?1 AND status = 'pending'",
                params![wakeup_id],
            )
            .map(|n| n > 0)
            .unwrap_or(false)
    }

    /// Cancel every pending wakeup for a session. Returns the cancelled
    /// ids so callers can emit matching events. Invoked when a session
    /// is archived or deleted.
    pub async fn cancel_wakeups_for_session(&self, session_id: &str) -> Vec<String> {
        let connection = self.connection.lock();
        let ids: Vec<String> = {
            let mut statement = match connection.prepare(
                "SELECT wakeup_id FROM scheduled_wakeups
                 WHERE session_id = ?1 AND status = 'pending'",
            ) {
                Ok(s) => s,
                Err(_) => return Vec::new(),
            };
            let rows = statement.query_map(params![session_id], |r| r.get::<_, String>(0));
            match rows {
                Ok(iter) => iter.filter_map(|r| r.ok()).collect(),
                Err(_) => Vec::new(),
            }
        };
        let _ = connection.execute(
            "UPDATE scheduled_wakeups SET status = 'cancelled'
             WHERE session_id = ?1 AND status = 'pending'",
            params![session_id],
        );
        ids
    }

    pub async fn get_file_state(&self, abs_path: &str) -> Option<FileStateRow> {
        let connection = self.connection.lock();
        connection
            .query_row(
                "SELECT abs_path, mtime_ns, size_bytes, blob_hash, updated_at
                 FROM file_state WHERE abs_path = ?1",
                params![abs_path],
                |r| {
                    Ok(FileStateRow {
                        abs_path: r.get(0)?,
                        mtime_ns: r.get(1)?,
                        size_bytes: r.get(2)?,
                        blob_hash: r.get(3)?,
                        updated_at: r.get(4)?,
                    })
                },
            )
            .optional()
            .ok()
            .flatten()
    }

    pub async fn upsert_file_state(&self, row: FileStateRow) {
        let connection = self.connection.lock();
        let _ = connection.execute(
            "INSERT INTO file_state (abs_path, mtime_ns, size_bytes, blob_hash, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(abs_path) DO UPDATE SET
                mtime_ns   = excluded.mtime_ns,
                size_bytes = excluded.size_bytes,
                blob_hash  = excluded.blob_hash,
                updated_at = excluded.updated_at",
            params![
                row.abs_path,
                row.mtime_ns,
                row.size_bytes,
                row.blob_hash,
                row.updated_at,
            ],
        );
    }

    /// Return every file_state row whose `abs_path` starts with
    /// `prefix`. Used by `FsCheckpointStore::capture` to detect files
    /// that existed last time we captured but have since disappeared
    /// from disk. `prefix` must include the trailing path separator so
    /// a workspace at `/a` doesn't accidentally match `/a-b/foo`.
    pub async fn list_file_state_under_prefix(&self, prefix: &str) -> Vec<FileStateRow> {
        let connection = self.connection.lock();
        // SQLite `LIKE` with an escaped prefix. We don't use user-
        // provided patterns so no need for parameterized ESCAPE; the
        // LIKE wildcards (`%`, `_`) aren't present in filesystem paths
        // on any platform we support.
        let like = format!("{prefix}%");
        let mut statement = match connection.prepare(
            "SELECT abs_path, mtime_ns, size_bytes, blob_hash, updated_at
             FROM file_state WHERE abs_path LIKE ?1",
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let rows = statement.query_map(params![like], |r| {
            Ok(FileStateRow {
                abs_path: r.get(0)?,
                mtime_ns: r.get(1)?,
                size_bytes: r.get(2)?,
                blob_hash: r.get(3)?,
                updated_at: r.get(4)?,
            })
        });
        match rows {
            Ok(iter) => iter.filter_map(|r| r.ok()).collect(),
            Err(_) => Vec::new(),
        }
    }

    pub async fn delete_file_state(&self, abs_path: &str) {
        let connection = self.connection.lock();
        let _ = connection.execute(
            "DELETE FROM file_state WHERE abs_path = ?1",
            params![abs_path],
        );
    }

    /// Return all file-state rows (path + blob_hash only — GC doesn't
    /// need mtime/size). Used during GC to build the set of blob hashes
    /// still referenced by the cache so they're not reclaimed.
    pub async fn list_file_state_blob_hashes(&self) -> Vec<String> {
        let connection = self.connection.lock();
        let mut statement = match connection.prepare("SELECT blob_hash FROM file_state") {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let rows = statement.query_map([], |r| r.get::<_, String>(0));
        match rows {
            Ok(iter) => iter.filter_map(|r| r.ok()).collect(),
            Err(_) => Vec::new(),
        }
    }

    /// Delete file-state rows older than `older_than_rfc3339`. Returns
    /// the count deleted so GC telemetry can report it. LRU policy:
    /// anything whose `updated_at` predates the cutoff is assumed stale
    /// (file moved, project deleted, workspace abandoned).
    pub async fn prune_stale_file_state(&self, older_than_rfc3339: &str) -> usize {
        let connection = self.connection.lock();
        connection
            .execute(
                "DELETE FROM file_state WHERE updated_at < ?1",
                params![older_than_rfc3339],
            )
            .unwrap_or(0)
    }

    // ------------------------------------------------------------------
    // Checkpoint enablement setting.
    //
    // Single global flag: disabled → runtime-core skips capture and
    // rewind surfaces `Disabled`. Read once at boot by
    // `seed_checkpoint_enablement`, re-read on every mutation.
    // ------------------------------------------------------------------

    /// Default value surfaced to the runtime when no row exists yet.
    /// Kept in one place so `seed_checkpoint_enablement`, a brand-new
    /// database, and any future default-change migration all agree.
    pub const CHECKPOINTS_GLOBAL_DEFAULT: bool = true;

    /// Read the global checkpoint-enablement flag from
    /// `runtime_settings["checkpoints.global_enabled"]`, falling back
    /// to [`Self::CHECKPOINTS_GLOBAL_DEFAULT`] when unset.
    pub async fn get_checkpoints_global_enabled(&self) -> bool {
        let connection = self.connection.lock();
        connection
            .query_row(
                "SELECT value FROM runtime_settings WHERE key = 'checkpoints.global_enabled'",
                [],
                |r| r.get::<_, String>(0),
            )
            .optional()
            .ok()
            .flatten()
            .and_then(|raw| serde_json::from_str::<bool>(&raw).ok())
            .unwrap_or(Self::CHECKPOINTS_GLOBAL_DEFAULT)
    }

    pub async fn set_checkpoints_global_enabled(&self, enabled: bool) {
        let now = chrono::Utc::now().to_rfc3339();
        let value = if enabled { "true" } else { "false" };
        let connection = self.connection.lock();
        let _ = connection.execute(
            "INSERT INTO runtime_settings (key, value, updated_at)
             VALUES ('checkpoints.global_enabled', ?1, ?2)
             ON CONFLICT(key) DO UPDATE SET
                value      = excluded.value,
                updated_at = excluded.updated_at",
            params![value, now],
        );
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
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
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

            -- Checkpoint index. One row per captured turn. The runtime's
            -- DeleteSession handler explicitly calls
            -- `CheckpointStore::delete_for_session` to reclaim manifests
            -- on disk + the rows here, so no FOREIGN KEY is declared —
            -- it would just complicate test setup without adding safety
            -- (the explicit cleanup is the canonical path). See the
            -- checkpoints crate for the on-disk format at
            -- <data_dir>/checkpoints/manifests/<checkpoint_id>.json.
            CREATE TABLE IF NOT EXISTS checkpoints (
                checkpoint_id TEXT PRIMARY KEY,
                session_id    TEXT NOT NULL,
                turn_id       TEXT NOT NULL,
                created_at    TEXT NOT NULL,
                manifest_path TEXT NOT NULL,
                UNIQUE (session_id, turn_id)
            );
            CREATE INDEX IF NOT EXISTS idx_checkpoints_session_turn
                ON checkpoints(session_id, turn_id);
            CREATE INDEX IF NOT EXISTS idx_checkpoints_session_created
                ON checkpoints(session_id, created_at);

            -- Persistent (path, mtime, size) -> blob_hash cache. Global
            -- scope (not session-scoped) because a file exists once on
            -- disk regardless of which session observed it. `updated_at`
            -- drives LRU GC so this table doesn't grow unbounded as
            -- workspaces are opened and forgotten.
            CREATE TABLE IF NOT EXISTS file_state (
                abs_path   TEXT PRIMARY KEY,
                mtime_ns   INTEGER NOT NULL,
                size_bytes INTEGER NOT NULL,
                blob_hash  TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_file_state_updated
                ON file_state(updated_at);

            -- Generic key/value settings owned by the runtime. Uses a
            -- JSON-encoded value column so future toggles (telemetry,
            -- etc.) can reuse the same table without another
            -- migration. Current keys:
            --   * checkpoints.global_enabled -> bool (default true)
            CREATE TABLE IF NOT EXISTS runtime_settings (
                key        TEXT PRIMARY KEY,
                value      TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );

            -- Observed scheduled wakeups. Populated when the runtime
            -- sees Claude Code's native `ScheduleWakeup` tool call
            -- land in a turn's stream. Runtime-read (not display): on
            -- daemon startup the scheduler rehydrates every `pending`
            -- row and arms a tokio timer. On fire, the scheduler
            -- injects `prompt` as a user turn on `session_id` —
            -- respawning the bridge as needed so Claude Code resumes
            -- via `native_thread_id` and sees it as a normal turn.
            --
            -- We persist because our timer (not Claude Code's) is the
            -- one that has to survive daemon + bridge lifecycle
            -- events, including the `ProcessCache` idle-kill that
            -- takes Claude Code's in-CLI timer down with it. Cascade
            -- on session delete so archival can't leave dangling
            -- timers.
            CREATE TABLE IF NOT EXISTS scheduled_wakeups (
                wakeup_id       TEXT PRIMARY KEY,
                session_id      TEXT NOT NULL REFERENCES sessions(session_id) ON DELETE CASCADE,
                origin_turn_id  TEXT,
                fire_at_unix    INTEGER NOT NULL,
                prompt          TEXT NOT NULL,
                reason          TEXT,
                status          TEXT NOT NULL,
                created_at_unix INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_wakeups_pending_fire_at
                ON scheduled_wakeups(status, fire_at_unix);
            CREATE INDEX IF NOT EXISTS idx_wakeups_session
                ON scheduled_wakeups(session_id);
            ",
            )
            .context("failed to run sqlite migrations")?;

        // Idempotent column additions — ignore errors if the column already exists.
        let _ = connection.execute(
            "ALTER TABLE sessions ADD COLUMN provider_state_json TEXT",
            [],
        );
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
        // Authorship column — distinguishes user-typed turns from
        // wakeup / peer-injected ones. NULL on legacy rows is decoded
        // as `TurnSource::User` (see `turn_source_from_str`).
        let _ = connection.execute("ALTER TABLE turns ADD COLUMN source TEXT", []);
        let _ = connection.execute("ALTER TABLE archived_turns ADD COLUMN source TEXT", []);
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
                 plan_json, permission_mode, reasoning_effort, blocks_json, source)
             SELECT turn_id, session_id, input, output, status, created_at, updated_at,
                    reasoning_json, tool_calls_json, file_changes_json, subagents_json,
                    plan_json, permission_mode, reasoning_effort, blocks_json, source
             FROM turns WHERE session_id = ?1",
            params![session_id],
        );
        let _ = tx.execute(
            "DELETE FROM turns WHERE session_id = ?1",
            params![session_id],
        );
        let _ = tx.execute(
            "DELETE FROM sessions WHERE session_id = ?1",
            params![session_id],
        );
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
                     plan_json, permission_mode, reasoning_effort, blocks_json, source)
                 SELECT turn_id, session_id, input, output, status, created_at, updated_at,
                        reasoning_json, tool_calls_json, file_changes_json, subagents_json,
                        plan_json, permission_mode, reasoning_effort, blocks_json, source
                 FROM archived_turns WHERE session_id = ?1",
                params![session_id],
            );
            let _ = tx.execute(
                "DELETE FROM archived_turns WHERE session_id = ?1",
                params![session_id],
            );
            let _ = tx.execute(
                "DELETE FROM archived_sessions WHERE session_id = ?1",
                params![session_id],
            );
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
    load_session_from(connection, session_id, limit, "sessions", "turns")
}

/// Same shape as [`load_session`] but reads from the `archived_sessions` /
/// `archived_turns` tables. Used by the `LoadSession` read-only fallback so
/// the UI can render archived threads without unarchiving them. No runtime
/// state is materialized from this — archived sessions aren't resumable,
/// just readable.
fn load_archived_session(
    connection: &Connection,
    session_id: &str,
    limit: Option<usize>,
) -> Result<Option<SessionDetail>> {
    load_session_from(
        connection,
        session_id,
        limit,
        "archived_sessions",
        "archived_turns",
    )
}

fn load_session_from(
    connection: &Connection,
    session_id: &str,
    limit: Option<usize>,
    sessions_table: &str,
    turns_table: &str,
) -> Result<Option<SessionDetail>> {
    let summary_sql = format!(
        "SELECT session_id, provider, status, created_at, updated_at, turn_count, provider_state_json, model, project_id
         FROM {sessions_table} WHERE session_id = ?1"
    );
    let summary = connection
        .query_row(&summary_sql, params![session_id], |row| {
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
        })
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
            format!(
                "SELECT turn_id, input, output, status, created_at, updated_at, reasoning_json,
                        tool_calls_json, file_changes_json, subagents_json, plan_json, permission_mode,
                        reasoning_effort, blocks_json, source
                 FROM {turns_table} WHERE session_id = ?1 ORDER BY created_at DESC LIMIT ?2"
            ),
            Some(n as i64),
        )
    } else {
        (
            format!(
                "SELECT turn_id, input, output, status, created_at, updated_at, reasoning_json,
                        tool_calls_json, file_changes_json, subagents_json, plan_json, permission_mode,
                        reasoning_effort, blocks_json, source
                 FROM {turns_table} WHERE session_id = ?1 ORDER BY created_at ASC"
            ),
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
        let source_str: Option<String> = row.get(14)?;
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
            // NULL on legacy rows (column added after the original
            // schema) decodes to `TurnSource::User` — see
            // `turn_source_from_str`.
            source: turn_source_from_str(source_str.as_deref()),
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

    let provider_state_sql =
        format!("SELECT provider_state_json FROM {sessions_table} WHERE session_id = ?1");
    let provider_state = connection
        .query_row(&provider_state_sql, params![session_id], |row| {
            row.get::<_, Option<String>>(0)
        })
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
        service
            .set_cached_health(ProviderKind::Claude, &status)
            .await;

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

    fn sample_wakeup(wakeup_id: &str, session_id: &str, fire_at: i64) -> ScheduledWakeupRow {
        ScheduledWakeupRow {
            wakeup_id: wakeup_id.to_string(),
            session_id: session_id.to_string(),
            origin_turn_id: Some("turn-1".to_string()),
            fire_at_unix: fire_at,
            prompt: "continue polling the build".to_string(),
            reason: Some("loop probe".to_string()),
            status: ScheduledWakeupStatus::Pending,
            created_at_unix: 1_700_000_000,
        }
    }

    #[tokio::test]
    async fn insert_and_list_pending_wakeups_round_trip() {
        let service = PersistenceService::in_memory().expect("in-memory service");
        service.insert_session_row_for_tests("s-1").unwrap();
        service.insert_session_row_for_tests("s-2").unwrap();

        service
            .insert_wakeup(sample_wakeup("w-2", "s-1", 2_000))
            .await
            .expect("insert w-2");
        service
            .insert_wakeup(sample_wakeup("w-1", "s-1", 1_000))
            .await
            .expect("insert w-1");
        service
            .insert_wakeup(sample_wakeup("w-3", "s-2", 3_000))
            .await
            .expect("insert w-3");

        let pending = service.list_pending_wakeups().await;
        assert_eq!(
            pending
                .iter()
                .map(|r| r.wakeup_id.as_str())
                .collect::<Vec<_>>(),
            vec!["w-1", "w-2", "w-3"],
            "pending must be ordered by fire_at_unix ASC"
        );
        assert_eq!(service.count_pending_wakeups_for_session("s-1").await, 2);
        assert_eq!(service.count_pending_wakeups_for_session("s-2").await, 1);
    }

    #[tokio::test]
    async fn mark_fired_is_idempotent_and_gates_on_pending() {
        let service = PersistenceService::in_memory().expect("in-memory service");
        service.insert_session_row_for_tests("s-1").unwrap();
        service
            .insert_wakeup(sample_wakeup("w-1", "s-1", 1_000))
            .await
            .expect("insert");

        assert!(service.mark_wakeup_fired("w-1").await, "first fire wins");
        assert!(
            !service.mark_wakeup_fired("w-1").await,
            "second fire is a no-op"
        );
        assert!(
            service.list_pending_wakeups().await.is_empty(),
            "fired row leaves pending set"
        );
    }

    /// `TurnRecord.source` survives a write/read round-trip through
    /// the live `turns` table — guards against a future column-list
    /// edit dropping the bind site or row index.
    #[tokio::test]
    async fn turn_source_round_trips_through_live_turns() {
        use zenui_provider_api::{
            SessionDetail, SessionStatus, SessionSummary, TurnSource, TurnStatus,
        };

        let service = PersistenceService::in_memory().expect("in-memory service");
        let now = chrono::Utc::now().to_rfc3339();
        let mut session = SessionDetail {
            summary: SessionSummary {
                session_id: "s-src".to_string(),
                provider: ProviderKind::Claude,
                status: SessionStatus::Ready,
                created_at: now.clone(),
                updated_at: now.clone(),
                turn_count: 0,
                model: None,
                project_id: None,
            },
            turns: Vec::new(),
            provider_state: None,
            cwd: None,
        };
        let mk_turn = |id: &str, source: TurnSource| TurnRecord {
            turn_id: id.to_string(),
            input: format!("input for {id}"),
            output: String::new(),
            status: TurnStatus::Completed,
            source,
            created_at: now.clone(),
            updated_at: now.clone(),
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
        };
        session.turns.push(mk_turn("t-user", TurnSource::User));
        session.turns.push(mk_turn("t-wake", TurnSource::Wakeup));
        session.turns.push(mk_turn("t-send", TurnSource::PeerSend));
        session.turns.push(mk_turn("t-spawn", TurnSource::PeerSpawn));
        session.summary.turn_count = session.turns.len();

        service.upsert_session(session).await;

        let loaded = service
            .get_session("s-src")
            .await
            .expect("session loads back");
        let by_id: std::collections::HashMap<_, _> = loaded
            .turns
            .iter()
            .map(|t| (t.turn_id.as_str(), t.source))
            .collect();
        assert_eq!(by_id.get("t-user"), Some(&TurnSource::User));
        assert_eq!(by_id.get("t-wake"), Some(&TurnSource::Wakeup));
        assert_eq!(by_id.get("t-send"), Some(&TurnSource::PeerSend));
        assert_eq!(by_id.get("t-spawn"), Some(&TurnSource::PeerSpawn));
    }

    /// A row written before the `source` column existed (NULL in
    /// SQLite) decodes to `TurnSource::User`, matching the
    /// `#[serde(default)]` on the wire type. This is the legacy-data
    /// safety net — without it, every pre-migration turn would
    /// suddenly render with a wakeup chip.
    #[tokio::test]
    async fn legacy_turn_with_null_source_decodes_as_user() {
        use zenui_provider_api::TurnSource;

        let service = PersistenceService::in_memory().expect("in-memory service");
        service.insert_session_row_for_tests("s-legacy").unwrap();

        // Hand-craft an INSERT that omits the `source` column entirely
        // — sqlite stores NULL — to simulate a row written by the
        // pre-migration daemon.
        {
            let connection = service.connection.lock();
            connection
                .execute(
                    "INSERT INTO turns (
                        turn_id, session_id, input, output, status, created_at, updated_at
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    params![
                        "t-legacy",
                        "s-legacy",
                        "old prompt",
                        "",
                        "completed",
                        "2020-01-01T00:00:00Z",
                        "2020-01-01T00:00:00Z",
                    ],
                )
                .unwrap();
        }

        let loaded = service
            .get_session("s-legacy")
            .await
            .expect("session loads back");
        let turn = loaded
            .turns
            .iter()
            .find(|t| t.turn_id == "t-legacy")
            .expect("legacy turn loads");
        assert_eq!(
            turn.source,
            TurnSource::User,
            "NULL source must decode as User"
        );
    }

    #[tokio::test]
    async fn cancel_wakeups_for_session_returns_affected_ids() {
        let service = PersistenceService::in_memory().expect("in-memory service");
        service.insert_session_row_for_tests("s-1").unwrap();
        for (i, id) in ["w-a", "w-b", "w-c"].iter().enumerate() {
            service
                .insert_wakeup(sample_wakeup(id, "s-1", 1_000 + i as i64))
                .await
                .expect("insert");
        }
        assert!(service.mark_wakeup_fired("w-a").await);

        let mut cancelled = service.cancel_wakeups_for_session("s-1").await;
        cancelled.sort();
        assert_eq!(cancelled, vec!["w-b".to_string(), "w-c".to_string()]);
        assert_eq!(service.count_pending_wakeups_for_session("s-1").await, 0);
        assert!(service.cancel_wakeups_for_session("s-1").await.is_empty());
    }
}
