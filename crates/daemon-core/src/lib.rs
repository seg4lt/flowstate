//! ZenUI daemon core.
//!
//! Owns the runtime bootstrap, the lifecycle state (counters + idle
//! watchdog), the ready file coordination, and the graceful shutdown
//! sequence. It deliberately has no transport code of its own — the
//! HTTP + WebSocket surface lives in `zenui-http-api` and daemon-core
//! just spins it up as one of its sidecar tasks.
//!
//! Phase 1: extraction from `app-shell`. `bootstrap()` is moved verbatim.
//! `run_blocking()` is scaffolded against the new module set but is not
//! yet wired for idle auto-shutdown — that's Phase 2.

mod config;
mod lifecycle;
mod ready_file;
mod shutdown;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use tracing_subscriber::EnvFilter;
use zenui_http_api::{ConnectionObserver, LocalServer, spawn_local_server};
use zenui_orchestration::OrchestrationService;
use zenui_persistence::PersistenceService;
use zenui_provider_api::{ProviderAdapter, RuntimeEvent};
use zenui_provider_claude_cli::ClaudeCliAdapter;
use zenui_provider_claude_sdk::ClaudeSdkAdapter;
use zenui_provider_codex::CodexAdapter;
use zenui_provider_github_copilot::GitHubCopilotAdapter;
use zenui_provider_github_copilot_cli::GitHubCopilotCliAdapter;
use zenui_runtime_core::{RuntimeCore, TurnLifecycleObserver};

pub use config::DaemonConfig;
pub use lifecycle::{DaemonLifecycle, IdleShutdownReason, idle_watchdog};
pub use ready_file::{ReadyFile, ReadyFileContent};
pub use shutdown::graceful_shutdown;

pub struct BootstrappedApp {
    pub tokio_runtime: tokio::runtime::Runtime,
    pub runtime_core: Arc<RuntimeCore>,
    pub server: LocalServer,
}

/// Single-process bootstrap. Creates the tokio runtime, wires providers,
/// persistence, orchestration, and `RuntimeCore`, reconciles any stuck
/// sessions, and starts the local HTTP/WS server.
///
/// `lifecycle` is `None` for callers that just want a local process (the
/// tao-web-shell in Phase 2) and `Some(Arc<DaemonLifecycle>)` for the
/// daemon binary. When `Some`, counters fire on every client connect and
/// every turn start/end, driving `idle_watchdog`.
pub fn bootstrap(
    bind_addr: SocketAddr,
    database_name: &str,
    project_root: Option<&Path>,
    frontend_dist: Option<PathBuf>,
    lifecycle: Option<Arc<DaemonLifecycle>>,
) -> Result<BootstrappedApp> {
    init_tracing();

    let working_directory = match project_root {
        Some(root) => root.to_path_buf(),
        None => std::env::current_dir().context("failed to resolve working directory")?,
    };
    let database_path = working_directory.join(".zenui").join(database_name);
    let frontend_dist =
        frontend_dist.unwrap_or_else(|| working_directory.join("frontend").join("dist"));

    let tokio_runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("zenui-runtime")
        .build()
        .context("failed to build tokio runtime")?;

    let adapters: Vec<Arc<dyn ProviderAdapter>> = vec![
        Arc::new(CodexAdapter::new(working_directory.clone())),
        Arc::new(ClaudeSdkAdapter::new(working_directory.clone())),
        Arc::new(GitHubCopilotAdapter::new(working_directory.clone())),
        Arc::new(GitHubCopilotCliAdapter::new(working_directory.clone())),
        Arc::new(ClaudeCliAdapter::new(working_directory.clone())),
    ];
    let orchestration = Arc::new(OrchestrationService::new());
    let persistence = Arc::new(
        PersistenceService::new(database_path)
            .context("failed to initialize sqlite persistence")?,
    );

    let turn_observer: Option<Arc<dyn TurnLifecycleObserver>> = lifecycle
        .as_ref()
        .map(|l| -> Arc<dyn TurnLifecycleObserver> { l.clone() });
    let connection_observer: Option<Arc<dyn ConnectionObserver>> = lifecycle
        .as_ref()
        .map(|l| -> Arc<dyn ConnectionObserver> { l.clone() });

    let runtime_core = Arc::new(RuntimeCore::new(
        adapters,
        orchestration,
        persistence,
        turn_observer,
    ));

    // Reclaim any sessions stuck at `Running` from a prior crash before we
    // begin serving clients.
    tokio_runtime.block_on(runtime_core.reconcile_startup());

    let server = spawn_local_server(
        &tokio_runtime,
        runtime_core.clone(),
        frontend_dist,
        bind_addr,
        connection_observer,
    )
    .context("failed to launch local server")?;

    runtime_core.publish(RuntimeEvent::RuntimeReady {
        message: format!("Local server listening on {}", server.frontend_url()),
    });

    Ok(BootstrappedApp {
        tokio_runtime,
        runtime_core,
        server,
    })
}

pub fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| "zenui=debug,warn".into()),
        )
        .try_init();
}

/// Block the current thread running the daemon. Bootstraps the runtime
/// *with* a real `DaemonLifecycle` attached, writes the ready file, starts
/// the idle watchdog, installs the SIGINT/ctrl-c handler, then waits for
/// either the watchdog to fire or an explicit shutdown request. On wake
/// it runs graceful shutdown, deletes the ready file, and returns.
pub fn run_blocking(config: DaemonConfig) -> Result<()> {
    let lifecycle = DaemonLifecycle::new(config.idle_timeout);

    let BootstrappedApp {
        tokio_runtime,
        runtime_core,
        server,
    } = bootstrap(
        config.bind_addr,
        &config.database_name,
        Some(&config.project_root),
        config.frontend_dist.clone(),
        Some(lifecycle.clone()),
    )?;

    let ready = ReadyFile::for_project(&config.project_root)
        .context("resolve daemon ready file")?;
    let http_base = server.frontend_url();
    let ws_url = format!("{}/ws", http_base.replacen("http://", "ws://", 1));
    let content = ReadyFileContent::new(
        http_base,
        ws_url,
        config.project_root.to_string_lossy().into_owned(),
    );
    ready.write_atomic(&content).context("write ready file")?;

    let shutdown_grace = config.shutdown_grace;

    let shutdown_result: Result<()> = {
        let lifecycle = lifecycle.clone();
        let runtime_core = runtime_core.clone();
        tokio_runtime.block_on(async move {
            let (watchdog_tx, watchdog_rx) = tokio::sync::oneshot::channel();
            tokio::spawn(idle_watchdog(lifecycle.clone(), watchdog_tx));

            tokio::select! {
                reason = watchdog_rx => {
                    match reason {
                        Ok(IdleShutdownReason::Idle) => {
                            tracing::info!("daemon idle timeout reached, shutting down");
                        }
                        Ok(IdleShutdownReason::Explicit) => {
                            tracing::info!("explicit shutdown request received, shutting down");
                        }
                        Err(_) => {
                            tracing::warn!("idle watchdog channel closed unexpectedly");
                        }
                    }
                }
                _ = tokio::signal::ctrl_c() => {
                    tracing::info!("SIGINT received, initiating graceful shutdown");
                    lifecycle.request_shutdown();
                }
            }
            graceful_shutdown(runtime_core, lifecycle, shutdown_grace).await?;
            drop(server);
            Ok(())
        })
    };

    // Explicit drop order: tokio_runtime after runtime_core so that any
    // still-pending task cleanup runs on a live runtime. `tokio_runtime`
    // goes out of scope at end-of-function, which reaps subprocesses.
    drop(runtime_core);
    drop(tokio_runtime);

    let _ = ready.delete();
    shutdown_result
}
