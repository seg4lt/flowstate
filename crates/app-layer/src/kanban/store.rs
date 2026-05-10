//! Persistence layer for the kanban orchestrator feature.
//!
//! Opens a **dedicated SQLite file** (`kanban.sqlite`) next to
//! `user_config.sqlite` under the flowstate data dir. Separate file
//! rather than another set of tables in `user_config` because:
//!
//! - The orchestrator feature is gated behind a setting and ships
//!   OFF by default. Keeping it in its own file means a user who
//!   never enables it never even opens this DB.
//! - Schema lifecycle is independent. Devs can blow away
//!   `kanban.sqlite` to reset the board without touching the user's
//!   accumulated session/project display labels.
//! - The data is **not** an SDK runtime concern (per
//!   `crates/core/persistence/CLAUDE.md`), so it has no business
//!   sharing storage with the agent runtime's tables either.
//!
//! Connection pattern mirrors `UserConfigStore` exactly:
//! `Arc<Mutex<Connection>>` — `Mutex<Connection>` is neither `Sync`
//! nor `Clone` on its own, the Arc lets handlers + the tick loop
//! share a cheap clone, and lock duration is bounded to one query.

use std::path::Path;
use std::sync::{Arc, Mutex};

use rusqlite::{Connection, OptionalExtension, params};

use super::model::{
    CommentAuthor, KeyDirectory, OrchestratorSetting, ProjectMemory, SessionRole, Task,
    TaskComment, TaskSession, TaskState,
};

/// Handle to the kanban SQLite DB. Cheap to clone.
#[derive(Clone)]
pub struct KanbanStore {
    connection: Arc<Mutex<Connection>>,
}

impl KanbanStore {
    /// Open (or create) `<data_dir>/kanban.sqlite` and bootstrap
    /// schema. Failing here is fatal at the caller's discretion —
    /// the Tauri shell currently treats `UserConfigStore::open`
    /// failures as soft (logs + continues), and we do the same.
    pub fn open(data_dir: &Path) -> Result<Self, String> {
        if let Err(e) = std::fs::create_dir_all(data_dir) {
            return Err(format!("create data dir: {e}"));
        }
        let db_path = data_dir.join("kanban.sqlite");
        let connection =
            Connection::open(&db_path).map_err(|e| format!("open kanban sqlite: {e}"))?;
        // Foreign keys are off by default in SQLite — turn them on
        // so the CASCADE deletes on `kanban_comments` /
        // `task_sessions` actually fire when a task is dropped.
        connection
            .execute_batch("PRAGMA foreign_keys = ON;")
            .map_err(|e| format!("enable foreign_keys pragma: {e}"))?;
        connection
            .execute_batch(SCHEMA_SQL)
            .map_err(|e| format!("create kanban schema: {e}"))?;
        Ok(Self {
            connection: Arc::new(Mutex::new(connection)),
        })
    }

    /// Construct an in-memory store for tests. Same schema, no
    /// disk. The handle is otherwise identical.
    #[cfg(test)]
    pub fn in_memory() -> Result<Self, String> {
        let connection = Connection::open_in_memory()
            .map_err(|e| format!("open in-memory sqlite: {e}"))?;
        connection
            .execute_batch("PRAGMA foreign_keys = ON;")
            .map_err(|e| format!("enable foreign_keys pragma: {e}"))?;
        connection
            .execute_batch(SCHEMA_SQL)
            .map_err(|e| format!("create kanban schema: {e}"))?;
        Ok(Self {
            connection: Arc::new(Mutex::new(connection)),
        })
    }

    // ── tasks ─────────────────────────────────────────────────────

    /// Insert a new task with a generated `task_id`. State always
    /// starts as `Open` regardless of caller intent — only the
    /// triage path can move it forward.
    pub fn insert_task(&self, title: &str, body: &str) -> Result<Task, String> {
        let conn = lock(&self.connection);
        let task_id = format!("task_{}", uuid::Uuid::new_v4().simple());
        let now = now_unix_secs();
        conn.execute(
            "INSERT INTO kanban_tasks
                (task_id, title, body, state,
                 project_id, worktree_project_id, branch,
                 orchestrator_session_id, needs_human_reason,
                 created_at, updated_at)
             VALUES
                (?1, ?2, ?3, 'Open',
                 NULL, NULL, NULL,
                 NULL, NULL,
                 ?4, ?4)",
            params![task_id, title, body, now],
        )
        .map_err(|e| format!("insert_task: {e}"))?;
        Ok(Task {
            task_id,
            title: title.to_string(),
            body: body.to_string(),
            state: TaskState::Open,
            project_id: None,
            worktree_project_id: None,
            branch: None,
            orchestrator_session_id: None,
            needs_human_reason: None,
            created_at: now,
            updated_at: now,
        })
    }

    pub fn get_task(&self, task_id: &str) -> Result<Option<Task>, String> {
        let conn = lock(&self.connection);
        conn.query_row(
            "SELECT task_id, title, body, state,
                    project_id, worktree_project_id, branch,
                    orchestrator_session_id, needs_human_reason,
                    created_at, updated_at
             FROM kanban_tasks WHERE task_id = ?1",
            params![task_id],
            row_to_task,
        )
        .optional()
        .map_err(|e| format!("get_task: {e}"))
    }

    /// List every task, newest-first. Cheap because the table is
    /// bounded by human-scale board size.
    pub fn list_tasks(&self) -> Result<Vec<Task>, String> {
        let conn = lock(&self.connection);
        let mut stmt = conn
            .prepare(
                "SELECT task_id, title, body, state,
                        project_id, worktree_project_id, branch,
                        orchestrator_session_id, needs_human_reason,
                        created_at, updated_at
                 FROM kanban_tasks
                 ORDER BY created_at DESC",
            )
            .map_err(|e| format!("prepare list_tasks: {e}"))?;
        let rows = stmt
            .query_map([], row_to_task)
            .map_err(|e| format!("query list_tasks: {e}"))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| format!("read list_tasks row: {e}"))?);
        }
        Ok(out)
    }

    /// Tasks the tick loop should consider. Filtered in SQL via the
    /// partial index `idx_tasks_active`, then narrowed to actionable
    /// states in code (the partial index excludes Done/Cancelled;
    /// `TaskState::is_actionable` further excludes HumanReview /
    /// NeedsHuman, which the loop ignores).
    pub fn list_actionable_tasks(&self) -> Result<Vec<Task>, String> {
        let conn = lock(&self.connection);
        let mut stmt = conn
            .prepare(
                "SELECT task_id, title, body, state,
                        project_id, worktree_project_id, branch,
                        orchestrator_session_id, needs_human_reason,
                        created_at, updated_at
                 FROM kanban_tasks
                 WHERE state NOT IN ('Done','Cancelled','HumanReview','NeedsHuman')
                 ORDER BY created_at ASC",
            )
            .map_err(|e| format!("prepare list_actionable_tasks: {e}"))?;
        let rows = stmt
            .query_map([], row_to_task)
            .map_err(|e| format!("query list_actionable_tasks: {e}"))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| format!("read row: {e}"))?);
        }
        Ok(out)
    }

    /// Patch a task's mutable fields. Pass `None` to leave a column
    /// untouched. State transitions should go through
    /// `service::validate_transition` before calling this — the
    /// store doesn't enforce the FSM, only the legal-string set.
    #[allow(clippy::too_many_arguments)]
    pub fn update_task(
        &self,
        task_id: &str,
        title: Option<&str>,
        body: Option<&str>,
        state: Option<TaskState>,
        project_id: Option<Option<&str>>,
        worktree_project_id: Option<Option<&str>>,
        branch: Option<Option<&str>>,
        orchestrator_session_id: Option<Option<&str>>,
        needs_human_reason: Option<Option<&str>>,
    ) -> Result<(), String> {
        let conn = lock(&self.connection);
        let now = now_unix_secs();
        // Build a dynamic SET clause to avoid clobbering unchanged
        // columns. Bind values go into `params` in lockstep.
        let mut set_parts: Vec<String> = Vec::new();
        let mut binds: Vec<rusqlite::types::Value> = Vec::new();
        if let Some(v) = title {
            set_parts.push(format!("title = ?{}", set_parts.len() + 1));
            binds.push(v.to_string().into());
        }
        if let Some(v) = body {
            set_parts.push(format!("body = ?{}", set_parts.len() + 1));
            binds.push(v.to_string().into());
        }
        if let Some(v) = state {
            set_parts.push(format!("state = ?{}", set_parts.len() + 1));
            binds.push(v.as_str().to_string().into());
        }
        if let Some(v) = project_id {
            set_parts.push(format!("project_id = ?{}", set_parts.len() + 1));
            binds.push(opt_str_to_value(v));
        }
        if let Some(v) = worktree_project_id {
            set_parts.push(format!("worktree_project_id = ?{}", set_parts.len() + 1));
            binds.push(opt_str_to_value(v));
        }
        if let Some(v) = branch {
            set_parts.push(format!("branch = ?{}", set_parts.len() + 1));
            binds.push(opt_str_to_value(v));
        }
        if let Some(v) = orchestrator_session_id {
            set_parts.push(format!(
                "orchestrator_session_id = ?{}",
                set_parts.len() + 1
            ));
            binds.push(opt_str_to_value(v));
        }
        if let Some(v) = needs_human_reason {
            set_parts.push(format!("needs_human_reason = ?{}", set_parts.len() + 1));
            binds.push(opt_str_to_value(v));
        }
        if set_parts.is_empty() {
            // Nothing to do — but still bump updated_at so the UI
            // notices "touch" events from the caller.
            conn.execute(
                "UPDATE kanban_tasks SET updated_at = ?1 WHERE task_id = ?2",
                params![now, task_id],
            )
            .map_err(|e| format!("touch task: {e}"))?;
            return Ok(());
        }
        set_parts.push(format!("updated_at = ?{}", set_parts.len() + 1));
        binds.push(now.into());
        let sql = format!(
            "UPDATE kanban_tasks SET {} WHERE task_id = ?{}",
            set_parts.join(", "),
            binds.len() + 1
        );
        binds.push(task_id.to_string().into());
        let params_refs: Vec<&dyn rusqlite::ToSql> =
            binds.iter().map(|v| v as &dyn rusqlite::ToSql).collect();
        conn.execute(&sql, params_refs.as_slice())
            .map_err(|e| format!("update_task: {e}"))?;
        Ok(())
    }

    /// Hard-delete a task and (via FK CASCADE) its comments and
    /// session links. Intended for the rare `task_cancel` flow
    /// where the user wants the row gone, not just Cancelled.
    /// Most callers should set `state = Cancelled` instead.
    pub fn delete_task(&self, task_id: &str) -> Result<(), String> {
        let conn = lock(&self.connection);
        conn.execute("DELETE FROM kanban_tasks WHERE task_id = ?1", params![task_id])
            .map_err(|e| format!("delete_task: {e}"))?;
        Ok(())
    }

    // ── comments ──────────────────────────────────────────────────

    pub fn insert_comment(
        &self,
        task_id: &str,
        author: CommentAuthor,
        body: &str,
    ) -> Result<TaskComment, String> {
        let conn = lock(&self.connection);
        let comment_id = format!("cmt_{}", uuid::Uuid::new_v4().simple());
        let now = now_unix_secs();
        conn.execute(
            "INSERT INTO kanban_comments (comment_id, task_id, author, body, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![comment_id, task_id, author.as_str(), body, now],
        )
        .map_err(|e| format!("insert_comment: {e}"))?;
        // Bump task updated_at so list views resort correctly.
        conn.execute(
            "UPDATE kanban_tasks SET updated_at = ?1 WHERE task_id = ?2",
            params![now, task_id],
        )
        .map_err(|e| format!("bump task updated_at after comment: {e}"))?;
        Ok(TaskComment {
            comment_id,
            task_id: task_id.to_string(),
            author,
            body: body.to_string(),
            created_at: now,
        })
    }

    pub fn list_comments(&self, task_id: &str) -> Result<Vec<TaskComment>, String> {
        let conn = lock(&self.connection);
        let mut stmt = conn
            .prepare(
                "SELECT comment_id, task_id, author, body, created_at
                 FROM kanban_comments
                 WHERE task_id = ?1
                 ORDER BY created_at ASC",
            )
            .map_err(|e| format!("prepare list_comments: {e}"))?;
        let rows = stmt
            .query_map(params![task_id], |row| {
                let author_str: String = row.get(2)?;
                let author = CommentAuthor::from_str(&author_str).ok_or_else(|| {
                    rusqlite::Error::FromSqlConversionFailure(
                        2,
                        rusqlite::types::Type::Text,
                        format!("unknown comment author '{author_str}'").into(),
                    )
                })?;
                Ok(TaskComment {
                    comment_id: row.get(0)?,
                    task_id: row.get(1)?,
                    author,
                    body: row.get(3)?,
                    created_at: row.get(4)?,
                })
            })
            .map_err(|e| format!("query list_comments: {e}"))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| format!("read row: {e}"))?);
        }
        Ok(out)
    }

    // ── task_sessions ─────────────────────────────────────────────

    /// Link a flowstate session to a task with a given role. The
    /// caller is responsible for spawning the session first; we
    /// just record the relationship and the role for the audience
    /// check.
    pub fn insert_task_session(
        &self,
        session_id: &str,
        task_id: &str,
        role: SessionRole,
    ) -> Result<TaskSession, String> {
        let conn = lock(&self.connection);
        let now = now_unix_secs();
        conn.execute(
            "INSERT INTO task_sessions (session_id, task_id, role, created_at, retired_at)
             VALUES (?1, ?2, ?3, ?4, NULL)",
            params![session_id, task_id, role.as_str(), now],
        )
        .map_err(|e| format!("insert_task_session: {e}"))?;
        Ok(TaskSession {
            session_id: session_id.to_string(),
            task_id: task_id.to_string(),
            role,
            created_at: now,
            retired_at: None,
        })
    }

    /// Mark a session retired (no longer active for the task).
    /// Used when a one-shot completes, or when the orchestrator
    /// session is archived on Done/Cancelled.
    pub fn retire_task_session(&self, session_id: &str) -> Result<(), String> {
        let conn = lock(&self.connection);
        let now = now_unix_secs();
        conn.execute(
            "UPDATE task_sessions SET retired_at = ?1 WHERE session_id = ?2",
            params![now, session_id],
        )
        .map_err(|e| format!("retire_task_session: {e}"))?;
        Ok(())
    }

    /// Look up a task session by `session_id`. Used by the
    /// orchestrator-MCP dispatch to enforce the audience check —
    /// the caller's session must exist here and have an
    /// orchestrator-audience role, else 403.
    pub fn get_task_session(&self, session_id: &str) -> Result<Option<TaskSession>, String> {
        let conn = lock(&self.connection);
        conn.query_row(
            "SELECT session_id, task_id, role, created_at, retired_at
             FROM task_sessions WHERE session_id = ?1",
            params![session_id],
            row_to_task_session,
        )
        .optional()
        .map_err(|e| format!("get_task_session: {e}"))
    }

    pub fn list_task_sessions(&self, task_id: &str) -> Result<Vec<TaskSession>, String> {
        let conn = lock(&self.connection);
        let mut stmt = conn
            .prepare(
                "SELECT session_id, task_id, role, created_at, retired_at
                 FROM task_sessions
                 WHERE task_id = ?1
                 ORDER BY created_at ASC",
            )
            .map_err(|e| format!("prepare list_task_sessions: {e}"))?;
        let rows = stmt
            .query_map(params![task_id], row_to_task_session)
            .map_err(|e| format!("query list_task_sessions: {e}"))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| format!("read row: {e}"))?);
        }
        Ok(out)
    }

    /// Find the currently-active session of a given role for a
    /// task, if any. Used by the orchestrator to ask "is a coder
    /// already running for this task?"
    pub fn find_active_session(
        &self,
        task_id: &str,
        role: SessionRole,
    ) -> Result<Option<TaskSession>, String> {
        let conn = lock(&self.connection);
        conn.query_row(
            "SELECT session_id, task_id, role, created_at, retired_at
             FROM task_sessions
             WHERE task_id = ?1 AND role = ?2 AND retired_at IS NULL
             ORDER BY created_at DESC
             LIMIT 1",
            params![task_id, role.as_str()],
            row_to_task_session,
        )
        .optional()
        .map_err(|e| format!("find_active_session: {e}"))
    }

    // ── project memory ────────────────────────────────────────────

    pub fn get_project_memory(&self, project_id: &str) -> Result<Option<ProjectMemory>, String> {
        let conn = lock(&self.connection);
        conn.query_row(
            "SELECT project_id, purpose, languages, key_directories,
                    conventions, recent_task_themes, seeded_at, updated_at
             FROM project_memory WHERE project_id = ?1",
            params![project_id],
            row_to_project_memory,
        )
        .optional()
        .map_err(|e| format!("get_project_memory: {e}"))
    }

    pub fn list_project_memory(&self) -> Result<Vec<ProjectMemory>, String> {
        let conn = lock(&self.connection);
        let mut stmt = conn
            .prepare(
                "SELECT project_id, purpose, languages, key_directories,
                        conventions, recent_task_themes, seeded_at, updated_at
                 FROM project_memory
                 ORDER BY updated_at DESC",
            )
            .map_err(|e| format!("prepare list_project_memory: {e}"))?;
        let rows = stmt
            .query_map([], row_to_project_memory)
            .map_err(|e| format!("query list_project_memory: {e}"))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| format!("read row: {e}"))?);
        }
        Ok(out)
    }

    /// Upsert a project memory row. `seeded_at` is set only on the
    /// first insert (or when `seeded_at` is `Some` on the input,
    /// for tests). Subsequent updates leave it alone.
    pub fn upsert_project_memory(&self, memory: &ProjectMemory) -> Result<(), String> {
        let conn = lock(&self.connection);
        let now = now_unix_secs();
        let languages = serde_json::to_string(&memory.languages)
            .map_err(|e| format!("encode languages: {e}"))?;
        let key_dirs = serde_json::to_string(&memory.key_directories)
            .map_err(|e| format!("encode key_directories: {e}"))?;
        let conventions = serde_json::to_string(&memory.conventions)
            .map_err(|e| format!("encode conventions: {e}"))?;
        let themes = serde_json::to_string(&memory.recent_task_themes)
            .map_err(|e| format!("encode recent_task_themes: {e}"))?;
        // If caller didn't pass seeded_at, use `now` only on first insert;
        // COALESCE preserves an existing value across re-upserts.
        let seeded_at_param: Option<i64> = memory.seeded_at;
        conn.execute(
            "INSERT INTO project_memory
                (project_id, purpose, languages, key_directories,
                 conventions, recent_task_themes, seeded_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, COALESCE(?7, ?8), ?8)
             ON CONFLICT(project_id) DO UPDATE SET
                purpose = excluded.purpose,
                languages = excluded.languages,
                key_directories = excluded.key_directories,
                conventions = excluded.conventions,
                recent_task_themes = excluded.recent_task_themes,
                seeded_at = COALESCE(project_memory.seeded_at, excluded.seeded_at),
                updated_at = excluded.updated_at",
            params![
                memory.project_id,
                memory.purpose,
                languages,
                key_dirs,
                conventions,
                themes,
                seeded_at_param,
                now,
            ],
        )
        .map_err(|e| format!("upsert_project_memory: {e}"))?;
        Ok(())
    }

    // ── orchestrator settings ─────────────────────────────────────

    pub fn get_setting(&self, key: &str) -> Result<Option<String>, String> {
        let conn = lock(&self.connection);
        conn.query_row(
            "SELECT value FROM orchestrator_settings WHERE key = ?1",
            params![key],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(|e| format!("get_setting: {e}"))
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<(), String> {
        let conn = lock(&self.connection);
        let now = now_unix_secs();
        conn.execute(
            "INSERT INTO orchestrator_settings (key, value, updated_at)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(key) DO UPDATE SET
                value = excluded.value,
                updated_at = excluded.updated_at",
            params![key, value, now],
        )
        .map_err(|e| format!("set_setting: {e}"))?;
        Ok(())
    }

    pub fn list_settings(&self) -> Result<Vec<OrchestratorSetting>, String> {
        let conn = lock(&self.connection);
        let mut stmt = conn
            .prepare("SELECT key, value, updated_at FROM orchestrator_settings")
            .map_err(|e| format!("prepare list_settings: {e}"))?;
        let rows = stmt
            .query_map([], |row| {
                Ok(OrchestratorSetting {
                    key: row.get(0)?,
                    value: row.get(1)?,
                    updated_at: row.get(2)?,
                })
            })
            .map_err(|e| format!("query list_settings: {e}"))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| format!("read row: {e}"))?);
        }
        Ok(out)
    }

    /// Convenience for the feature flag — defaults to `false`.
    pub fn feature_enabled(&self) -> Result<bool, String> {
        Ok(self
            .get_setting(super::model::settings_keys::FEATURE_ENABLED)?
            .map(|v| v == "true")
            .unwrap_or(false))
    }

    /// Convenience for the tick toggle — defaults to `false`.
    pub fn tick_enabled(&self) -> Result<bool, String> {
        Ok(self
            .get_setting(super::model::settings_keys::TICK_ENABLED)?
            .map(|v| v == "true")
            .unwrap_or(false))
    }

    /// Convenience for the tick interval — defaults to 10_000ms.
    pub fn tick_interval_ms(&self) -> Result<u64, String> {
        match self.get_setting(super::model::settings_keys::TICK_INTERVAL_MS)? {
            Some(v) => v
                .parse::<u64>()
                .map_err(|e| format!("parse tick_interval_ms '{v}': {e}")),
            None => Ok(10_000),
        }
    }

    /// Convenience for the parallelism cap — defaults to 3.
    pub fn max_parallel_tasks(&self) -> Result<u64, String> {
        match self.get_setting(super::model::settings_keys::MAX_PARALLEL_TASKS)? {
            Some(v) => v
                .parse::<u64>()
                .map_err(|e| format!("parse max_parallel_tasks '{v}': {e}")),
            None => Ok(3),
        }
    }

    // ── task dependencies ─────────────────────────────────────────

    /// Add a `task_id depends on depends_on` edge. Idempotent —
    /// inserting the same edge twice is silently a no-op.
    /// Self-dependency is rejected by the SQL CHECK constraint.
    pub fn add_task_dependency(
        &self,
        task_id: &str,
        depends_on: &str,
    ) -> Result<(), String> {
        if task_id == depends_on {
            return Err("task cannot depend on itself".to_string());
        }
        let conn = lock(&self.connection);
        let now = now_unix_secs();
        conn.execute(
            "INSERT OR IGNORE INTO kanban_task_deps
                (task_id, depends_on_task_id, created_at)
             VALUES (?1, ?2, ?3)",
            params![task_id, depends_on, now],
        )
        .map_err(|e| format!("add_task_dependency: {e}"))?;
        Ok(())
    }

    /// List the task_ids that `task_id` is currently waiting on.
    /// Returns ALL dependencies regardless of their current state;
    /// the caller filters to "still blocking" using `list_tasks` /
    /// `get_task` and the FSM's terminal-state predicate.
    pub fn list_task_dependencies(&self, task_id: &str) -> Result<Vec<String>, String> {
        let conn = lock(&self.connection);
        let mut stmt = conn
            .prepare(
                "SELECT depends_on_task_id FROM kanban_task_deps
                 WHERE task_id = ?1",
            )
            .map_err(|e| format!("prepare list_task_dependencies: {e}"))?;
        let rows = stmt
            .query_map(params![task_id], |row| row.get::<_, String>(0))
            .map_err(|e| format!("query list_task_dependencies: {e}"))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| format!("read row: {e}"))?);
        }
        Ok(out)
    }

    /// Return only those dependencies that are STILL BLOCKING the
    /// given task — i.e. the dep's state is not Done/Cancelled.
    /// This is what the tick loop calls to decide whether to
    /// advance the task past Ready.
    pub fn unresolved_dependencies(&self, task_id: &str) -> Result<Vec<String>, String> {
        let conn = lock(&self.connection);
        let mut stmt = conn
            .prepare(
                "SELECT d.depends_on_task_id FROM kanban_task_deps d
                 JOIN kanban_tasks t ON t.task_id = d.depends_on_task_id
                 WHERE d.task_id = ?1
                   AND t.state NOT IN ('Done','Cancelled')",
            )
            .map_err(|e| format!("prepare unresolved_dependencies: {e}"))?;
        let rows = stmt
            .query_map(params![task_id], |row| row.get::<_, String>(0))
            .map_err(|e| format!("query unresolved_dependencies: {e}"))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| format!("read row: {e}"))?);
        }
        Ok(out)
    }

    pub fn remove_task_dependency(
        &self,
        task_id: &str,
        depends_on: &str,
    ) -> Result<(), String> {
        let conn = lock(&self.connection);
        conn.execute(
            "DELETE FROM kanban_task_deps
             WHERE task_id = ?1 AND depends_on_task_id = ?2",
            params![task_id, depends_on],
        )
        .map_err(|e| format!("remove_task_dependency: {e}"))?;
        Ok(())
    }

    /// Count tasks currently occupying a "spawn slot" — the
    /// states where an agent is actively running or about to be.
    /// Used by the tick loop to gate Ready→Code transitions
    /// when the parallelism cap is reached.
    pub fn count_active_spawn_slots(&self) -> Result<u64, String> {
        let conn = lock(&self.connection);
        conn.query_row(
            "SELECT COUNT(*) FROM kanban_tasks
             WHERE state IN ('Code','AgentReview','Merge')",
            [],
            |row| row.get::<_, i64>(0),
        )
        .map(|n| n as u64)
        .map_err(|e| format!("count_active_spawn_slots: {e}"))
    }
}

// ── helpers ───────────────────────────────────────────────────────

fn lock(m: &Arc<Mutex<Connection>>) -> std::sync::MutexGuard<'_, Connection> {
    // Same poison-tolerant pattern as `UserConfigStore::get`. We
    // never hold the lock across panic-unsafe code, so unwrapping
    // a poisoned guard is fine.
    match m.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn now_unix_secs() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn opt_str_to_value(v: Option<&str>) -> rusqlite::types::Value {
    match v {
        Some(s) => rusqlite::types::Value::Text(s.to_string()),
        None => rusqlite::types::Value::Null,
    }
}

fn row_to_task(row: &rusqlite::Row<'_>) -> rusqlite::Result<Task> {
    let state_str: String = row.get(3)?;
    let state = TaskState::from_str(&state_str).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            3,
            rusqlite::types::Type::Text,
            format!("unknown task state '{state_str}'").into(),
        )
    })?;
    Ok(Task {
        task_id: row.get(0)?,
        title: row.get(1)?,
        body: row.get(2)?,
        state,
        project_id: row.get(4)?,
        worktree_project_id: row.get(5)?,
        branch: row.get(6)?,
        orchestrator_session_id: row.get(7)?,
        needs_human_reason: row.get(8)?,
        created_at: row.get(9)?,
        updated_at: row.get(10)?,
    })
}

fn row_to_task_session(row: &rusqlite::Row<'_>) -> rusqlite::Result<TaskSession> {
    let role_str: String = row.get(2)?;
    let role = SessionRole::from_str(&role_str).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            2,
            rusqlite::types::Type::Text,
            format!("unknown session role '{role_str}'").into(),
        )
    })?;
    Ok(TaskSession {
        session_id: row.get(0)?,
        task_id: row.get(1)?,
        role,
        created_at: row.get(3)?,
        retired_at: row.get(4)?,
    })
}

fn row_to_project_memory(row: &rusqlite::Row<'_>) -> rusqlite::Result<ProjectMemory> {
    let languages_json: Option<String> = row.get(2)?;
    let key_dirs_json: Option<String> = row.get(3)?;
    let conventions_json: Option<String> = row.get(4)?;
    let themes_json: Option<String> = row.get(5)?;
    let languages: Vec<String> = languages_json
        .map(|s| serde_json::from_str(&s).unwrap_or_default())
        .unwrap_or_default();
    let key_directories: Vec<KeyDirectory> = key_dirs_json
        .map(|s| serde_json::from_str(&s).unwrap_or_default())
        .unwrap_or_default();
    let conventions: Vec<String> = conventions_json
        .map(|s| serde_json::from_str(&s).unwrap_or_default())
        .unwrap_or_default();
    let recent_task_themes: Vec<String> = themes_json
        .map(|s| serde_json::from_str(&s).unwrap_or_default())
        .unwrap_or_default();
    Ok(ProjectMemory {
        project_id: row.get(0)?,
        purpose: row.get(1)?,
        languages,
        key_directories,
        conventions,
        recent_task_themes,
        seeded_at: row.get(6)?,
        updated_at: row.get(7)?,
    })
}

const SCHEMA_SQL: &str = "
CREATE TABLE IF NOT EXISTS kanban_tasks (
    task_id TEXT PRIMARY KEY,
    title TEXT NOT NULL,
    body  TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN
        ('Open','Triage','Ready','Code','AgentReview','HumanReview','Merge','Done','NeedsHuman','Cancelled')),
    project_id TEXT,
    worktree_project_id TEXT,
    branch TEXT,
    orchestrator_session_id TEXT,
    needs_human_reason TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_tasks_active ON kanban_tasks(state)
    WHERE state NOT IN ('Done','Cancelled');

CREATE INDEX IF NOT EXISTS idx_tasks_project ON kanban_tasks(project_id);

CREATE TABLE IF NOT EXISTS kanban_comments (
    comment_id TEXT PRIMARY KEY,
    task_id TEXT NOT NULL REFERENCES kanban_tasks(task_id) ON DELETE CASCADE,
    author TEXT NOT NULL CHECK (author IN
        ('user','triage','orchestrator','reviewer','coder','system')),
    body TEXT NOT NULL,
    created_at INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_comments_task ON kanban_comments(task_id, created_at);

CREATE TABLE IF NOT EXISTS task_sessions (
    session_id TEXT PRIMARY KEY,
    task_id TEXT NOT NULL REFERENCES kanban_tasks(task_id) ON DELETE CASCADE,
    role TEXT NOT NULL CHECK (role IN
        ('triage','orchestrator','coder','reviewer','memory_seeder','memory_updater')),
    created_at INTEGER NOT NULL,
    retired_at INTEGER
);

CREATE INDEX IF NOT EXISTS idx_task_sessions_task ON task_sessions(task_id);
CREATE INDEX IF NOT EXISTS idx_task_sessions_role_active
    ON task_sessions(role) WHERE retired_at IS NULL;

CREATE TABLE IF NOT EXISTS project_memory (
    project_id TEXT PRIMARY KEY,
    purpose TEXT,
    languages TEXT,
    key_directories TEXT,
    conventions TEXT,
    recent_task_themes TEXT,
    seeded_at INTEGER,
    updated_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS orchestrator_settings (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL,
    updated_at INTEGER NOT NULL
);

-- Directed dependency edges between tasks.
-- (task_id) depends on (depends_on_task_id) — i.e. the task at
-- task_id should not advance past Ready until depends_on_task_id
-- reaches a terminal state (Done or Cancelled). Both columns FK
-- with CASCADE so deleting a task wipes its edges cleanly.
CREATE TABLE IF NOT EXISTS kanban_task_deps (
    task_id TEXT NOT NULL REFERENCES kanban_tasks(task_id) ON DELETE CASCADE,
    depends_on_task_id TEXT NOT NULL REFERENCES kanban_tasks(task_id) ON DELETE CASCADE,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (task_id, depends_on_task_id),
    -- A task depending on itself would deadlock the loop forever.
    CHECK (task_id <> depends_on_task_id)
);
CREATE INDEX IF NOT EXISTS idx_task_deps_blocking ON kanban_task_deps(depends_on_task_id);
";

#[cfg(test)]
mod tests {
    use super::super::model::{CommentAuthor, ProjectMemory, SessionRole, TaskState};
    use super::*;

    #[test]
    fn schema_bootstraps_idempotently() {
        let s = KanbanStore::in_memory().unwrap();
        // Re-running the schema must be a no-op.
        let conn = lock(&s.connection);
        conn.execute_batch(SCHEMA_SQL).unwrap();
    }

    #[test]
    fn insert_and_list_tasks() {
        let s = KanbanStore::in_memory().unwrap();
        let t1 = s.insert_task("title", "body").unwrap();
        assert_eq!(t1.state, TaskState::Open);
        let listed = s.list_tasks().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].task_id, t1.task_id);
    }

    #[test]
    fn update_task_only_touches_specified_columns() {
        let s = KanbanStore::in_memory().unwrap();
        let t = s.insert_task("title", "body").unwrap();
        s.update_task(
            &t.task_id,
            None,
            None,
            Some(TaskState::Triage),
            Some(Some("project_xyz")),
            None,
            None,
            None,
            None,
        )
        .unwrap();
        let got = s.get_task(&t.task_id).unwrap().unwrap();
        assert_eq!(got.state, TaskState::Triage);
        assert_eq!(got.project_id.as_deref(), Some("project_xyz"));
        // Untouched fields stay put.
        assert_eq!(got.title, "title");
        assert_eq!(got.body, "body");
    }

    #[test]
    fn actionable_excludes_terminal_and_human_gated() {
        let s = KanbanStore::in_memory().unwrap();
        let a = s.insert_task("A", "").unwrap();
        let b = s.insert_task("B", "").unwrap();
        let c = s.insert_task("C", "").unwrap();
        let d = s.insert_task("D", "").unwrap();
        s.update_task(&a.task_id, None, None, Some(TaskState::Code), None, None, None, None, None)
            .unwrap();
        s.update_task(&b.task_id, None, None, Some(TaskState::Done), None, None, None, None, None)
            .unwrap();
        s.update_task(
            &c.task_id,
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
        s.update_task(
            &d.task_id,
            None,
            None,
            Some(TaskState::NeedsHuman),
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        let actionable = s.list_actionable_tasks().unwrap();
        assert_eq!(actionable.len(), 1);
        assert_eq!(actionable[0].task_id, a.task_id);
    }

    #[test]
    fn comments_round_trip() {
        let s = KanbanStore::in_memory().unwrap();
        let t = s.insert_task("title", "body").unwrap();
        s.insert_comment(&t.task_id, CommentAuthor::User, "hello")
            .unwrap();
        s.insert_comment(&t.task_id, CommentAuthor::Triage, "noted")
            .unwrap();
        let cs = s.list_comments(&t.task_id).unwrap();
        assert_eq!(cs.len(), 2);
        assert_eq!(cs[0].author, CommentAuthor::User);
        assert_eq!(cs[1].author, CommentAuthor::Triage);
    }

    #[test]
    fn task_sessions_audience_lookup() {
        let s = KanbanStore::in_memory().unwrap();
        let t = s.insert_task("title", "body").unwrap();
        s.insert_task_session("sess_orch", &t.task_id, SessionRole::Orchestrator)
            .unwrap();
        s.insert_task_session("sess_coder", &t.task_id, SessionRole::Coder)
            .unwrap();
        let orch = s.get_task_session("sess_orch").unwrap().unwrap();
        assert!(orch.role.is_orchestrator_audience());
        let coder = s.get_task_session("sess_coder").unwrap().unwrap();
        assert!(!coder.role.is_orchestrator_audience());
    }

    #[test]
    fn retire_session_marks_retired_at() {
        let s = KanbanStore::in_memory().unwrap();
        let t = s.insert_task("title", "body").unwrap();
        s.insert_task_session("sess1", &t.task_id, SessionRole::Coder)
            .unwrap();
        s.retire_task_session("sess1").unwrap();
        let got = s.get_task_session("sess1").unwrap().unwrap();
        assert!(got.retired_at.is_some());
    }

    #[test]
    fn project_memory_round_trip_and_seeded_at_is_sticky() {
        let s = KanbanStore::in_memory().unwrap();
        let mut m = ProjectMemory {
            project_id: "p1".into(),
            purpose: Some("the flowstate orchestrator".into()),
            languages: vec!["rust".into(), "typescript".into()],
            key_directories: vec![],
            conventions: vec!["no auto-commit".into()],
            recent_task_themes: vec!["plumbing".into()],
            seeded_at: None,
            updated_at: 0,
        };
        s.upsert_project_memory(&m).unwrap();
        let got = s.get_project_memory("p1").unwrap().unwrap();
        assert!(got.seeded_at.is_some(), "seeded_at populated on first insert");
        let first_seeded_at = got.seeded_at.unwrap();
        // Second upsert with a different seeded_at must not override.
        m.purpose = Some("updated purpose".into());
        m.seeded_at = Some(99999);
        s.upsert_project_memory(&m).unwrap();
        let got2 = s.get_project_memory("p1").unwrap().unwrap();
        assert_eq!(got2.purpose.as_deref(), Some("updated purpose"));
        assert_eq!(got2.seeded_at, Some(first_seeded_at));
    }

    #[test]
    fn settings_defaults() {
        let s = KanbanStore::in_memory().unwrap();
        assert!(!s.feature_enabled().unwrap());
        assert!(!s.tick_enabled().unwrap());
        assert_eq!(s.tick_interval_ms().unwrap(), 10_000);
        s.set_setting(super::super::model::settings_keys::FEATURE_ENABLED, "true")
            .unwrap();
        assert!(s.feature_enabled().unwrap());
    }

    #[test]
    fn cascade_delete_removes_children() {
        let s = KanbanStore::in_memory().unwrap();
        let t = s.insert_task("title", "body").unwrap();
        s.insert_comment(&t.task_id, CommentAuthor::User, "x").unwrap();
        s.insert_task_session("sess1", &t.task_id, SessionRole::Triage)
            .unwrap();
        s.delete_task(&t.task_id).unwrap();
        assert!(s.list_comments(&t.task_id).unwrap().is_empty());
        assert!(s.get_task_session("sess1").unwrap().is_none());
    }
}
