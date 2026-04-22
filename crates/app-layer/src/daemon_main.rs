//! Entry point for the standalone `flowstate daemon` subcommand.
//!
//! Phase 6 of the architecture plan. The Tauri shell used to run the
//! daemon inline via `bootstrap_core_async` inside its own setup
//! closure; this module extracts the "open SQLite, build adapters,
//! mount HTTP, wait for shutdown" pipeline into a single function
//! both the Tauri embedded mode and the `flowstate daemon` binary
//! can call. The Tauri shell spawns the binary as a child, reads the
//! handshake file to discover the base URL, and proxies every
//! `#[tauri::command]` over HTTP.
//!
//! # What lives here vs. what still lives in the Tauri crate
//!
//! - **Here (daemon concerns):** provider-adapter construction,
//!   `UserConfigStore` / `UsageStore` open, `DaemonConfig` build,
//!   `bootstrap_core_async`, `HttpTransport::with_extra_router`,
//!   handshake-file write, in-flight drain on shutdown.
//! - **Tauri crate (UI concerns):** window events, signal handlers
//!   for the shell process, app-handle plumbing, the 22
//!   `#[tauri::command]` wrappers, PTY, file / git pickers. Those
//!   stay in the Tauri binary; they never run inside the daemon.
//!
//! # Handshake
//!
//! On successful bring-up the daemon writes
//! `<data_dir>/daemon.handshake` with `{base_url, pid,
//! schema_version, build_sha}` at 0600 perms, then blocks. The
//! Tauri shell's supervisor reads the file, verifies the PID is
//! alive, compares `schema_version` against its own bundled
//! [`zenui_provider_api::SCHEMA_VERSION`], and either attaches
//! (matching) or spawns a fresh daemon (missing / stale / skewed).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::Serialize;
use zenui_daemon_core::{
    DaemonConfig, Transport, TransportHandle, bootstrap_core_async, drain_shutdown,
    graceful_shutdown,
};
use zenui_provider_api::{
    OrchestrationIpcHandle, OrchestrationIpcInfo, ProviderAdapter, RuntimeEvent, SCHEMA_VERSION,
};
use zenui_provider_claude_cli::ClaudeCliAdapter;
use zenui_provider_claude_sdk::ClaudeSdkAdapter;
use zenui_provider_codex::CodexAdapter;
use zenui_provider_github_copilot::GitHubCopilotAdapter;
use zenui_provider_github_copilot_cli::GitHubCopilotCliAdapter;
use zenui_provider_opencode::OpenCodeAdapter;
use zenui_transport_http::HttpTransport;

use crate::http::{AppLayerApiState, router as app_layer_router};
use crate::usage::UsageStore;
use crate::user_config::UserConfigStore;

/// Construct the standard 6-provider adapter vector flowstate ships.
///
/// `ipc_handle` is cloned into every adapter that supports cross-
/// provider orchestration. Claude SDK gets a plain constructor
/// because it registers orchestration in-process via
/// `createSdkMcpServer` (the IPC handle is only useful for stdio-
/// MCP providers that spawn `flowstate mcp-server` subprocesses).
///
/// Single definition here means the Tauri embedded mode and the
/// standalone daemon binary can't drift on the adapter set.
pub fn build_adapters(
    data_dir: PathBuf,
    ipc_handle: OrchestrationIpcHandle,
    user_config: Option<&UserConfigStore>,
) -> Vec<Arc<dyn ProviderAdapter>> {
    // Opencode alone needs per-provider config today: its shared
    // `opencode serve` supports idle-kill, with a TTL persisted in
    // `user_config`. Other adapters don't take tunables yet; when
    // they do, read them here and thread them through their
    // constructors the same way.
    let opencode_idle_ttl = user_config
        .map(UserConfigStore::opencode_idle_ttl)
        .unwrap_or_else(|| std::time::Duration::from_secs(600));

    vec![
        Arc::new(ClaudeSdkAdapter::new(data_dir.clone())) as Arc<dyn ProviderAdapter>,
        Arc::new(ClaudeCliAdapter::new_with_orchestration(
            data_dir.clone(),
            Some(ipc_handle.clone()),
        )),
        Arc::new(CodexAdapter::new_with_orchestration(
            data_dir.clone(),
            Some(ipc_handle.clone()),
        )),
        Arc::new(GitHubCopilotAdapter::new_with_orchestration(
            data_dir.clone(),
            Some(ipc_handle.clone()),
        )),
        Arc::new(GitHubCopilotCliAdapter::new_with_orchestration(
            data_dir.clone(),
            Some(ipc_handle.clone()),
        )),
        Arc::new(OpenCodeAdapter::new_with_orchestration_and_idle_ttl(
            data_dir,
            Some(ipc_handle),
            Some(opencode_idle_ttl),
        )),
    ]
}

/// Contents of `<data_dir>/daemon.handshake`. Kept identical to the
/// shape the Tauri loopback path already writes in
/// `apps/flowstate/src-tauri/src/loopback_http.rs::Handshake` so the
/// shell's parser works against either source.
#[derive(Debug, Serialize)]
struct Handshake {
    base_url: String,
    pid: u32,
    schema_version: u32,
    build_sha: &'static str,
}

fn write_handshake(path: &Path, base_url: &str) -> Result<()> {
    let hs = Handshake {
        base_url: base_url.to_string(),
        pid: std::process::id(),
        schema_version: SCHEMA_VERSION,
        build_sha: option_env!("FLOWSTATE_BUILD_SHA").unwrap_or("dev"),
    };
    let body = serde_json::to_string_pretty(&hs).context("serialize handshake")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).context("create handshake parent dir")?;
    }
    let tmp = path.with_extension("handshake.tmp");
    {
        use std::io::Write;
        let mut f = std::fs::File::create(&tmp).context("create handshake tmp")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            f.set_permissions(std::fs::Permissions::from_mode(0o600))
                .context("chmod 0600 handshake tmp")?;
        }
        f.write_all(body.as_bytes()).context("write handshake body")?;
        f.sync_all().context("fsync handshake tmp")?;
    }
    std::fs::rename(&tmp, path).context("rename handshake into place")?;
    Ok(())
}

/// Argument bundle for the daemon entry point. Callers (the
/// `flowstate daemon` subcommand) parse argv into this and hand it
/// off; the daemon never re-resolves its own data-dir or binds
/// anywhere other than loopback.
pub struct DaemonMainArgs {
    /// Per-user data directory resolved by the Tauri shell (or the
    /// CLI caller). The daemon reads / writes SQLite under here and
    /// places the handshake file at `<data_dir>/daemon.handshake`.
    pub data_dir: PathBuf,
    /// Idle-timeout. `None` ⇒ never idle out — the intended mode
    /// when Tauri shell is the supervisor (daemon exits on
    /// explicit shutdown only, not on last-client-disconnect).
    pub idle_timeout: Option<Duration>,
    /// Max wall-clock to drain in-flight turns when a shutdown is
    /// requested before escalating to interrupt. Default 30 s if
    /// `None` — generous enough for a typical turn to finish, short
    /// enough that a user forcibly restarting the app isn't waiting
    /// minutes.
    pub drain_grace: Option<Duration>,
}

/// Blocking entry point used by the argv dispatcher in the Tauri
/// binary's `main.rs`. Owns its own tokio runtime (single multi-
/// threaded) so the daemon subcommand is self-contained and doesn't
/// pollute the Tauri runtime with daemon tasks.
///
/// Blocks until one of:
/// - `SIGINT` / `SIGTERM` arrives → drain shutdown.
/// - `POST /api/shutdown` from a loopback client → drain shutdown.
/// - Idle watchdog fires (only when `idle_timeout` is set) →
///   interrupt shutdown.
pub fn run_blocking(args: DaemonMainArgs) -> Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init()
        .ok();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("build tokio runtime for flowstate daemon")?;
    runtime.block_on(run(args))
}

/// Async variant — runs the daemon on whatever runtime the caller
/// already has. Useful for integration tests that want to exercise
/// the daemon path without spawning a process. The Tauri embedded
/// mode does NOT call this path (it reuses the existing setup that
/// runs adapters inside the Tauri tokio runtime).
pub async fn run(args: DaemonMainArgs) -> Result<()> {
    tracing::info!(
        data_dir = %args.data_dir.display(),
        schema_version = SCHEMA_VERSION,
        "flowstate daemon starting"
    );

    // Open app-layer stores FIRST so SQLite open failures fail fast
    // before we bind ports or spawn adapters. UserConfigStore is
    // mandatory; UsageStore is best-effort (analytics degrades to
    // empty rather than blocking app-level IPC).
    let user_config = UserConfigStore::open(&args.data_dir)
        .map_err(|e| anyhow::anyhow!("open user_config store: {e}"))?;
    let usage_reader = match UsageStore::open(&args.data_dir) {
        Ok(s) => Some(Arc::new(s)),
        Err(e) => {
            tracing::warn!("usage store failed to open: {e}; /api/usage/* will 503");
            None
        }
    };
    // Separate writer connection for the analytics subscriber. The
    // app-layer `UsageStore` wraps `Mutex<Connection>`; a second
    // handle avoids writer-vs-reader lock contention when the
    // dashboard queries mid-write.
    let usage_writer: Option<UsageStore> = match UsageStore::open(&args.data_dir) {
        Ok(s) => Some(s),
        Err(e) => {
            tracing::warn!("usage writer failed to open: {e}; analytics disabled");
            None
        }
    };

    let ipc_handle = OrchestrationIpcHandle::new();

    // Config with explicit data-dir (Phase 5.5.6) — the daemon must
    // NOT re-resolve paths; the shell owns resolution and passes
    // the dir via --data-dir so the two processes agree.
    let mut config = DaemonConfig::with_project_root(args.data_dir.clone())
        .with_explicit_data_dir(args.data_dir.clone());
    config.idle_timeout = args.idle_timeout.unwrap_or(Duration::MAX);
    config.app_name = "Flowstate".to_string();
    config.adapters = build_adapters(
        args.data_dir.clone(),
        ipc_handle.clone(),
        Some(&user_config),
    );

    // Bootstrap runtime + persistence + lifecycle + reconcile.
    let core = bootstrap_core_async(&config)
        .await
        .context("daemon bootstrap failed")?;

    // Analytics subscriber — same shape as the Tauri embedded path,
    // just runs inside the daemon process now.
    if let Some(writer) = usage_writer {
        let mut rx = core.runtime_core.subscribe();
        tokio::spawn(async move {
            use tokio::sync::broadcast::error::RecvError;
            loop {
                match rx.recv().await {
                    Ok(RuntimeEvent::TurnCompleted { session, turn, .. }) => {
                        let event = crate::usage::UsageEvent::from_turn(&session, &turn);
                        if let Err(e) = writer.record_turn(&event) {
                            tracing::warn!("record turn usage failed: {e}");
                        }
                    }
                    Ok(_) => {}
                    Err(RecvError::Lagged(n)) => {
                        tracing::warn!("usage subscriber lagged by {n} events; continuing");
                    }
                    Err(RecvError::Closed) => break,
                }
            }
        });
    }

    // HTTP transport on loopback + app-layer router merged in.
    let bind_addr: std::net::SocketAddr = "127.0.0.1:0"
        .parse()
        .expect("static loopback addr parses");
    let extra_router = app_layer_router(AppLayerApiState {
        user_config,
        usage: usage_reader,
    });
    let transport: Box<dyn Transport> =
        Box::new(HttpTransport::new(bind_addr).with_extra_router(extra_router));
    let bound = transport.bind().context("bind loopback HTTP transport")?;
    let address = bound.address_info();
    let base_url = match &address {
        zenui_daemon_core::TransportAddressInfo::Http { http_base, .. } => http_base.clone(),
        other => anyhow::bail!("expected HTTP transport address info, got {other:?}"),
    };

    // Populate the IPC channel so adapters spawning MCP subprocesses
    // can find the loopback URL.
    let exe_path = std::env::current_exe().context("resolve current_exe for IPC handle")?;
    ipc_handle.publish(OrchestrationIpcInfo {
        base_url: base_url.clone(),
        executable_path: exe_path,
    });

    let observer: Arc<dyn zenui_runtime_core::ConnectionObserver> = core.lifecycle.clone();
    let handle: Box<dyn TransportHandle> = bound
        .serve(core.runtime_core.clone(), observer)
        .context("serve loopback HTTP transport")?;

    // Handshake file — this is what tells the Tauri shell the daemon
    // is up and what port to connect to. Writing AFTER serve()
    // returns means any supervisor that reads the file and
    // immediately hits the URL never races.
    let handshake_path = args.data_dir.join("daemon.handshake");
    write_handshake(&handshake_path, &base_url)
        .with_context(|| format!("write handshake at {}", handshake_path.display()))?;

    tracing::info!(
        base_url = %base_url,
        pid = std::process::id(),
        handshake = %handshake_path.display(),
        "flowstate daemon ready"
    );

    // Install SIGINT/SIGTERM handlers that flip to drain shutdown
    // (wait for turns to finish before killing adapters). SIGKILL
    // still can't be caught; the Tauri supervisor handles that via
    // respawn + orphan scan on next start.
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let lifecycle = core.lifecycle.clone();
        tokio::spawn(async move {
            let mut sigterm = signal(SignalKind::terminate()).expect("install SIGTERM");
            let mut sigint = signal(SignalKind::interrupt()).expect("install SIGINT");
            let sig = tokio::select! {
                _ = sigterm.recv() => "SIGTERM",
                _ = sigint.recv() => "SIGINT",
            };
            tracing::info!(signal = sig, "daemon received termination signal");
            lifecycle.request_shutdown();
        });
    }

    // Block until shutdown requested — then drain.
    core.lifecycle.wait_for_shutdown().await;

    let drain = args.drain_grace.unwrap_or(Duration::from_secs(30));
    let _ = drain_shutdown(
        core.runtime_core.clone(),
        core.lifecycle.clone(),
        drain,
        config.shutdown_grace,
    )
    .await;

    // Keep the interrupt path available for callers (e.g. idle
    // watchdog) that wanted the faster exit.
    let _ = graceful_shutdown(
        core.runtime_core.clone(),
        core.lifecycle.clone(),
        config.shutdown_grace,
    )
    .await;

    handle.shutdown().await;

    // Unlink the handshake file on clean shutdown — stale files with
    // dead PIDs are the only artifact of ungraceful exits.
    if let Err(e) = std::fs::remove_file(&handshake_path) {
        tracing::debug!(%e, "remove handshake file (non-fatal)");
    }

    tracing::info!("flowstate daemon shut down cleanly");
    Ok(())
}
