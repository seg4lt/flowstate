use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use zenui_provider_api::ProviderAdapter;

/// Transport-agnostic runtime configuration for the daemon. Any
/// transport-specific settings (bind address, socket path, frontend
/// dist, TLS certs, ...) live on the individual `Transport` structs
/// that the app constructs and passes to `run_blocking`.
///
/// **Adapter ownership:** `adapters` is empty by default. The
/// hosting app (e.g. `apps/flowstate/src-tauri`) is responsible for
/// constructing the `ProviderAdapter` instances it wants to expose
/// and pushing them into this field before calling
/// `bootstrap_core_async`. Keeping construction out of `daemon-core`
/// preserves the provider-agnostic layering — middleman bridges
/// lifecycle and transport concerns and never picks concrete
/// providers.
#[derive(Clone)]
pub struct DaemonConfig {
    pub database_name: String,
    pub project_root: PathBuf,
    pub idle_timeout: Duration,
    pub shutdown_grace: Duration,
    pub log_file: Option<PathBuf>,
    pub detach: bool,
    pub adapters: Vec<Arc<dyn ProviderAdapter>>,
    /// Name the runtime advertises in the Bootstrap wire payload. Lets
    /// multiple apps (flowstate, experimental shells, tests) embed the
    /// same runtime without leaking a hard-coded label into core.
    pub app_name: String,
    /// Explicit data-directory override for [`database_path`](Self::database_path).
    ///
    /// Phase 5.5.6 addition. Pre-split, the SQLite path was always
    /// `project_root.join(database_name)`. Post-split (Phase 6), the
    /// Tauri shell resolves the app's data dir from the OS and
    /// injects it into the daemon via `--data-dir <PATH>`; the daemon
    /// must NOT re-resolve the path because any resolver skew between
    /// shell and daemon would point them at different databases
    /// (stale-shell vs. new-daemon SQLite race, see plan's risk #1).
    ///
    /// When `Some(dir)`, `database_path()` returns `dir.join(database_name)`.
    /// When `None`, falls back to the pre-existing
    /// `project_root.join(database_name)` behaviour.
    pub explicit_data_dir: Option<PathBuf>,
}

impl DaemonConfig {
    /// Sensible defaults for a project-rooted daemon with normal
    /// idle-shutdown behavior (60-second idle timeout, 5-second
    /// shutdown grace). The caller must populate `adapters` before
    /// calling `bootstrap_core_async`.
    pub fn with_project_root(project_root: PathBuf) -> Self {
        Self {
            database_name: "zenui.db".to_string(),
            project_root,
            idle_timeout: Duration::from_secs(60),
            shutdown_grace: Duration::from_secs(5),
            log_file: None,
            detach: false,
            adapters: Vec::new(),
            app_name: "zenui".to_string(),
            explicit_data_dir: None,
        }
    }

    /// Builder-style chain: override the data directory.
    ///
    /// Intended for the Phase 6 daemon entry point — the Tauri shell
    /// resolves the app data dir once, writes it to the handshake
    /// file, passes it on the command line, and the daemon stores it
    /// here so every SQLite open + data-dir-derived path uses the
    /// same string the shell sees. Prevents the stale-shell /
    /// new-daemon race described in plan's risk #1.
    pub fn with_explicit_data_dir(mut self, data_dir: PathBuf) -> Self {
        self.explicit_data_dir = Some(data_dir);
        self
    }

    /// Canonical database path. Prefers `explicit_data_dir` over
    /// `project_root` — see the field docstring for why.
    pub fn database_path(&self) -> PathBuf {
        let dir = self
            .explicit_data_dir
            .as_deref()
            .unwrap_or_else(|| self.project_root.as_path());
        dir.join(&self.database_name)
    }

    /// Defaults for a daemon running with zero transports (e.g. an
    /// embedded GPUI shell that drives `RuntimeCore` in-process and
    /// wants the daemon lifecycle features without any wire protocol).
    ///
    /// Sets `idle_timeout = Duration::MAX` so the idle watchdog never
    /// fires on its own — the caller is expected to initiate shutdown
    /// explicitly via `DaemonLifecycle::request_shutdown`.
    pub fn zero_transport(project_root: PathBuf) -> Self {
        Self {
            database_name: "zenui.db".to_string(),
            project_root,
            idle_timeout: Duration::MAX,
            shutdown_grace: Duration::from_secs(5),
            log_file: None,
            detach: false,
            adapters: Vec::new(),
            app_name: "zenui".to_string(),
            explicit_data_dir: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn database_path_uses_project_root_by_default() {
        let cfg = DaemonConfig::with_project_root(PathBuf::from("/tmp/proj"));
        assert_eq!(cfg.database_path(), PathBuf::from("/tmp/proj/zenui.db"));
    }

    #[test]
    fn database_path_honours_explicit_data_dir() {
        let cfg = DaemonConfig::with_project_root(PathBuf::from("/tmp/proj"))
            .with_explicit_data_dir(PathBuf::from("/var/flowstate"));
        assert_eq!(
            cfg.database_path(),
            PathBuf::from("/var/flowstate/zenui.db")
        );
    }
}
