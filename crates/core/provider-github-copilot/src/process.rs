//! Long-lived Copilot bridge subprocess, idle-watchdog constants, the
//! shared `ProcessCache<CopilotBridgeProcess>` alias, plus the I/O
//! helper `write_request` that serializes bridge frames.
//!
//! Extracted from `lib.rs` in the phase 3 god-file split.

use std::sync::Arc;

use tokio::io::{AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::Mutex;
use tracing::debug;

use crate::wire::{BridgeRequest, BridgeResponse};

pub(crate) struct CopilotBridgeProcess {
    pub(crate) child: Child,
    /// Cross-platform process-group / Job-Object owning the Node
    /// bridge subtree. Used by `Drop` + the idle watchdog's kill_fn
    /// so the `copilot` CLI subprocess the bridge spawns via
    /// `new CopilotClient({ useStdio: true })` dies alongside the
    /// bridge. Without this the CLI grandchild would reparent to
    /// PID 1 (Unix) / orphan (Windows) when flowstate exits. See
    /// `zenui_provider_api::ProcessGroup`.
    pub(crate) process_group: zenui_provider_api::ProcessGroup,
    // Wrapped in Arc<Mutex> so a background writer task can forward
    // permission/user-input answers back to the bridge concurrently with the
    // main read loop. Mirrors the pattern in provider-claude-sdk/src/lib.rs.
    pub(crate) stdin: Arc<Mutex<ChildStdin>>,
    pub(crate) stdout: Lines<BufReader<ChildStdout>>,
    pub(crate) bridge_session_id: String,
}

impl Drop for CopilotBridgeProcess {
    fn drop(&mut self) {
        self.process_group.kill_best_effort();
        let _ = self.child.start_kill();
    }
}

/// Idle timeout: a cached bridge with no in-flight turn is killed after
/// this many seconds of inactivity.
pub(crate) const BRIDGE_IDLE_TIMEOUT_SECS: u64 = 30 * 60;
/// Watchdog tick interval. Determines the worst-case delay between a
/// bridge crossing the idle threshold and actually being killed.
pub(crate) const BRIDGE_WATCHDOG_INTERVAL_SECS: u64 = 30;

/// Per-request timeout for non-stream bridge calls.
pub(crate) const BRIDGE_TIMEOUT_MS: u64 = 30 * 60 * 1_000;

/// Alias for the shared `ProcessCache` entry type. Each cached entry
/// wraps a long-lived Copilot bridge child along with the atomics the
/// idle-kill watchdog reads; see `zenui_provider_api::process_cache`.
pub(crate) type CachedBridge = zenui_provider_api::CachedProcess<CopilotBridgeProcess>;

impl CopilotBridgeProcess {
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
    let json = serde_json::to_string(request)
        .map_err(|e| format!("Failed to serialize bridge request: {e}"))?;
    let mut guard = stdin.lock().await;
    guard
        .write_all(json.as_bytes())
        .await
        .map_err(|e| format!("Failed to write to bridge: {e}"))?;
    guard
        .write_all(b"\n")
        .await
        .map_err(|e| format!("Failed to write to bridge: {e}"))?;
    guard
        .flush()
        .await
        .map_err(|e| format!("Failed to flush bridge stdin: {e}"))
}
