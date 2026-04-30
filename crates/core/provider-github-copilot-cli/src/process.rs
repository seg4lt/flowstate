//! Long-lived copilot CLI subprocess: shared type aliases for the
//! pending-request / event-forwarding / callback-forwarding channels,
//! the `CopilotCliProcess` handle itself, its inherent `call` method,
//! and the background dispatcher that routes incoming framed
//! messages.
//!
//! Extracted from `lib.rs` in the phase 3 god-file split. JSON-RPC
//! framing helpers live in `rpc.rs`.

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::Value;
use tokio::io::BufReader;
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::{Mutex, mpsc, oneshot};
use tracing::debug;

use crate::TURN_TIMEOUT_SECS;
use crate::rpc::{make_error_response, make_request, read_rpc_frame, write_rpc_frame};

pub(crate) type PendingMap = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>;
pub(crate) type EventSender = Arc<Mutex<Option<mpsc::UnboundedSender<Value>>>>;
pub(crate) type CallbackSender = Arc<Mutex<Option<mpsc::UnboundedSender<ServerCallback>>>>;

/// A server-initiated request that requires a response from our side.
pub(crate) struct ServerCallback {
    /// The original JSON-RPC request id (needed to build the response).
    pub(crate) rpc_id: Value,
    pub(crate) method: String,
    pub(crate) params: Value,
    /// Send the response JSON-RPC result through here; the dispatcher task
    /// picks it up and writes it back to the copilot process stdin.
    pub(crate) response_tx: oneshot::Sender<Value>,
}

// ── Process handle ────────────────────────────────────────────────────────────

pub(crate) struct CopilotCliProcess {
    pub(crate) child: Child,
    /// Cross-platform process-group / Job-Object owning the
    /// `copilot` CLI subtree. The `Drop` below reaps any per-session
    /// agent workers / MCP subprocesses the CLI forked so they die
    /// with the parent. See `zenui_provider_api::ProcessGroup`.
    pub(crate) process_group: zenui_provider_api::ProcessGroup,
    pub(crate) stdin: Arc<Mutex<ChildStdin>>,
    pub(crate) pending: PendingMap,
    pub(crate) next_id: Arc<Mutex<u64>>,
    /// Channel forwarding `session.event` notifications to the active turn loop.
    pub(crate) event_tx: EventSender,
    /// Channel forwarding server callbacks (`permission.request`,
    /// `userInput.request`) to the active turn loop.
    pub(crate) callback_tx: CallbackSender,
    /// Copilot CLI sessionId bound to this process — stored alongside
    /// the child handle (rather than in a sibling map) so the shared
    /// `ProcessCache<T>` only needs to cache one type per session.
    pub(crate) native_session_id: String,
}

impl Drop for CopilotCliProcess {
    fn drop(&mut self) {
        self.process_group.kill_best_effort();
        let _ = self.child.start_kill();
    }
}

impl CopilotCliProcess {
    /// Send a JSON-RPC request and await its response.
    pub(crate) async fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        let id = {
            let mut guard = self.next_id.lock().await;
            let id = *guard;
            *guard += 1;
            id
        };
        let (tx, rx) = oneshot::channel();
        {
            self.pending.lock().await.insert(id, tx);
        }
        let msg = make_request(id, method, params);
        write_rpc_frame(&self.stdin, &msg).await?;
        match tokio::time::timeout(std::time::Duration::from_secs(TURN_TIMEOUT_SECS), rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(format!(
                "RPC channel closed waiting for '{method}' response"
            )),
            Err(_) => {
                self.pending.lock().await.remove(&id);
                Err(format!("RPC timeout waiting for '{method}' response"))
            }
        }
    }
}

// ── Background dispatcher ─────────────────────────────────────────────────────
//
// Spawned once per CopilotCliProcess. Reads all incoming framed messages and
// routes them:
//   • JSON-RPC response (has "id", no "method") → wake pending request
//   • "session.event" notification → forward to event_tx
//   • "permission.request" / "userInput.request" requests → forward to callback_tx
//     and spawn a sub-task that awaits the response then writes it back to stdin.

pub(crate) async fn run_dispatcher(
    mut reader: BufReader<ChildStdout>,
    stdin: Arc<Mutex<ChildStdin>>,
    pending: PendingMap,
    event_tx: EventSender,
    callback_tx: CallbackSender,
) {
    loop {
        let Some(msg) = read_rpc_frame(&mut reader).await else {
            debug!("copilot CLI: stdout closed, stopping dispatcher");
            break;
        };

        // Classify the message.
        let has_id = msg.get("id").is_some();
        let method = msg
            .get("method")
            .and_then(Value::as_str)
            .map(str::to_string);

        if let Some(method_str) = method {
            if has_id {
                // ── server-initiated request (requires a response) ────────────
                let rpc_id = msg["id"].clone();
                let params = msg.get("params").cloned().unwrap_or(Value::Null);
                let (response_tx, response_rx) = oneshot::channel::<Value>();
                let cb = ServerCallback {
                    rpc_id: rpc_id.clone(),
                    method: method_str.clone(),
                    params,
                    response_tx,
                };
                {
                    let guard = callback_tx.lock().await;
                    if let Some(tx) = guard.as_ref() {
                        if tx.send(cb).is_err() {
                            debug!(
                                "copilot CLI: callback channel closed, auto-denying '{method_str}'"
                            );
                            // No turn loop listening — write an error response so the
                            // copilot binary doesn't block indefinitely.
                            let err_resp = make_error_response(&rpc_id, -32603, "no handler");
                            let stdin2 = stdin.clone();
                            tokio::spawn(async move {
                                let _ = write_rpc_frame(&stdin2, &err_resp).await;
                            });
                            continue;
                        }
                    } else {
                        // No active turn; auto-deny.
                        let err_resp = make_error_response(&rpc_id, -32603, "no active session");
                        let stdin2 = stdin.clone();
                        tokio::spawn(async move {
                            let _ = write_rpc_frame(&stdin2, &err_resp).await;
                        });
                        continue;
                    }
                }
                // Spawn a task that waits for the turn loop to supply the
                // response, then writes it back to the copilot binary.
                let stdin2 = stdin.clone();
                tokio::spawn(async move {
                    if let Ok(result) = response_rx.await {
                        let _ = write_rpc_frame(&stdin2, &result).await;
                    }
                });
            } else {
                // ── notification (no response needed) ────────────────────────
                if method_str == "session.event" {
                    let guard = event_tx.lock().await;
                    if let Some(tx) = guard.as_ref() {
                        let params = msg.get("params").cloned().unwrap_or(Value::Null);
                        let _ = tx.send(params);
                    }
                }
                // session.lifecycle and others are intentionally ignored.
            }
        } else if has_id {
            // ── JSON-RPC response ─────────────────────────────────────────────
            let id = msg["id"].as_u64();
            if let Some(id) = id {
                let mut guard = pending.lock().await;
                if let Some(tx) = guard.remove(&id) {
                    let result = if msg.get("error").is_some() {
                        let err = msg["error"]["message"]
                            .as_str()
                            .unwrap_or("unknown error")
                            .to_string();
                        Err(err)
                    } else {
                        Ok(msg.get("result").cloned().unwrap_or(Value::Null))
                    };
                    let _ = tx.send(result);
                }
            }
        }
    }
}

// ── Idle-kill watchdog ───────────────────────────────────────────────────────

/// Idle timeout: a cached copilot CLI process with no in-flight turn is

pub(crate) const CLI_IDLE_TIMEOUT_SECS: u64 = 120;
/// Watchdog tick interval.
pub(crate) const CLI_WATCHDOG_INTERVAL_SECS: u64 = 30;

/// Alias for the shared `ProcessCache` entry type — see
/// `zenui_provider_api::process_cache` for the idle-watchdog and
/// activity-guard plumbing. The copilot CLI's per-session metadata
/// (`native_session_id`) lives inside `CopilotCliProcess` so a single
/// cache slot is enough.
pub(crate) type CachedProcess = zenui_provider_api::CachedProcess<CopilotCliProcess>;
