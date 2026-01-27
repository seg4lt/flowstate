use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

/// Runtime configuration for the daemon. Phase 1 ships this as a scaffold;
/// Phase 2 wires it into `run_blocking` and the lifecycle counters.
#[derive(Debug, Clone)]
pub struct DaemonConfig {
    pub bind_addr: SocketAddr,
    pub database_name: String,
    pub project_root: PathBuf,
    pub idle_timeout: Duration,
    pub shutdown_grace: Duration,
    pub frontend_dist: Option<PathBuf>,
    pub log_file: Option<PathBuf>,
    pub detach: bool,
}

impl DaemonConfig {
    pub fn with_project_root(project_root: PathBuf) -> Self {
        Self {
            bind_addr: SocketAddr::from(([127, 0, 0, 1], 0)),
            database_name: "zenui.db".to_string(),
            project_root,
            idle_timeout: Duration::from_secs(60),
            shutdown_grace: Duration::from_secs(5),
            frontend_dist: None,
            log_file: None,
            detach: false,
        }
    }
}
