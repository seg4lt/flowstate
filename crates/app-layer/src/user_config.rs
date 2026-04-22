// Flowstate-app-owned store for app-level state.
//
// Lives in its own SQLite file under Tauri's app_data_dir,
// deliberately separate from the agent SDK's persistence layer:
// the SDK owns session/agent state and anything the runtime
// needs to execute or resume agents; the app owns everything
// else — UI tunables and per-session/per-project display
// metadata (titles, names, previews) that the runtime never
// reads. There is no schema sharing, no shared connection, no
// overlap. Adding new app-level state means editing this file;
// it never touches the SDK.
//
// Tables in this file:
//   * `user_config`     — kv table for global toggles
//                         (highlighter pool size, etc.)
//   * `session_display` — per-session display labels (title,
//                         last-turn preview) keyed by session_id
//   * `project_display` — per-project display labels (name,
//                         sort order) keyed by project_id
//   * `project_worktree` — parent/child link marking an SDK
//                         project as a git worktree of another
//                         SDK project. Flowstate groups them under
//                         the parent in the sidebar; the SDK never
//                         sees this relationship.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionDisplay {
    pub title: Option<String>,
    pub last_turn_preview: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectDisplay {
    pub name: Option<String>,
    pub sort_order: Option<i64>,
}

/// Parent/child link between two SDK projects where the child is a
/// git worktree of the parent. Stored in flowstate's user_config —
/// the agent SDK treats both as ordinary independent projects and
/// has no notion of worktree ancestry. The flowstate sidebar reads
/// this table to group worktree threads under the parent project
/// visually, and the branch-switcher reads it to find-or-create the
/// worktree project when a user clicks or creates a worktree.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectWorktree {
    pub project_id: String,
    pub parent_project_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
}

/// Owned by Tauri state and cloned into the orchestration layer
/// (for the runtime's metadata provider + worktree provisioner).
/// The connection is wrapped in `Arc<Mutex<...>>` so handles can be
/// shared cheaply — `Mutex<Connection>` is neither `Sync` nor
/// `Clone` on its own. The lock is held only for the duration of a
/// single read/write, which is fine — queries are microsecond-level
/// on local SQLite.
#[derive(Clone)]
pub struct UserConfigStore {
    connection: Arc<Mutex<Connection>>,
}

impl UserConfigStore {
    /// Open (or create) the SQLite file at `<data_dir>/user_config.sqlite`
    /// and ensure the schema exists. Called once during Tauri
    /// `setup`. Failing here is fatal — there's nothing useful the
    /// app can do without its config store.
    ///
    /// `data_dir` should be `~/.flowstate` so the file sits next to
    /// the daemon's database in its own dedicated file. SDK and
    /// app each own their own SQLite; this module never touches
    /// the daemon's schema or connection.
    pub fn open(data_dir: &Path) -> Result<Self, String> {
        if let Err(e) = std::fs::create_dir_all(data_dir) {
            return Err(format!("create data dir: {e}"));
        }
        let db_path = data_dir.join("user_config.sqlite");
        let connection =
            Connection::open(&db_path).map_err(|e| format!("open user_config sqlite: {e}"))?;
        connection
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS user_config (
                    key TEXT PRIMARY KEY,
                    value TEXT NOT NULL,
                    updated_at TEXT NOT NULL
                );

                CREATE TABLE IF NOT EXISTS session_display (
                    session_id TEXT PRIMARY KEY,
                    title TEXT,
                    last_turn_preview TEXT,
                    updated_at TEXT NOT NULL
                );

                CREATE TABLE IF NOT EXISTS project_display (
                    project_id TEXT PRIMARY KEY,
                    name TEXT,
                    sort_order INTEGER,
                    updated_at TEXT NOT NULL
                );

                CREATE TABLE IF NOT EXISTS project_worktree (
                    project_id TEXT PRIMARY KEY,
                    parent_project_id TEXT NOT NULL,
                    branch TEXT,
                    updated_at TEXT NOT NULL
                );

                CREATE INDEX IF NOT EXISTS idx_project_worktree_parent
                    ON project_worktree(parent_project_id);",
            )
            .map_err(|e| format!("create user_config schema: {e}"))?;
        Ok(Self {
            connection: Arc::new(Mutex::new(connection)),
        })
    }

    pub fn get(&self, key: &str) -> Result<Option<String>, String> {
        let connection = match self.connection.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        connection
            .query_row(
                "SELECT value FROM user_config WHERE key = ?1",
                params![key],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|e| format!("get user_config: {e}"))
    }

    pub fn set(&self, key: &str, value: &str) -> Result<(), String> {
        let connection = match self.connection.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let now = chrono::Utc::now().to_rfc3339();
        connection
            .execute(
                "INSERT INTO user_config (key, value, updated_at)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(key) DO UPDATE SET
                    value = excluded.value,
                    updated_at = excluded.updated_at",
                params![key, value, now],
            )
            .map_err(|e| format!("set user_config: {e}"))?;
        Ok(())
    }

    /// Resolve the opencode idle-kill TTL.
    ///
    /// Reads `opencode.idle_ttl_seconds` from `user_config` and maps
    /// it to a [`std::time::Duration`]. Semantics:
    ///
    /// - Unset / malformed → `Some(DEFAULT)` — idle-kill defaults ON
    ///   (Phase C). Callers that want to bypass the default pass an
    ///   explicit override to the adapter constructor.
    /// - `"0"` → `Some(Duration::ZERO)`. The adapter treats this as
    ///   "disabled" internally; distinguishing ZERO from None lets a
    ///   user explicitly opt out via config without falling back to
    ///   the default.
    /// - Positive integer → `Some(Duration::from_secs(n))`.
    ///
    /// Default is 10 minutes — long enough to absorb tab-switching
    /// patterns without cold-start pain, short enough that an idle
    /// laptop doesn't keep `opencode serve` resident indefinitely.
    pub fn opencode_idle_ttl(&self) -> std::time::Duration {
        const DEFAULT_SECS: u64 = 600;
        match self.get("opencode.idle_ttl_seconds") {
            Ok(Some(raw)) => match raw.trim().parse::<u64>() {
                Ok(secs) => std::time::Duration::from_secs(secs),
                Err(_) => std::time::Duration::from_secs(DEFAULT_SECS),
            },
            _ => std::time::Duration::from_secs(DEFAULT_SECS),
        }
    }

    /// Persist the opencode idle-kill TTL. `0` disables idle-kill;
    /// any positive value sets the window. Errors propagate so a
    /// Settings-UI save path can surface them to the user.
    pub fn set_opencode_idle_ttl_seconds(&self, secs: u64) -> Result<(), String> {
        self.set("opencode.idle_ttl_seconds", &secs.to_string())
    }

    pub fn set_session_display(
        &self,
        session_id: &str,
        display: &SessionDisplay,
    ) -> Result<(), String> {
        let connection = match self.connection.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let now = chrono::Utc::now().to_rfc3339();
        connection
            .execute(
                "INSERT INTO session_display
                    (session_id, title, last_turn_preview, updated_at)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(session_id) DO UPDATE SET
                    title = excluded.title,
                    last_turn_preview = excluded.last_turn_preview,
                    updated_at = excluded.updated_at",
                params![session_id, display.title, display.last_turn_preview, now],
            )
            .map_err(|e| format!("set session_display: {e}"))?;
        Ok(())
    }

    pub fn get_session_display(&self, session_id: &str) -> Result<Option<SessionDisplay>, String> {
        let connection = match self.connection.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        connection
            .query_row(
                "SELECT title, last_turn_preview FROM session_display WHERE session_id = ?1",
                params![session_id],
                |row| {
                    Ok(SessionDisplay {
                        title: row.get(0)?,
                        last_turn_preview: row.get(1)?,
                    })
                },
            )
            .optional()
            .map_err(|e| format!("get session_display: {e}"))
    }

    pub fn list_session_display(&self) -> Result<HashMap<String, SessionDisplay>, String> {
        let connection = match self.connection.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let mut stmt = connection
            .prepare("SELECT session_id, title, last_turn_preview FROM session_display")
            .map_err(|e| format!("prepare list session_display: {e}"))?;
        let rows = stmt
            .query_map([], |row| {
                let id: String = row.get(0)?;
                Ok((
                    id,
                    SessionDisplay {
                        title: row.get(1)?,
                        last_turn_preview: row.get(2)?,
                    },
                ))
            })
            .map_err(|e| format!("query list session_display: {e}"))?;
        let mut out = HashMap::new();
        for row in rows {
            let (id, display) = row.map_err(|e| format!("row session_display: {e}"))?;
            out.insert(id, display);
        }
        Ok(out)
    }

    pub fn delete_session_display(&self, session_id: &str) -> Result<(), String> {
        let connection = match self.connection.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        connection
            .execute(
                "DELETE FROM session_display WHERE session_id = ?1",
                params![session_id],
            )
            .map_err(|e| format!("delete session_display: {e}"))?;
        Ok(())
    }

    pub fn set_project_display(
        &self,
        project_id: &str,
        display: &ProjectDisplay,
    ) -> Result<(), String> {
        let connection = match self.connection.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let now = chrono::Utc::now().to_rfc3339();
        connection
            .execute(
                "INSERT INTO project_display
                    (project_id, name, sort_order, updated_at)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(project_id) DO UPDATE SET
                    name = excluded.name,
                    sort_order = excluded.sort_order,
                    updated_at = excluded.updated_at",
                params![project_id, display.name, display.sort_order, now],
            )
            .map_err(|e| format!("set project_display: {e}"))?;
        Ok(())
    }

    pub fn get_project_display(&self, project_id: &str) -> Result<Option<ProjectDisplay>, String> {
        let connection = match self.connection.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        connection
            .query_row(
                "SELECT name, sort_order FROM project_display WHERE project_id = ?1",
                params![project_id],
                |row| {
                    Ok(ProjectDisplay {
                        name: row.get(0)?,
                        sort_order: row.get(1)?,
                    })
                },
            )
            .optional()
            .map_err(|e| format!("get project_display: {e}"))
    }

    pub fn list_project_display(&self) -> Result<HashMap<String, ProjectDisplay>, String> {
        let connection = match self.connection.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let mut stmt = connection
            .prepare("SELECT project_id, name, sort_order FROM project_display")
            .map_err(|e| format!("prepare list project_display: {e}"))?;
        let rows = stmt
            .query_map([], |row| {
                let id: String = row.get(0)?;
                Ok((
                    id,
                    ProjectDisplay {
                        name: row.get(1)?,
                        sort_order: row.get(2)?,
                    },
                ))
            })
            .map_err(|e| format!("query list project_display: {e}"))?;
        let mut out = HashMap::new();
        for row in rows {
            let (id, display) = row.map_err(|e| format!("row project_display: {e}"))?;
            out.insert(id, display);
        }
        Ok(out)
    }

    pub fn delete_project_display(&self, project_id: &str) -> Result<(), String> {
        let connection = match self.connection.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        connection
            .execute(
                "DELETE FROM project_display WHERE project_id = ?1",
                params![project_id],
            )
            .map_err(|e| format!("delete project_display: {e}"))?;
        Ok(())
    }

    pub fn set_project_worktree(
        &self,
        project_id: &str,
        parent_project_id: &str,
        branch: Option<&str>,
    ) -> Result<(), String> {
        let connection = match self.connection.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let now = chrono::Utc::now().to_rfc3339();
        connection
            .execute(
                "INSERT INTO project_worktree
                    (project_id, parent_project_id, branch, updated_at)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(project_id) DO UPDATE SET
                    parent_project_id = excluded.parent_project_id,
                    branch = excluded.branch,
                    updated_at = excluded.updated_at",
                params![project_id, parent_project_id, branch, now],
            )
            .map_err(|e| format!("set project_worktree: {e}"))?;
        Ok(())
    }

    pub fn get_project_worktree(
        &self,
        project_id: &str,
    ) -> Result<Option<ProjectWorktree>, String> {
        let connection = match self.connection.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        connection
            .query_row(
                "SELECT project_id, parent_project_id, branch
                 FROM project_worktree WHERE project_id = ?1",
                params![project_id],
                |row| {
                    Ok(ProjectWorktree {
                        project_id: row.get(0)?,
                        parent_project_id: row.get(1)?,
                        branch: row.get(2)?,
                    })
                },
            )
            .optional()
            .map_err(|e| format!("get project_worktree: {e}"))
    }

    pub fn list_project_worktree(&self) -> Result<HashMap<String, ProjectWorktree>, String> {
        let connection = match self.connection.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let mut stmt = connection
            .prepare("SELECT project_id, parent_project_id, branch FROM project_worktree")
            .map_err(|e| format!("prepare list project_worktree: {e}"))?;
        let rows = stmt
            .query_map([], |row| {
                let project_id: String = row.get(0)?;
                Ok((
                    project_id.clone(),
                    ProjectWorktree {
                        project_id,
                        parent_project_id: row.get(1)?,
                        branch: row.get(2)?,
                    },
                ))
            })
            .map_err(|e| format!("query list project_worktree: {e}"))?;
        let mut out = HashMap::new();
        for row in rows {
            let (id, rec) = row.map_err(|e| format!("row project_worktree: {e}"))?;
            out.insert(id, rec);
        }
        Ok(out)
    }

    pub fn delete_project_worktree(&self, project_id: &str) -> Result<(), String> {
        let connection = match self.connection.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        connection
            .execute(
                "DELETE FROM project_worktree WHERE project_id = ?1",
                params![project_id],
            )
            .map_err(|e| format!("delete project_worktree: {e}"))?;
        Ok(())
    }
}
