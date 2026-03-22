// Flowzen-app-owned key/value store for user-tunable settings.
//
// Lives in its own SQLite file under Tauri's app_data_dir,
// deliberately separate from the agent SDK's persistence layer:
// the SDK owns session/agent state, the app owns app-level
// configuration. There is no schema sharing, no shared
// connection, no overlap. Adding a new app-level setting means
// editing this file (or just calling `get` / `set` with a new
// key); it never touches the SDK.
//
// Schema is intentionally minimal: a single `user_config` table
// with `(key TEXT PRIMARY KEY, value TEXT NOT NULL, updated_at
// TEXT NOT NULL)`. Values are stored as strings — callers
// serialise/parse on their own. Good enough for the small
// number of toggles a desktop app needs without designing a
// full settings system up front.

use std::path::Path;
use std::sync::Mutex;

use rusqlite::{params, Connection, OptionalExtension};

/// Owned by Tauri state. The connection is wrapped in a Mutex
/// because rusqlite's Connection is not Sync; the lock is held
/// for the duration of a single read/write which is fine — these
/// commands are off the UI thread and the queries are
/// microsecond-level on local SQLite.
pub struct UserConfigStore {
    connection: Mutex<Connection>,
}

impl UserConfigStore {
    /// Open (or create) the SQLite file at `<data_dir>/user_config.sqlite`
    /// and ensure the schema exists. Called once during Tauri
    /// `setup`. Failing here is fatal — there's nothing useful the
    /// app can do without its config store.
    ///
    /// `data_dir` should be `~/.flowzen` so the file sits next to
    /// the daemon's database in its own dedicated file. SDK and
    /// app each own their own SQLite; this module never touches
    /// the daemon's schema or connection.
    pub fn open(data_dir: &Path) -> Result<Self, String> {
        if let Err(e) = std::fs::create_dir_all(data_dir) {
            return Err(format!("create data dir: {e}"));
        }
        let db_path = data_dir.join("user_config.sqlite");
        let connection = Connection::open(&db_path)
            .map_err(|e| format!("open user_config sqlite: {e}"))?;
        connection
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS user_config (
                    key TEXT PRIMARY KEY,
                    value TEXT NOT NULL,
                    updated_at TEXT NOT NULL
                );",
            )
            .map_err(|e| format!("create user_config schema: {e}"))?;
        Ok(Self {
            connection: Mutex::new(connection),
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
}
