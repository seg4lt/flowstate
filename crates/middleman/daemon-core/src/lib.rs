//! ZenUI daemon core.
//!
//! Owns the runtime bootstrap, lifecycle state (counters + idle
//! watchdog), ready file coordination, and the graceful shutdown
//! sequence. **Transport-agnostic**: does not depend on any transport
//! crate. The app binary composes a `Vec<Box<dyn Transport>>` (HTTP,
//! Unix socket, wry IPC, anything that implements the trait) and hands
//! it to `run_blocking`, which drives the shared lifecycle across all
//! of them.
//!
//! # Entry points
//!
//! - [`bootstrap_core`] — builds the tokio runtime, providers, SQLite,
//!   `RuntimeCore`, and `DaemonLifecycle`. Reconciles stuck sessions.
//!   Does NOT start any transport. Use this when you want the runtime
//!   in-process and will wire your own transport.
//! - [`run_blocking`] — the daemon-binary entry point. Calls
//!   `bootstrap_core`, invokes `bind()` then `serve()` on each provided
//!   transport, writes the ready file after every transport is live,
//!   spawns the idle watchdog, waits for shutdown, runs graceful
//!   shutdown, deletes the ready file.

mod config;
mod lifecycle;
mod ready_file;
mod shutdown;

use std::sync::Arc;

use anyhow::{Context, Result};
use tracing_subscriber::EnvFilter;
use zenui_orchestration::OrchestrationService;
use zenui_persistence::PersistenceService;
use zenui_provider_api::{ProviderAdapter, RuntimeEvent};
#[cfg(any(
    feature = "provider-codex",
    feature = "provider-claude-sdk",
    feature = "provider-claude-cli",
    feature = "provider-github-copilot",
    feature = "provider-github-copilot-cli",
))]
use zenui_provider_api::ProviderKind;
#[cfg(feature = "provider-claude-cli")]
use zenui_provider_claude_cli::ClaudeCliAdapter;
#[cfg(feature = "provider-claude-sdk")]
use zenui_provider_claude_sdk::ClaudeSdkAdapter;
#[cfg(feature = "provider-codex")]
use zenui_provider_codex::CodexAdapter;
#[cfg(feature = "provider-github-copilot")]
use zenui_provider_github_copilot::GitHubCopilotAdapter;
#[cfg(feature = "provider-github-copilot-cli")]
use zenui_provider_github_copilot_cli::GitHubCopilotCliAdapter;
use zenui_runtime_core::{ConnectionObserver, RuntimeCore, TurnLifecycleObserver};

pub use config::DaemonConfig;
pub use lifecycle::{DaemonLifecycle, IdleShutdownReason, idle_watchdog};
pub use ready_file::{ReadyFile, ReadyFileContent};
pub use shutdown::graceful_shutdown;
// Transport traits now live in `runtime-core` so transport crates can
// depend on them without pulling in daemon-core (which would create a
// cycle when daemon-core takes optional deps on concrete transports).
// Re-exported here for API stability.
pub use zenui_runtime_core::transport::{Bound, Transport, TransportAddressInfo, TransportHandle};

// Concrete transport crates, re-exported under stable module aliases.
// Consumers opt in via the matching Cargo feature (`transport-tauri`,
// `transport-http`, or the `all-transports` meta feature) and then
// import via `zenui_daemon_core::transport_tauri::…` etc. — same
// centralised entry-point pattern as the provider crates above.
#[cfg(feature = "transport-tauri")]
pub use zenui_transport_tauri as transport_tauri;
#[cfg(feature = "transport-http")]
pub use zenui_transport_http as transport_http;

/// Headless runtime handle returned by [`bootstrap_core`]. Owns the tokio
/// runtime, the `RuntimeCore`, and the `DaemonLifecycle`. Callers use
/// `run_blocking` to drive the full daemon lifecycle with transports, or
/// work with these fields directly for in-process embedding.
pub struct BootstrappedCore {
    pub tokio_runtime: tokio::runtime::Runtime,
    pub runtime_core: Arc<RuntimeCore>,
    pub lifecycle: Arc<DaemonLifecycle>,
}

/// Transport-free bootstrap. Builds the tokio runtime, wires every
/// provider adapter, opens SQLite, constructs `RuntimeCore` with a
/// `DaemonLifecycle` as the `TurnLifecycleObserver`, and reconciles any
/// sessions stuck at `Running` from a prior crash. Does **not** start
/// any transport — the caller (usually `run_blocking`) is responsible
/// for that.
pub fn bootstrap_core(config: &DaemonConfig) -> Result<BootstrappedCore> {
    init_tracing();

    let working_directory = config.project_root.clone();
    let database_path = working_directory.join(".zenui").join(&config.database_name);

    let tokio_runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("zenui-runtime")
        .build()
        .context("failed to build tokio runtime")?;

    let lifecycle = DaemonLifecycle::new(config.idle_timeout);

    // Provider adapter construction. Delegated to `build_adapters`
    // which is feature-gated — when no provider features are enabled it
    // collapses to an empty-vec stub that doesn't even reference the
    // ProviderKind enum. That keeps the zero-provider build warning-free
    // for embedders who want to bring their own adapters or test the
    // transport layer in isolation.
    let adapters = build_adapters(&working_directory, &config.enabled_providers);
    let orchestration = Arc::new(OrchestrationService::new());
    let persistence = Arc::new(
        PersistenceService::new(database_path)
            .context("failed to initialize sqlite persistence")?,
    );

    let threads_dir = working_directory
        .join("threads")
        .to_string_lossy()
        .into_owned();
    let turn_observer: Arc<dyn TurnLifecycleObserver> = lifecycle.clone();
    let runtime_core = Arc::new(RuntimeCore::new(
        adapters,
        orchestration,
        persistence,
        Some(turn_observer),
        threads_dir,
    ));

    // Reclaim any sessions stuck at `Running` from a prior crash, and
    // seed the provider enablement map from persistence, before we
    // serve any clients. Both are single-shot async reads over the
    // already-open SQLite handle — cheap and idempotent.
    tokio_runtime.block_on(async {
        runtime_core.reconcile_startup().await;
        runtime_core.seed_provider_enablement().await;
    });

    Ok(BootstrappedCore {
        tokio_runtime,
        runtime_core,
        lifecycle,
    })
}

/// Walk `config.enabled_providers` and instantiate the subset that has
/// been compiled in. Each match arm is gated on its Cargo feature, so a
/// disabled provider's adapter code is stripped from the binary
/// entirely. Variants requested at runtime but not compiled in fall
/// through to the catch-all and log a warning.
#[cfg(any(
    feature = "provider-codex",
    feature = "provider-claude-sdk",
    feature = "provider-claude-cli",
    feature = "provider-github-copilot",
    feature = "provider-github-copilot-cli",
))]
fn build_adapters(
    working_directory: &std::path::PathBuf,
    enabled: &[zenui_provider_api::ProviderKind],
) -> Vec<Arc<dyn ProviderAdapter>> {
    let mut adapters: Vec<Arc<dyn ProviderAdapter>> = Vec::new();
    for &kind in enabled {
        let adapter: Arc<dyn ProviderAdapter> = match kind {
            #[cfg(feature = "provider-codex")]
            ProviderKind::Codex => Arc::new(CodexAdapter::new(working_directory.clone())),
            #[cfg(feature = "provider-claude-sdk")]
            ProviderKind::Claude => Arc::new(ClaudeSdkAdapter::new(working_directory.clone())),
            #[cfg(feature = "provider-github-copilot")]
            ProviderKind::GitHubCopilot => {
                Arc::new(GitHubCopilotAdapter::new(working_directory.clone()))
            }
            #[cfg(feature = "provider-claude-cli")]
            ProviderKind::ClaudeCli => Arc::new(ClaudeCliAdapter::new(working_directory.clone())),
            #[cfg(feature = "provider-github-copilot-cli")]
            ProviderKind::GitHubCopilotCli => {
                Arc::new(GitHubCopilotCliAdapter::new(working_directory.clone()))
            }
            #[allow(unreachable_patterns)]
            kind => {
                tracing::warn!(
                    ?kind,
                    "provider requested in enabled_providers but disabled at compile time; skipping"
                );
                continue;
            }
        };
        adapters.push(adapter);
    }
    adapters
}

/// Zero-provider fallback. Used when the crate is built with no
/// `provider-*` features — typically by integration-test harnesses or
/// embedders that inject their own adapters via a custom bootstrap.
#[cfg(not(any(
    feature = "provider-codex",
    feature = "provider-claude-sdk",
    feature = "provider-claude-cli",
    feature = "provider-github-copilot",
    feature = "provider-github-copilot-cli",
)))]
fn build_adapters(
    _working_directory: &std::path::PathBuf,
    enabled: &[zenui_provider_api::ProviderKind],
) -> Vec<Arc<dyn ProviderAdapter>> {
    if !enabled.is_empty() {
        tracing::warn!(
            provider_count = enabled.len(),
            "daemon-core built without any provider features; ignoring enabled_providers"
        );
    }
    Vec::new()
}

pub fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| "zenui=debug,warn".into()),
        )
        .try_init();
}

/// Full daemon lifecycle with a caller-supplied set of transports.
///
/// Sequence:
///   1. `bootstrap_core` — runtime + providers + lifecycle + reconcile.
///   2. For each transport, call `bind()` on the host thread. Any error
///      aborts startup.
///   3. Enter `tokio_runtime.block_on` and call `serve()` on each Bound,
///      collecting `Box<dyn TransportHandle>`s. On error, already-started
///      handles are drained via their `shutdown()` before bubbling up.
///   4. Write the ready file v2 listing every transport's address. The
///      write happens **after** every transport is serving, so clients
///      polling the file never see a "ready file exists but port isn't
///      accepting yet" race.
///   5. Spawn `idle_watchdog`; install SIGINT handler; wait for shutdown.
///   6. On shutdown: publish `DaemonShuttingDown`, sweep in-flight turns,
///      drain every transport via `shutdown().await`, delete ready file,
///      drop runtime.
///
/// Zero-transport daemons are allowed. In that case the idle watchdog
/// fires immediately unless `config.idle_timeout == Duration::MAX`
/// (which `DaemonConfig::zero_transport` sets automatically).
pub fn run_blocking(config: DaemonConfig, transports: Vec<Box<dyn Transport>>) -> Result<()> {
    let BootstrappedCore {
        tokio_runtime,
        runtime_core,
        lifecycle,
    } = bootstrap_core(&config)?;

    // Phase 1: sync bind on host thread. Errors abort startup.
    let mut bound_transports: Vec<Box<dyn Bound>> = Vec::with_capacity(transports.len());
    for t in transports {
        let kind = t.kind();
        let b = t
            .bind()
            .with_context(|| format!("transport '{kind}' failed to bind"))?;
        bound_transports.push(b);
    }

    let ready = ReadyFile::for_project(&config.project_root)
        .context("resolve daemon ready file")?;
    let shutdown_grace = config.shutdown_grace;

    let result: Result<()> = {
        let lifecycle_inner = lifecycle.clone();
        let runtime_inner = runtime_core.clone();
        let ready_inner = ready.clone();
        let project_root_str = config.project_root.to_string_lossy().into_owned();

        tokio_runtime.block_on(async move {
            let observer: Arc<dyn ConnectionObserver> = lifecycle_inner.clone();

            // Phase 2: serve each bound transport. On error, drain the
            // already-started transports in reverse order.
            let mut handles: Vec<Box<dyn TransportHandle>> =
                Vec::with_capacity(bound_transports.len());
            for b in bound_transports {
                let kind = b.kind();
                match b.serve(runtime_inner.clone(), observer.clone()) {
                    Ok(h) => handles.push(h),
                    Err(e) => {
                        for h in handles.into_iter().rev() {
                            h.shutdown().await;
                        }
                        return Err(e).with_context(|| {
                            format!("transport '{kind}' failed to start serving")
                        });
                    }
                }
            }

            // Phase 3: write the ready file AFTER every transport is
            // serving. Invariant: ready file exists ⟹ every listed
            // transport is accepting connections.
            let address_infos: Vec<TransportAddressInfo> =
                handles.iter().map(|h| h.address_info()).collect();
            let ready_content =
                ReadyFileContent::new(project_root_str, address_infos.clone());
            if let Err(e) = ready_inner.write_atomic(&ready_content) {
                for h in handles.into_iter().rev() {
                    h.shutdown().await;
                }
                return Err(e).context("write ready file");
            }

            runtime_inner.publish(RuntimeEvent::RuntimeReady {
                message: format!(
                    "daemon ready with {} transport(s): {:?}",
                    handles.len(),
                    address_infos.iter().map(|a| a.kind()).collect::<Vec<_>>()
                ),
            });

            // Phase 4: watchdog + signal + shutdown.
            let (watchdog_tx, watchdog_rx) = tokio::sync::oneshot::channel();
            tokio::spawn(idle_watchdog(lifecycle_inner.clone(), watchdog_tx));

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
                    lifecycle_inner.request_shutdown();
                }
            }

            graceful_shutdown(runtime_inner, lifecycle_inner, shutdown_grace).await?;

            // Drain transports in reverse order of start.
            for h in handles.into_iter().rev() {
                h.shutdown().await;
            }

            Ok::<_, anyhow::Error>(())
        })
    };

    // Explicit drop order: runtime_core before tokio_runtime so any
    // still-pending cleanup runs on a live runtime. tokio_runtime goes
    // out of scope at end-of-function, reaping subprocesses.
    drop(runtime_core);
    drop(tokio_runtime);

    let _ = ready.delete();
    result
}
