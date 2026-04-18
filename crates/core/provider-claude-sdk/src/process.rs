//! Long-lived Claude SDK bridge subprocess: spawn-time state,
//! per-message I/O helpers, and the idle-watchdog constants / cache
//! alias that connect this process type to the shared
//! `zenui_provider_api::ProcessCache`.
//!
//! Extracted from `lib.rs` in the phase 3 god-file split. Wire
//! protocol types (BridgeRequest/BridgeResponse) live in `wire.rs`;
//! mid-turn RPC types in `rpc.rs`.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::io::{BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::{Mutex, oneshot};
use tracing::debug;

use crate::rpc::BridgeRpcResponse;
use crate::wire::BridgeRequest;

#[derive(Debug)]
pub(crate) struct ClaudeBridgeProcess {
    pub(crate) child: Child,
    pub(crate) stdin: Arc<Mutex<ChildStdin>>,
    pub(crate) stdout: Lines<BufReader<ChildStdout>>,
    pub(crate) bridge_session_id: String,
    /// Pending mid-turn RPC responses keyed by request_id. An adapter
    /// method fires an RPC to the bridge by inserting a `oneshot::Sender`
    /// here; `run_turn`'s drain loop forwards the matching
    /// `BridgeResponse::RpcResponse` back through the sender. The
    /// caller `await`s the receiver under a timeout and cleans its
    /// entry on both success and cancellation paths so senders don't
    /// leak across turn boundaries.
    ///
    /// Kept here (rather than on the cache slot) so migrating the
    /// outer sessions map to the shared `ProcessCache<T>` helper
    /// doesn't need a per-provider extension: `T` carries whatever
    /// auxiliary state is truly adapter-specific.
    pub(crate) pending_rpcs: Arc<Mutex<HashMap<String, oneshot::Sender<BridgeRpcResponse>>>>,
}

/// Idle timeout: a cached bridge with no in-flight turn is killed after
/// this many seconds of inactivity.
pub(crate) const BRIDGE_IDLE_TIMEOUT_SECS: u64 = 120;
/// Watchdog tick interval. Determines the worst-case delay between a
/// bridge crossing the idle threshold and actually being killed.
pub(crate) const BRIDGE_WATCHDOG_INTERVAL_SECS: u64 = 30;

/// Alias for the shared `ProcessCache` entry type — see
/// `zenui_provider_api::process_cache` for the idle-watchdog and
/// activity-guard plumbing. The claude-sdk adapter keeps two sibling
/// maps (`session_stdins`, `session_pending_rpcs`) so mid-turn callers
/// can bypass `run_turn`'s outer lock without re-locking the cached
/// process. See `ClaudeSdkAdapter` docs.
pub(crate) type CachedBridge = zenui_provider_api::CachedProcess<ClaudeBridgeProcess>;

use crate::wire::BridgeResponse;

impl ClaudeBridgeProcess {
    pub(crate) async fn read_response(&mut self) -> Result<BridgeResponse, String> {
        loop {
            match self.stdout.next_line().await {
                Ok(Some(line)) => {
                    let line = line.trim();
                    if line.is_empty() {
                        continue;
                    }
                    debug!("Bridge output: {}", line);
                    return serde_json::from_str(line)
                        .map_err(|e| format!("Failed to parse bridge response '{line}': {e}"));
                }
                Ok(None) => {
                    return Err("Bridge process closed stdout".to_string());
                }
                Err(e) => {
                    return Err(format!("Failed to read from bridge: {e}"));
                }
            }
        }
    }
}

pub(crate) async fn write_request(
    stdin: &Arc<Mutex<ChildStdin>>,
    request: &BridgeRequest,
) -> Result<(), String> {
    let mut guard = stdin.lock().await;
    zenui_provider_api::write_json_line(&mut *guard, request, "bridge").await
}

