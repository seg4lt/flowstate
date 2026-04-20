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
//! - [`bootstrap_core_async`] — **preferred for library embedders**.
//!   Async; does not build a tokio runtime. Call from inside an
//!   existing `#[tokio::main]`, `tokio::test`, or task. Returns an
//!   [`InProcessCore`] that shares the caller's runtime. Use this when
//!   you want `RuntimeCore` as an in-process service inside a host app.
//! - [`bootstrap_core`] — sync wrapper over `bootstrap_core_async` that
//!   builds and owns its own multi-threaded tokio runtime. Returns a
//!   [`BootstrappedCore`]. Use from a plain sync `main()` that does not
//!   already have a runtime (typically the daemon binary path).
//!   ⚠️ Will panic with "Cannot start a runtime from within a runtime"
//!   if called from inside an existing tokio runtime — reach for
//!   `bootstrap_core_async` there instead.
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
use zenui_persistence::PersistenceService;
#[cfg(feature = "standalone-binary")]
use zenui_provider_api::RuntimeEvent;
#[cfg(feature = "standalone-binary")]
use zenui_runtime_core::ConnectionObserver;
use zenui_runtime_core::{OrchestrationService, RuntimeCore, TurnLifecycleObserver};

pub use config::DaemonConfig;
pub use lifecycle::{DaemonLifecycle, DaemonStatus, IdleShutdownReason, idle_watchdog};
pub use ready_file::{ReadyFile, ReadyFileContent};
pub use shutdown::graceful_shutdown;
// Transport traits now live in `runtime-core` so transport crates can
// depend on them without pulling in daemon-core (which would create a
// cycle when daemon-core takes optional deps on concrete transports).
// Re-exported here for API stability.
pub use zenui_runtime_core::transport::{Bound, Transport, TransportAddressInfo, TransportHandle};

// Concrete transport crate, re-exported under a stable module alias.
// Consumers opt in via the `transport-tauri` Cargo feature and then
// import via `zenui_daemon_core::transport_tauri::…`. The HTTP
// transport was dropped in Phase 6.1 of the architecture audit — no
// in-tree consumer used it. Re-add an optional dep + `pub use` here
// if a future binary needs it.
#[cfg(feature = "transport-tauri")]
pub use zenui_transport_tauri as transport_tauri;

/// Headless runtime handle returned by [`bootstrap_core`]. Owns the tokio
/// runtime, the `RuntimeCore`, and the `DaemonLifecycle`. Callers use
/// `run_blocking` to drive the full daemon lifecycle with transports, or
/// work with these fields directly for in-process embedding from a sync
/// `main()`.
///
/// Library embedders that already have a tokio runtime should prefer
/// [`bootstrap_core_async`] + [`InProcessCore`] instead, to avoid a
/// second runtime in the process.
#[cfg(feature = "standalone-binary")]
pub struct BootstrappedCore {
    pub tokio_runtime: tokio::runtime::Runtime,
    pub runtime_core: Arc<RuntimeCore>,
    pub lifecycle: Arc<DaemonLifecycle>,
}

/// Runtime-agnostic handle returned by [`bootstrap_core_async`]. Holds
/// just the `RuntimeCore` and `DaemonLifecycle` — no tokio runtime,
/// because the caller owns one already. Every subsequent `RuntimeCore`
/// call and every tokio task it spawns runs on the caller's runtime.
///
/// This is the preferred shape for library embedders (`tinybot`,
/// `zenui-desktop`, test harnesses) that want `RuntimeCore` as just
/// another in-process service rather than as a standalone daemon.
pub struct InProcessCore {
    pub runtime_core: Arc<RuntimeCore>,
    pub lifecycle: Arc<DaemonLifecycle>,
}

/// Sync, runtime-owning bootstrap.
///
/// # When to use this
///
/// Use from a plain sync `main()` that does **not** already have a
/// tokio runtime — typically the daemon-binary entry point. This
/// function builds its own multi-threaded tokio runtime, wires every
/// provider adapter, opens SQLite, constructs `RuntimeCore`, and
/// synchronously reconciles any sessions stuck at `Running` from a
/// prior crash. It returns a [`BootstrappedCore`] that **owns** the
/// runtime; drop it to shut everything down. [`run_blocking`] is the
/// canonical consumer.
///
/// ⚠️ Do not call this from inside an existing tokio runtime (e.g.
/// from a `#[tokio::main]` function or a `tokio::test`). The internal
/// `block_on` will panic with "Cannot start a runtime from within a
/// runtime". Use [`bootstrap_core_async`] instead for that case.
///
/// Does **not** start any transport — the caller (usually
/// `run_blocking`) is responsible for that.
#[cfg(feature = "standalone-binary")]
pub fn bootstrap_core(config: &DaemonConfig) -> Result<BootstrappedCore> {
    let tokio_runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("zenui-runtime")
        .build()
        .context("failed to build tokio runtime")?;

    let InProcessCore {
        runtime_core,
        lifecycle,
    } = tokio_runtime.block_on(bootstrap_core_async(config))?;

    Ok(BootstrappedCore {
        tokio_runtime,
        runtime_core,
        lifecycle,
    })
}

/// Async, runtime-agnostic bootstrap.
///
/// # When to use this
///
/// Use from an async context that **already has** a tokio runtime —
/// i.e. host applications embedding the SDK as a library. Typical
/// callers are inside `#[tokio::main]`, a `tokio::test`, or a task
/// spawned on an existing runtime. This function does **not** build a
/// runtime of its own: it awaits startup reconciliation on the
/// caller's runtime and returns an [`InProcessCore`] whose
/// `RuntimeCore` runs every subsequent task on that same runtime.
///
/// Prefer this over [`bootstrap_core`] whenever you can. Only reach
/// for `bootstrap_core` when you genuinely need the SDK to own a
/// dedicated runtime (e.g. a standalone daemon binary driven from a
/// sync `main`).
///
/// # What it does
///
/// Wires every enabled provider adapter, opens SQLite, constructs
/// `RuntimeCore` with `DaemonLifecycle` as the `TurnLifecycleObserver`,
/// reclaims any sessions stuck at `Running` from a prior crash, and
/// seeds the provider-enablement map from persistence. Does **not**
/// start any transport — embedders call `RuntimeCore` methods
/// directly.
pub async fn bootstrap_core_async(config: &DaemonConfig) -> Result<InProcessCore> {
    // Tracing initialisation is the *binary's* job, not the library's.
    // Host apps (flowstate's `tracing_setup::init_tracing`, standalone
    // daemon binaries, integration tests) opt in by calling
    // [`init_tracing`] themselves before this bootstrap runs — that
    // way one subscriber filter wins, not whichever call reached
    // `try_init()` first. Removed in Phase 6.5 of the architecture
    // audit.

    let working_directory = config.project_root.clone();
    let database_path = working_directory.join(".zenui").join(&config.database_name);

    let lifecycle = DaemonLifecycle::new(config.idle_timeout);

    // Provider adapters are owned by the hosting app — see
    // `DaemonConfig::adapters`. Middleman does not know which concrete
    // providers exist; it just forwards the vector the app constructed.
    let adapters = config.adapters.clone();
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

    // Checkpoint store — on-disk content-addressed snapshot backing
    // the `RewindFiles` / per-turn revert feature. Lives next to the
    // sqlite database under the same `.zenui` data dir; blobs and
    // manifests never touch the user's project tree. A failure here is
    // fatal because checkpoints are part of the runtime contract — if
    // we can't open the store we can't guarantee rewind semantics and
    // it's safer to refuse to start than to silently run without it.
    let checkpoints_dir = working_directory.join(".zenui").join("checkpoints");
    let checkpoints: Arc<dyn zenui_checkpoints::CheckpointStore> = Arc::new(
        zenui_checkpoints::FsCheckpointStore::open(checkpoints_dir, persistence.clone())
            .context("failed to open checkpoint store")?,
    );

    let runtime_core = Arc::new(RuntimeCore::new(
        adapters,
        orchestration,
        persistence,
        checkpoints,
        Some(turn_observer),
        threads_dir,
        config.app_name.clone(),
    ));
    // Install the weak self-reference the cross-session orchestration
    // dispatcher uses to spawn peer turns. Must happen before the
    // runtime serves any traffic; the RuntimeCall drain-loop branch
    // fail-closes if this step was skipped.
    runtime_core.install_self_ref();

    // Reclaim any sessions stuck at `Running` from a prior crash, and
    // seed the provider enablement map from persistence, before we
    // serve any clients. Both are single-shot async reads over the
    // already-open SQLite handle — cheap and idempotent.
    runtime_core.reconcile_startup().await;
    runtime_core.seed_provider_enablement().await;
    runtime_core.seed_checkpoint_enablement().await;

    Ok(InProcessCore {
        runtime_core,
        lifecycle,
    })
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
#[cfg(feature = "standalone-binary")]
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

    let ready =
        ReadyFile::for_project(&config.project_root).context("resolve daemon ready file")?;
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
            let ready_content = ReadyFileContent::new(project_root_str, address_infos.clone());
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    // Regression test for the runtime-in-runtime embedding bug.
    //
    // Before this refactor, `bootstrap_core` built its own tokio runtime
    // and called `block_on` on it, which made it unusable from inside an
    // existing runtime (e.g. a host app with `#[tokio::main]`). Any such
    // call panicked with "Cannot start a runtime from within a runtime".
    //
    // `bootstrap_core_async` is the embedding-friendly variant: it does
    // not construct a runtime and awaits startup reconciliation on the
    // caller's runtime. This test locks in that guarantee — if some
    // future change reintroduces a `block_on` or `Runtime::new` on the
    // hot path, this test will panic and fail.
    #[tokio::test(flavor = "multi_thread")]
    async fn bootstrap_core_async_embeds_in_existing_runtime() {
        let tmp = tempfile::tempdir().expect("create tempdir");
        let mut config = DaemonConfig::zero_transport(tmp.path().to_path_buf());
        // `config.adapters` defaults to empty; bootstrap should still
        // succeed (adapter construction is now an app-layer concern).
        config.idle_timeout = Duration::from_secs(5);

        let core = bootstrap_core_async(&config)
            .await
            .expect("bootstrap_core_async must succeed inside an existing runtime");

        // Touch both fields to prove the returned handle is usable.
        // `subscribe` creates a broadcast receiver; `request_shutdown`
        // is a cheap state mutation on the lifecycle counter.
        let _events = core.runtime_core.subscribe();
        core.lifecycle.request_shutdown();
    }
}
