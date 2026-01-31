use std::path::PathBuf;
use std::time::Duration;

/// Transport-agnostic runtime configuration for the daemon. Any
/// transport-specific settings (bind address, socket path, frontend
/// dist, TLS certs, ...) live on the individual `Transport` structs
/// that the app constructs and passes to `run_blocking`.
#[derive(Debug, Clone)]
pub struct DaemonConfig {
    pub database_name: String,
    pub project_root: PathBuf,
    pub idle_timeout: Duration,
    pub shutdown_grace: Duration,
    pub log_file: Option<PathBuf>,
    pub detach: bool,
}

impl DaemonConfig {
    /// Sensible defaults for a project-rooted daemon with normal
    /// idle-shutdown behavior (60-second idle timeout, 5-second
    /// shutdown grace).
    pub fn with_project_root(project_root: PathBuf) -> Self {
        Self {
            database_name: "zenui.db".to_string(),
            project_root,
            idle_timeout: Duration::from_secs(60),
            shutdown_grace: Duration::from_secs(5),
            log_file: None,
            detach: false,
        }
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
        }
    }
}
