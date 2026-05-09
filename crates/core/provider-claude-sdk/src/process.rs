//! Long-lived Claude SDK bridge subprocess: spawn-time state,
//! per-message I/O helpers, and the idle-watchdog constants / cache
//! alias that connect this process type to the shared
//! `zenui_provider_api::ProcessCache`.
//!
//! Extracted from `lib.rs` in the phase 3 god-file split. Wire
//! protocol types (BridgeRequest/BridgeResponse) live in `wire.rs`;
//! mid-turn RPC types in `rpc.rs`.
//!
//! ## Persistent background reader
//!
//! Session bridges (those stored in `ProcessCache`) are "promoted" by
//! `ensure_session_process` after the ready handshake via
//! [`ClaudeBridgeProcess::promote_to_session_bridge`]. Promotion:
//!
//! 1. Takes ownership of the raw `ChildStdout` (swaps `stdout_direct`
//!    to `None`).
//! 2. Creates an `mpsc::unbounded_channel` for forwarded lines.
//! 3. Spawns a persistent background reader task that owns the raw
//!    stdout and routes:
//!    - All non-spontaneous lines → `line_tx` (read by `run_turn`
//!      via `read_response()`).
//!    - `{ "type": "spontaneous_turn" }` events → `spontaneous_tx`
//!      (read by `RuntimeCore::init_spontaneous_turn_listener` which
//!      fires `spawn_peer_turn`).
//!
//! Ephemeral bridges (e.g. `fetch_models`) are never promoted and use
//! `stdout_direct` throughout their short lives.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::{Mutex, mpsc, oneshot};
use tracing::debug;

use crate::rpc::BridgeRpcResponse;
use crate::wire::BridgeRequest;

/// A spontaneous model turn that arrived between user-initiated turns.
///
/// Emitted by the bridge when the Claude Code SDK fires a new model
/// iteration with no active user prompt — the most common cause is a
/// background `Bash` task completing and the CLI sending a completion
/// notification. The `RuntimeCore` subscribes to these events via
/// `init_spontaneous_turn_listener` and fires `spawn_peer_turn`.
///
/// Re-exported from `zenui_provider_api` as `SpontaneousTurnEvent`.
pub use zenui_provider_api::SpontaneousTurnEvent;

/// Internal I/O mode for a bridge process.
/// - `Direct`: raw stdout, used by ephemeral bridges and during the
///   initial ready handshake before promotion.
/// - `Channel`: channel receiver fed by the persistent background
///   reader task; used by session bridges after promotion.
enum BridgeStdout {
    Direct(Lines<BufReader<ChildStdout>>),
    Channel(mpsc::UnboundedReceiver<Result<String, String>>),
}

pub(crate) struct ClaudeBridgeProcess {
    pub(crate) child: Child,
    /// Cross-platform process-group / Job-Object owning the Node
    /// bridge subtree. Stored alongside `child` so Drop can reap the
    /// whole tree (the Node bridge plus any `claude` CLI subprocess
    /// the `@anthropic-ai/claude-agent-sdk` may fork for tool use).
    /// See `zenui_provider_api::ProcessGroup`.
    pub(crate) process_group: zenui_provider_api::ProcessGroup,
    pub(crate) stdin: Arc<Mutex<ChildStdin>>,
    /// I/O source for `read_response()`. Starts as `Direct` (raw
    /// ChildStdout) and is promoted to `Channel` by
    /// `promote_to_session_bridge` when a session is established.
    stdout: BridgeStdout,
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

impl ClaudeBridgeProcess {
    pub(crate) fn new(
        child: Child,
        process_group: zenui_provider_api::ProcessGroup,
        stdin: Arc<Mutex<ChildStdin>>,
        stdout: Lines<BufReader<ChildStdout>>,
    ) -> Self {
        Self {
            child,
            process_group,
            stdin,
            stdout: BridgeStdout::Direct(stdout),
            bridge_session_id: String::new(),
            pending_rpcs: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Promote from direct-stdout mode to channel mode.
    ///
    /// Takes ownership of the raw ChildStdout and spawns the persistent
    /// background reader task. After this call, `read_response()` reads
    /// from the channel instead of the raw stdout.
    ///
    /// Panics if called twice (the second call would find no Direct
    /// stdout to take).
    pub(crate) fn promote_to_session_bridge(
        &mut self,
        session_id: String,
        spontaneous_tx: mpsc::UnboundedSender<SpontaneousTurnEvent>,
    ) {
        let direct = match std::mem::replace(&mut self.stdout, BridgeStdout::Channel({
            // Create a placeholder channel receiver; we'll replace it below
            // once we know the real sender. Rust requires the enum variant
            // to be fully initialized, so we use a temporary dummy.
            let (_, rx) = mpsc::unbounded_channel::<Result<String, String>>();
            rx
        })) {
            BridgeStdout::Direct(lines) => lines,
            BridgeStdout::Channel(_) => {
                panic!("promote_to_session_bridge called on an already-promoted bridge");
            }
        };

        let (line_tx, line_rx) = mpsc::unbounded_channel::<Result<String, String>>();
        self.stdout = BridgeStdout::Channel(line_rx);

        spawn_background_reader(direct, line_tx, spontaneous_tx, session_id);
    }

    pub(crate) async fn read_response(&mut self) -> Result<BridgeResponse, String> {
        match &mut self.stdout {
            BridgeStdout::Direct(lines) => {
                loop {
                    match lines.next_line().await {
                        Ok(Some(line)) => {
                            let line = line.trim();
                            if line.is_empty() {
                                continue;
                            }
                            debug!("Bridge output (direct): {}", line);
                            return serde_json::from_str(line).map_err(|e| {
                                format!("Failed to parse bridge response '{line}': {e}")
                            });
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
            BridgeStdout::Channel(rx) => {
                loop {
                    match rx.recv().await {
                        Some(Ok(line)) => {
                            let line = line.trim().to_owned();
                            if line.is_empty() {
                                continue;
                            }
                            debug!("Bridge output (channel): {}", line);
                            return serde_json::from_str(&line).map_err(|e| {
                                format!("Failed to parse bridge response '{line}': {e}")
                            });
                        }
                        Some(Err(e)) => {
                            return Err(e);
                        }
                        None => {
                            return Err("Bridge process closed stdout".to_string());
                        }
                    }
                }
            }
        }
    }
}

/// Idle timeout: a cached bridge with no in-flight turn is killed after
/// this many seconds of inactivity.
pub(crate) const BRIDGE_IDLE_TIMEOUT_SECS: u64 = 30 * 60;
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

impl Drop for ClaudeBridgeProcess {
    fn drop(&mut self) {
        // Kill the entire process-group / Job-Object subtree (the
        // Node bridge and any `claude` CLI subprocess the SDK forked
        // for tool use) atomically, so no grandchild reparents to
        // PID 1 (Unix) or orphans (Windows) when flowstate exits or
        // the idle watchdog reaps the bridge. `start_kill` on
        // `child` is the existing belt; `process_group` is the
        // suspenders.
        self.process_group.kill_best_effort();
        let _ = self.child.start_kill();
    }
}

pub(crate) async fn write_request(
    stdin: &Arc<Mutex<ChildStdin>>,
    request: &BridgeRequest,
) -> Result<(), String> {
    let mut guard = stdin.lock().await;
    zenui_provider_api::write_json_line(&mut *guard, request, "bridge").await
}

/// Spawn the persistent background reader task for a session bridge.
///
/// Owns the raw stdout `Lines` and:
/// 1. Forwards all non-spontaneous lines to `line_tx` for `run_turn`
///    to consume via `ClaudeBridgeProcess::read_response()`.
/// 2. Intercepts `{ "type": "spontaneous_turn" }` events (emitted by
///    the bridge when the SDK fires a model turn with no active user
///    prompt) and routes them to `spontaneous_tx` so
///    `RuntimeCore::init_spontaneous_turn_listener` can fire a new
///    session turn via `spawn_peer_turn`.
///
/// This task lives for the lifetime of the bridge process. When the
/// bridge exits, EOF causes the task to drop `line_tx`, signalling
/// `read_response()` with `None`.
fn spawn_background_reader(
    mut stdout: Lines<BufReader<ChildStdout>>,
    line_tx: mpsc::UnboundedSender<Result<String, String>>,
    spontaneous_tx: mpsc::UnboundedSender<SpontaneousTurnEvent>,
    session_id: String,
) {
    tokio::spawn(async move {
        loop {
            match stdout.next_line().await {
                Ok(Some(line)) => {
                    let trimmed = line.trim().to_string();
                    if trimmed.is_empty() {
                        continue;
                    }
                    // Cheap heuristic before full parse: only bother if
                    // the line contains "spontaneous_turn".
                    if trimmed.contains("spontaneous_turn") {
                        if let Ok(BridgeResponse::SpontaneousTurn { output, .. }) =
                            serde_json::from_str::<BridgeResponse>(&trimmed)
                        {
                            tracing::info!(
                                session_id = %session_id,
                                output_len = output.len(),
                                "bridge background reader: spontaneous turn detected"
                            );
                            let _ = spontaneous_tx.send(SpontaneousTurnEvent {
                                session_id: session_id.clone(),
                                output,
                            });
                            continue; // Do NOT forward to line_tx
                        }
                    }
                    // Normal line — forward to run_turn via channel.
                    if line_tx.send(Ok(trimmed)).is_err() {
                        // Receiver dropped (bridge being torn down).
                        break;
                    }
                }
                Ok(None) => {
                    // Bridge process exited cleanly.
                    let _ = line_tx.send(Err("Bridge process closed stdout".to_string()));
                    break;
                }
                Err(e) => {
                    let _ = line_tx.send(Err(format!("Failed to read from bridge: {e}")));
                    break;
                }
            }
        }
    });
}
