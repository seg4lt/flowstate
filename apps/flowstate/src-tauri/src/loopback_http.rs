//! Loopback HTTP transport mounted alongside the Tauri transport.
//!
//! Every flowstate session running on a non-Claude-SDK provider
//! (opencode, Copilot SDK, Claude CLI, Codex, Copilot CLI) spawns a
//! per-session `flowstate mcp-server` subprocess that needs to call
//! back into the runtime for orchestration dispatch. This module is
//! the server side of that loopback: binds `127.0.0.1:0`, generates a
//! random bearer token, writes a 0600 handshake file under the app
//! data dir, and shares the same `Arc<RuntimeCore>` with the primary
//! Tauri transport so every route reflects live runtime state.
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
//! - Every route except `/api/health` and `/api/version` demands
//!   `Authorization: Bearer <token>`. Token is a 32-byte hex string
//!   generated fresh on each app launch.
//! - The token is written to `<data_dir>/daemon.handshake` with
//!   `0600` permissions. Any same-user process that can read the file
//!   can talk to the runtime — this is the explicit contract, since
//!   the MCP subprocess is such a process.
//! - The handshake file is overwritten atomically on each launch so
//!   a stale token from a previous run is never valid against the
//!   current server.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use serde::Serialize;
use zenui_daemon_core::{Transport, TransportHandle};
use zenui_provider_api::{OrchestrationIpcHandle, OrchestrationIpcInfo};
use zenui_runtime_core::{ConnectionObserver, RuntimeCore};
use zenui_transport_http::HttpTransport;

/// Contents of `<data_dir>/daemon.handshake`. Every subprocess that
/// needs to talk to the runtime reads this file after launch, uses
/// `base_url` + `token` to make its first request, and verifies
/// `schema_version` against what it was built for.
///
/// Keep field names stable — the MCP subprocess parses this JSON.
#[derive(Debug, Serialize)]
pub struct Handshake {
    pub base_url: String,
    pub token: String,
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
    pub token: String,
    pub handshake_path: PathBuf,
    handle: Box<dyn TransportHandle>,
}

/// Generate a fresh 32-byte hex token for the handshake file. Uses
/// the OS RNG via `tauri::Uuid`-adjacent crates would pull extra
/// deps; instead we reuse what's already in the tree: `chrono` is
/// present but not a CSPRNG. We lean on `std::process::id` +
/// `SystemTime` mixed through SHA-ish isn't cryptographic either —
/// so use a small inline CSPRNG from `/dev/urandom` via `std::fs`.
/// Cross-platform variant: read from
/// `getrandom` once it lands — for now, `/dev/urandom` on Unix and a
/// time+pid fallback on Windows is acceptable for a loopback token
/// that an attacker would already need local-user access to read.
fn generate_token() -> String {
    #[cfg(unix)]
    {
        use std::io::Read;
        if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
            let mut buf = [0u8; 32];
            if f.read_exact(&mut buf).is_ok() {
                return hex_encode(&buf);
            }
        }
    }
    // Fallback: mix pid, time, and a counter. Not cryptographic, but
    // the handshake file's 0600 permission is the real gate — this
    // only defeats a trivially-predicted guess by a rogue process
    // that couldn't read the file anyway.
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id();
    let mut bytes = [0u8; 32];
    for (i, chunk) in bytes.chunks_mut(8).enumerate() {
        let n = nanos.wrapping_mul(1103515245).wrapping_add(pid as u128 + i as u128);
        let bs = n.to_le_bytes();
        chunk.copy_from_slice(&bs[..chunk.len()]);
    }
    hex_encode(&bytes)
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

/// Write the handshake atomically with 0600 perms. Atomic means:
/// write to a temp sibling, fsync, rename over the final path.
/// Readers either see the old or the new token — never a truncated
/// file.
fn write_handshake_atomic(path: &Path, hs: &Handshake) -> Result<()> {
    let body =
        serde_json::to_string_pretty(hs).context("serialize daemon handshake to JSON")?;
    let dir = path.parent().context("handshake path has no parent dir")?;
    std::fs::create_dir_all(dir).with_context(|| {
        format!("failed to create handshake dir {}", dir.display())
    })?;
    let tmp = path.with_extension("handshake.tmp");
    {
        use std::io::Write;
        let mut f = std::fs::File::create(&tmp).with_context(|| {
            format!("failed to open handshake tmp file {}", tmp.display())
        })?;
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
/// 0600 perms. Callers read this file from subprocess-spawn sites
/// (provider adapters) to discover `base_url` + `token` at session
/// start.
pub fn spawn(
    data_dir: &Path,
    runtime: Arc<RuntimeCore>,
    observer: Arc<dyn ConnectionObserver>,
    ipc_handle: OrchestrationIpcHandle,
) -> Result<LoopbackHttp> {
    let token = generate_token();
    let bind_addr: SocketAddr = "127.0.0.1:0"
        .parse()
        .expect("static loopback addr must parse");
    let transport: Box<dyn Transport> = Box::new(HttpTransport::new_with_auth(bind_addr, token.clone()));
    let bound = transport.bind().context("bind loopback HTTP listener")?;

    // `address_info()` on the bound handle gives us the actual port
    // (since we asked for :0). We capture it *before* serve() so the
    // handshake file is ready the moment the first subprocess tries
    // to connect.
    let address_info = bound.address_info();
    let base_url = match &address_info {
        zenui_daemon_core::TransportAddressInfo::Http { http_base, .. } => http_base.clone(),
        other => anyhow::bail!(
            "expected HTTP transport address info, got {other:?}"
        ),
    };

    let handle = bound
        .serve(runtime, observer)
        .context("serve loopback HTTP transport")?;

    // Populate the shared IPC handle so every adapter that was
    // constructed with a clone of it can now see the base URL +
    // token. Use `current_exe()` for the binary path — that's the
    // running `flowstate` binary which handles both the UI and the
    // `mcp-server` subcommand. Failures here are non-fatal: if the
    // cell was already populated (double-spawn, shouldn't happen
    // outside dev), we log and continue with the existing value.
    let executable_path = std::env::current_exe()
        .context("resolve current_exe for orchestration IPC handle")?;
    let info = OrchestrationIpcInfo {
        base_url: base_url.clone(),
        auth_token: token.clone(),
        executable_path,
    };
    if let Err(_already) = ipc_handle.set(info) {
        tracing::warn!(
            "orchestration IPC handle already populated; \
             keeping the earlier value and ignoring this one"
        );
    }

    let handshake_path = data_dir.join("daemon.handshake");
    let hs = Handshake {
        base_url: base_url.clone(),
        token: token.clone(),
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
        token,
        handshake_path,
        handle,
    })
}
