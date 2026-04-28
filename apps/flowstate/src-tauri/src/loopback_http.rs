//! Loopback HTTP transport mounted alongside the Tauri transport.
//!
//! Every flowstate session running on a non-Claude-SDK provider
//! (opencode, Copilot SDK, Claude CLI, Codex, Copilot CLI) spawns a
//! per-session `flowstate mcp-server` subprocess that needs to call
//! back into the runtime for orchestration dispatch. This module is
//! the server side of that loopback: binds `127.0.0.1:0`, writes a
//! 0600 handshake file under the app data dir, and shares the same
//! `Arc<RuntimeCore>` with the primary Tauri transport so every
//! route reflects live runtime state.
//!
//! # Why not reuse daemon-core's `run_blocking`?
//!
//! `run_blocking` builds its own tokio runtime and drives the whole
//! daemon lifecycle. Here we're a co-tenant of the Tauri app's
//! existing async runtime — we just bind + serve and hand the handle
//! back to the caller, who keeps it alive for the life of the
//! process.
//!
//! # Security model
//!
//! - Binds `127.0.0.1:0` (ephemeral loopback port). Never reachable
//!   from another host.
//! - **No bearer-token auth.** On a single-user desktop every local
//!   process that can reach the loopback port already runs with the
//!   user's credentials and has unrestricted access to the flowstate
//!   SQLite store, session attachments, and `~/.claude.json`. A token
//!   on top of the loopback bind would be theater. If flowstate is
//!   ever deployed on a multi-user host, reintroduce
//!   `HttpTransport::new_with_auth` (see git history for the prior
//!   shape) and add a `token` field to `Handshake` below.
//! - The handshake file publishes `base_url` + `pid` +
//!   `schema_version` + `build_sha` for supervisors / version-skew
//!   detection. Permissions are `0600` to match where the token used
//!   to live — cheap and harmless.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use flowstate_app_layer::http::{router as app_layer_router, AppLayerApiState, OpenProjectSender};
use flowstate_app_layer::usage::UsageStore;
use flowstate_app_layer::user_config::UserConfigStore;
use serde::Serialize;
use zenui_daemon_core::{Transport, TransportHandle};
use zenui_provider_api::{OrchestrationIpcHandle, OrchestrationIpcInfo};
use zenui_runtime_core::{ConnectionObserver, RuntimeCore};
use zenui_transport_http::HttpTransport;

/// Contents of `<data_dir>/daemon.handshake`. Subprocesses that need
/// to call back into the runtime read this file to discover the
/// loopback port. `pid` lets a caller verify the daemon is still the
/// one that wrote this file; `schema_version` is for future HTTP-API
/// version-skew detection (a supervisor can refuse to proceed on
/// mismatch). `build_sha` is a debugging aid.
///
/// Keep field names stable — the MCP subprocess (and any future
/// Tauri-proxy client) parses this JSON.
#[derive(Debug, Serialize)]
pub struct Handshake {
    pub base_url: String,
    pub pid: u32,
    pub schema_version: u32,
    pub build_sha: &'static str,
}

/// Live loopback server; drop to release the port. The handle is
/// retained for the life of the app so the transport keeps serving;
/// we don't currently need to call `shutdown()` explicitly because
/// process exit tears the listener down, but keeping the handle
/// means we can call it cleanly from a future `on_quit` hook.
#[allow(dead_code)]
pub struct LoopbackHttp {
    pub base_url: String,
    pub handshake_path: PathBuf,
    handle: Box<dyn TransportHandle>,
}

/// Write the handshake atomically with 0600 perms. Atomic means:
/// write to a temp sibling, fsync, rename over the final path.
/// Readers either see the old or the new contents — never a
/// truncated file.
fn write_handshake_atomic(path: &Path, hs: &Handshake) -> Result<()> {
    let body = serde_json::to_string_pretty(hs).context("serialize daemon handshake to JSON")?;
    let dir = path.parent().context("handshake path has no parent dir")?;
    std::fs::create_dir_all(dir)
        .with_context(|| format!("failed to create handshake dir {}", dir.display()))?;
    let tmp = path.with_extension("handshake.tmp");
    {
        use std::io::Write;
        let mut f = std::fs::File::create(&tmp)
            .with_context(|| format!("failed to open handshake tmp file {}", tmp.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o600);
            f.set_permissions(perms)
                .context("set 0600 permissions on handshake tmp")?;
        }
        f.write_all(body.as_bytes())
            .context("write handshake body")?;
        f.sync_all().context("fsync handshake tmp")?;
    }
    std::fs::rename(&tmp, path)
        .with_context(|| format!("rename handshake {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

/// Bring the loopback HTTP transport up alongside the existing
/// Tauri transport. Binds on the host thread (so port-in-use errors
/// fail fast at app startup), then moves the listener into the
/// caller's tokio runtime for `serve()`. The `runtime_core` handle
/// is shared with the Tauri transport — no duplicated state.
///
/// Writes the handshake file at `<data_dir>/daemon.handshake` with
/// 0600 perms.
#[allow(clippy::too_many_arguments)]
pub fn spawn(
    data_dir: &Path,
    runtime: Arc<RuntimeCore>,
    observer: Arc<dyn ConnectionObserver>,
    ipc_handle: OrchestrationIpcHandle,
    user_config: UserConfigStore,
    usage: Option<Arc<UsageStore>>,
    daemon_base_url: crate::daemon_client::DaemonBaseUrl,
    open_project: OpenProjectSender,
) -> Result<LoopbackHttp> {
    let bind_addr: SocketAddr = "127.0.0.1:0"
        .parse()
        .expect("static loopback addr must parse");
    // Phase 4 — mount the app-layer REST handlers on the same
    // loopback port the transport-http orchestration routes use.
    // The router is pre-`.with_state()`-stamped, so the HttpTransport
    // just needs to merge the raw `Router<()>` at serve time.
    let extra_router = app_layer_router(AppLayerApiState {
        user_config,
        usage,
        open_project: Some(open_project),
    });
    let transport: Box<dyn Transport> =
        Box::new(HttpTransport::new(bind_addr).with_extra_router(extra_router));
    let bound = transport.bind().context("bind loopback HTTP listener")?;

    // `address_info()` on the bound handle gives us the actual port
    // (since we asked for :0). We capture it *before* serve() so the
    // handshake file is ready the moment the first subprocess tries
    // to connect.
    let address_info = bound.address_info();
    let base_url = match &address_info {
        zenui_daemon_core::TransportAddressInfo::Http { http_base, .. } => http_base.clone(),
        other => anyhow::bail!("expected HTTP transport address info, got {other:?}"),
    };

    let handle = bound
        .serve(runtime, observer)
        .context("serve loopback HTTP transport")?;

    // Populate the shared IPC handle so every adapter that was
    // constructed with a clone of it can now see the base URL. Use
    // `current_exe()` for the binary path — that's the running
    // `flowstate` binary which handles both the UI and the
    // `mcp-server` subcommand. Failures here are non-fatal: if the
    // cell was already populated (double-spawn, shouldn't happen
    // outside dev), we log and continue with the existing value.
    let executable_path =
        std::env::current_exe().context("resolve current_exe for orchestration IPC handle")?;
    let info = OrchestrationIpcInfo {
        base_url: base_url.clone(),
        executable_path,
    };
    // Phase 5.5.1 — handle is now a `watch::Sender`-backed channel,
    // so `publish` replaces any prior value rather than rejecting.
    // Respawn cycles (Phase 6 daemon restart) publish the new port
    // and adapters on the next session-spawn read the fresh URL.
    ipc_handle.publish(info);

    // Phase 5 — publish the base URL to the DaemonClient channel so
    // the 22 app-layer Tauri commands can now reach the matching
    // HTTP handler (served on this same listener by the
    // `HttpTransport::with_extra_router` merge).
    daemon_base_url.publish(base_url.clone());

    let handshake_path = data_dir.join("daemon.handshake");
    let hs = Handshake {
        base_url: base_url.clone(),
        pid: std::process::id(),
        schema_version: 1,
        build_sha: option_env!("FLOWSTATE_BUILD_SHA").unwrap_or("dev"),
    };
    write_handshake_atomic(&handshake_path, &hs).context("write handshake file")?;

    tracing::info!(
        base_url = %base_url,
        handshake = %handshake_path.display(),
        "loopback HTTP transport ready"
    );

    Ok(LoopbackHttp {
        base_url,
        handshake_path,
        handle,
    })
}
