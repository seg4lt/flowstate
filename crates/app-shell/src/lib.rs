use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use tracing_subscriber::EnvFilter;
use zenui_http_api::{spawn_local_server, LocalServer};
use zenui_orchestration::OrchestrationService;
use zenui_persistence::PersistenceService;
use zenui_provider_api::{ProviderAdapter, RuntimeEvent};
use zenui_provider_claude_sdk::ClaudeSdkAdapter;
use zenui_provider_codex::CodexAdapter;
use zenui_provider_github_copilot::GitHubCopilotAdapter;
use zenui_runtime_core::RuntimeCore;

pub struct BootstrappedApp {
    pub tokio_runtime: tokio::runtime::Runtime,
    pub runtime_core: Arc<RuntimeCore>,
    pub server: LocalServer,
}

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
    ];
    let orchestration = Arc::new(OrchestrationService::new());
    let persistence = Arc::new(
        PersistenceService::new(database_path)
            .context("failed to initialize sqlite persistence")?,
    );
    let runtime_core = Arc::new(RuntimeCore::new(adapters, orchestration, persistence));
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
