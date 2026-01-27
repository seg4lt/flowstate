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
use std::sync::Arc;

use anyhow::{Context, Result};
use tracing_subscriber::EnvFilter;
use zenui_http_api::{LocalServer, spawn_local_server};
use zenui_orchestration::OrchestrationService;
use zenui_persistence::PersistenceService;
use zenui_provider_api::{ProviderAdapter, RuntimeEvent};
use zenui_provider_claude_cli::ClaudeCliAdapter;
use zenui_provider_claude_sdk::ClaudeSdkAdapter;
use zenui_provider_codex::CodexAdapter;
use zenui_provider_github_copilot::GitHubCopilotAdapter;
use zenui_provider_github_copilot_cli::GitHubCopilotCliAdapter;
use zenui_runtime_core::RuntimeCore;

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
/// This is still the entry point used by the tao-web-shell binary in
/// Phase 1 (no behavior change). In Phase 3, the shell switches to
/// `daemon-client` and this is called only by `zenui-server`.
pub fn bootstrap(bind_addr: SocketAddr, database_name: &str) -> Result<BootstrappedApp> {
    init_tracing();

    let working_directory =
        std::env::current_dir().context("failed to resolve working directory")?;
    let database_path = working_directory.join(".zenui").join(database_name);
    let frontend_dist = working_directory.join("frontend").join("dist");

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
    let runtime_core = Arc::new(RuntimeCore::new(adapters, orchestration, persistence, None));

    // Reclaim any sessions stuck at `Running` from a prior crash before we
    // begin serving clients.
    tokio_runtime.block_on(runtime_core.reconcile_startup());

    let server = spawn_local_server(
        &tokio_runtime,
        runtime_core.clone(),
        frontend_dist,
        bind_addr,
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

/// Block the current thread running the daemon. Bootstraps the runtime,
/// writes the ready file, installs a SIGINT / ctrl-c handler, waits for
/// an explicit shutdown request, runs the graceful-shutdown sequence,
/// and deletes the ready file.
///
/// Phase 1: signal handler + explicit shutdown wired. The idle watchdog
/// is defined in `lifecycle` but not yet spawned from here — Phase 2
/// wires `DaemonLifecycle` through `http-api` and `runtime-core` so the
/// counters are meaningful, then starts the watchdog.
pub fn run_blocking(config: DaemonConfig) -> Result<()> {
    let BootstrappedApp {
        tokio_runtime,
        runtime_core,
        server,
    } = bootstrap(config.bind_addr, &config.database_name)?;

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

    let lifecycle = DaemonLifecycle::new(config.idle_timeout);
    let shutdown_grace = config.shutdown_grace;

    let shutdown_result: Result<()> = {
        let lifecycle = lifecycle.clone();
        let runtime_core = runtime_core.clone();
        tokio_runtime.block_on(async move {
            tokio::select! {
                _ = lifecycle.wait_for_shutdown() => {
                    tracing::info!("explicit shutdown signal received");
                }
                _ = tokio::signal::ctrl_c() => {
                    tracing::info!("SIGINT received, initiating graceful shutdown");
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
