use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{mpsc, oneshot, Mutex};
use tracing::{debug, info, warn};
use uuid::Uuid;
use zenui_provider_api::{
    PermissionDecision, PermissionMode, ProviderAdapter, ProviderKind, ProviderModel,
    ProviderSessionState, ProviderStatus, ProviderStatusLevel, ProviderTurnEvent,
    ProviderTurnOutput, ReasoningEffort, SessionDetail, TurnEventSink, UserInput, UserInputOption,
    UserInputQuestion,
};

const TURN_TIMEOUT_SECS: u64 = 600;
const HEALTH_TIMEOUT_SECS: u64 = 10;

fn session_cwd(session: &SessionDetail, fallback: &Path) -> PathBuf {
    session
        .cwd
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(|| fallback.to_path_buf())
}

// ── JSON-RPC 2.0 framing ─────────────────────────────────────────────────────
//
// The copilot binary uses the vscode-jsonrpc Content-Length framing:
//   Content-Length: N\r\n
//   \r\n
//   <json body, N bytes>

async fn write_rpc_frame(stdin: &Mutex<ChildStdin>, msg: &Value) -> Result<(), String> {
    let json = serde_json::to_string(msg).map_err(|e| format!("rpc serialize: {e}"))?;
    let frame = format!("Content-Length: {}\r\n\r\n{}", json.len(), json);
    let mut guard = stdin.lock().await;
    guard
        .write_all(frame.as_bytes())
        .await
        .map_err(|e| format!("rpc write: {e}"))?;
    guard
        .flush()
        .await
        .map_err(|e| format!("rpc flush: {e}"))
}

/// Read one Content-Length-framed JSON-RPC message from the reader.
/// Returns None when the stream ends.
async fn read_rpc_frame(reader: &mut BufReader<ChildStdout>) -> Option<Value> {
    // Read headers until an empty line.
    let mut content_length: Option<usize> = None;
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line).await {
            Ok(0) | Err(_) => return None, // EOF or error
            Ok(_) => {}
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break; // end of headers
        }
        if let Some(val) = trimmed.strip_prefix("Content-Length:") {
            if let Ok(n) = val.trim().parse::<usize>() {
                content_length = Some(n);
            }
        }
    }

    let n = content_length?;
    if n == 0 {
        return None;
    }

    let mut body = vec![0u8; n];
    match reader.read_exact(&mut body).await {
        Ok(_) => {}
        Err(_) => return None,
    }

    serde_json::from_slice(&body).ok()
}

// ── RPC helpers ───────────────────────────────────────────────────────────────

fn make_request(id: u64, method: &str, params: Value) -> Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    })
}

fn make_response(id: &Value, result: Value) -> Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result,
    })
}

fn make_error_response(id: &Value, code: i64, message: &str) -> Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message },
    })
}

// ── Pending requests and server callbacks ─────────────────────────────────────

type PendingMap = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>;
type EventSender = Arc<Mutex<Option<mpsc::UnboundedSender<Value>>>>;
type CallbackSender = Arc<Mutex<Option<mpsc::UnboundedSender<ServerCallback>>>>;

/// A server-initiated request that requires a response from our side.
struct ServerCallback {
    /// The original JSON-RPC request id (needed to build the response).
    rpc_id: Value,
    method: String,
    params: Value,
    /// Send the response JSON-RPC result through here; the dispatcher task
    /// picks it up and writes it back to the copilot process stdin.
    response_tx: oneshot::Sender<Value>,
}

// ── Process handle ────────────────────────────────────────────────────────────

struct CopilotCliProcess {
    child: Child,
    stdin: Arc<Mutex<ChildStdin>>,
    pending: PendingMap,
    next_id: Arc<Mutex<u64>>,
    /// Channel forwarding `session.event` notifications to the active turn loop.
    event_tx: EventSender,
    /// Channel forwarding server callbacks (`permission.request`,
    /// `userInput.request`) to the active turn loop.
    callback_tx: CallbackSender,
}

impl CopilotCliProcess {
    /// Send a JSON-RPC request and await its response.
    async fn call(&self, method: &str, params: Value) -> Result<Value, String> {
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
        match tokio::time::timeout(
            std::time::Duration::from_secs(TURN_TIMEOUT_SECS),
            rx,
        )
        .await
        {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(format!("RPC channel closed waiting for '{method}' response")),
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

async fn run_dispatcher(
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
        let method = msg.get("method").and_then(Value::as_str).map(str::to_string);

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
                            debug!("copilot CLI: callback channel closed, auto-denying '{method_str}'");
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
/// killed after this many seconds of inactivity.
const CLI_IDLE_TIMEOUT_SECS: u64 = 120;
/// Watchdog tick interval.
const CLI_WATCHDOG_INTERVAL_SECS: u64 = 30;

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Cached CLI process entry with activity tracking. Wraps the long-lived
/// copilot CLI subprocess with two atomics so a background watchdog can
/// safely cull idle entries without racing against `run_turn`.
#[derive(Clone)]
struct CachedProcess {
    process: Arc<Mutex<CopilotCliProcess>>,
    /// Unix epoch seconds at which the last turn finished (or the
    /// process was spawned). Only consulted when `in_flight == 0`.
    last_activity: Arc<AtomicU64>,
    /// Number of turns currently running on this process.
    in_flight: Arc<AtomicU32>,
}

impl CachedProcess {
    fn new(process: CopilotCliProcess) -> Self {
        Self {
            process: Arc::new(Mutex::new(process)),
            last_activity: Arc::new(AtomicU64::new(unix_now())),
            in_flight: Arc::new(AtomicU32::new(0)),
        }
    }

    fn activity_guard(&self) -> ActivityGuard {
        self.in_flight.fetch_add(1, Ordering::AcqRel);
        ActivityGuard {
            in_flight: self.in_flight.clone(),
            last_activity: self.last_activity.clone(),
        }
    }
}

/// RAII guard held for the duration of a turn. On drop, stamps
/// `last_activity = now` and decrements the in-flight counter so the
/// 2-minute idle timer starts ticking.
struct ActivityGuard {
    in_flight: Arc<AtomicU32>,
    last_activity: Arc<AtomicU64>,
}

impl Drop for ActivityGuard {
    fn drop(&mut self) {
        self.last_activity.store(unix_now(), Ordering::Release);
        self.in_flight.fetch_sub(1, Ordering::AcqRel);
    }
}

// ── Adapter ───────────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct GitHubCopilotCliAdapter {
    working_directory: PathBuf,
    /// One process per ZenUI session.
    active_processes: Arc<Mutex<HashMap<String, CachedProcess>>>,
    /// Latches true the first time `ensure_session_process` runs so the
    /// idle-kill watchdog is spawned exactly once per adapter instance.
    watchdog_started: Arc<AtomicBool>,
}

impl GitHubCopilotCliAdapter {
    pub fn new(working_directory: PathBuf) -> Self {
        Self {
            working_directory,
            active_processes: Arc::new(Mutex::new(HashMap::new())),
            watchdog_started: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Spawn the idle-kill watchdog exactly once. Called lazily from
    /// `ensure_session_process` (rather than `new()`) so we don't rely
    /// on `tokio::spawn` being available at adapter construction time.
    ///
    /// Ticks every 30s, scans `active_processes`, and kills any CLI
    /// process whose `in_flight == 0` and whose `last_activity` is
    /// older than 2 minutes. Removal happens under the outer Mutex so
    /// a concurrent `ensure_session_process` either reuses the existing
    /// entry or spawns a fresh one — no torn state.
    fn ensure_watchdog(&self) {
        if self.watchdog_started.swap(true, Ordering::AcqRel) {
            return;
        }
        let active_processes = self.active_processes.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(
                CLI_WATCHDOG_INTERVAL_SECS,
            ));
            tick.tick().await;
            loop {
                tick.tick().await;
                let now = unix_now();
                let victims: Vec<(String, CachedProcess)> = {
                    let mut map = active_processes.lock().await;
                    let stale: Vec<String> = map
                        .iter()
                        .filter(|(_, c)| {
                            c.in_flight.load(Ordering::Acquire) == 0
                                && now.saturating_sub(
                                    c.last_activity.load(Ordering::Acquire),
                                ) > CLI_IDLE_TIMEOUT_SECS
                        })
                        .map(|(k, _)| k.clone())
                        .collect();
                    stale
                        .into_iter()
                        .filter_map(|k| map.remove(&k).map(|c| (k, c)))
                        .collect()
                };
                for (sid, cached) in victims {
                    info!(
                        session_id = %sid,
                        "copilot CLI process idle {}s, killing",
                        CLI_IDLE_TIMEOUT_SECS
                    );
                    let mut proc = cached.process.lock().await;
                    let _ = proc.child.start_kill();
                }
            }
        });
    }

    /// Locate the `copilot` binary. Delegates to the cross-platform
    /// resolver in `zenui-provider-api`, which walks PATH (with
    /// PATHEXT on Windows) and falls back to a curated list of
    /// install locations across Linux, macOS, and Windows — including
    /// `~/.local/bin/copilot`, which the previous hardcoded list was
    /// missing. Returns the bare name as a last resort so
    /// `Command::new` still attempts its own PATH lookup.
    fn find_copilot_binary() -> String {
        zenui_provider_api::find_cli_binary("copilot")
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| "copilot".to_string())
    }

    /// Spawn the copilot binary in headless stdio (JSON-RPC) mode.
    async fn spawn_process(binary: &str, cwd: &PathBuf) -> Result<CopilotCliProcess, String> {
        info!("Spawning copilot CLI: {}", binary);
        let mut child = Command::new(binary)
            .args([
                "--headless",
                "--no-auto-update",
                "--log-level",
                "warning",
                "--stdio",
            ])
            .current_dir(cwd)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("Failed to spawn copilot CLI ('{binary}'): {e}"))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "copilot CLI stdin unavailable".to_string())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "copilot CLI stdout unavailable".to_string())?;

        // Drain stderr to logs.
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let t = line.trim();
                    if !t.is_empty() {
                        debug!(target: "copilot-cli", "{}", t);
                    }
                }
            });
        }

        let stdin = Arc::new(Mutex::new(stdin));
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let event_tx: EventSender = Arc::new(Mutex::new(None));
        let callback_tx: CallbackSender = Arc::new(Mutex::new(None));

        // Start the dispatcher.
        let reader = BufReader::new(stdout);
        tokio::spawn(run_dispatcher(
            reader,
            stdin.clone(),
            pending.clone(),
            event_tx.clone(),
            callback_tx.clone(),
        ));

        Ok(CopilotCliProcess {
            child,
            stdin,
            pending,
            next_id: Arc::new(Mutex::new(1)),
            event_tx,
            callback_tx,
        })
    }

    /// Return the cached copilot CLI process for `session`, lazily
    /// spawning + handshaking if it's the first call. Reads the
    /// `needs_create` metadata marker that `start_session` plants on
    /// new threads to decide between `session.create` (fresh) and
    /// `session.resume` (restored from persistence). The marker is
    /// auto-cleared by `run_turn`'s output (which sets `metadata: None`)
    /// after the first successful turn.
    async fn ensure_session_process(
        &self,
        session: &SessionDetail,
    ) -> Result<CachedProcess, String> {
        self.ensure_watchdog();
        if let Some(existing) = self
            .active_processes
            .lock()
            .await
            .get(&session.summary.session_id)
            .cloned()
        {
            return Ok(existing);
        }

        let binary = Self::find_copilot_binary();
        let resolved_cwd = session_cwd(session, &self.working_directory);
        let process = Self::spawn_process(&binary, &resolved_cwd).await?;

        let native_session_id = session
            .provider_state
            .as_ref()
            .and_then(|s| s.native_thread_id.as_deref())
            .map(str::to_string)
            .unwrap_or_else(|| Uuid::new_v4().to_string());

        let cwd = resolved_cwd.to_string_lossy().to_string();
        let model = session.summary.model.as_deref().unwrap_or("gpt-4o");

        let needs_create = session
            .provider_state
            .as_ref()
            .and_then(|s| s.metadata.as_ref())
            .and_then(|m| m.get("needs_create"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);

        let create_params = serde_json::json!({
            "sessionId": native_session_id,
            "model": model,
            "workingDirectory": cwd,
            "requestPermission": true,
            "requestUserInput": true,
            "streaming": true,
        });
        let resume_params = serde_json::json!({
            "sessionId": native_session_id,
            "requestPermission": true,
            "requestUserInput": true,
            "streaming": true,
        });

        // Restored sessions try `session.resume` first. If the upstream
        // CLI has lost the session (upgrade, state wipe, `~/.copilot`
        // cleared, expired on the server) we fall back to
        // `session.create` with the same sessionId so the user keeps
        // the same zenui-visible native_thread_id but gets a fresh
        // upstream conversation — matching the self-healing behaviour
        // the other four provider adapters already have.
        if needs_create {
            process
                .call("session.create", create_params)
                .await
                .map_err(|e| format!("Failed to create: {e}"))?;
        } else {
            match process.call("session.resume", resume_params).await {
                Ok(_) => {}
                Err(resume_err) => {
                    warn!(
                        "copilot CLI: resume failed for {}, falling back to fresh create: {}",
                        native_session_id, resume_err
                    );
                    process
                        .call("session.create", create_params)
                        .await
                        .map_err(|e| {
                            format!(
                                "Failed to resume ({resume_err}) and fallback create also failed: {e}"
                            )
                        })?;
                }
            }
        }

        info!("copilot CLI: session ready ({})", native_session_id);

        let cached = CachedProcess::new(process);
        let mut sessions = self.active_processes.lock().await;
        Ok(sessions
            .entry(session.summary.session_id.clone())
            .or_insert_with(|| cached.clone())
            .clone())
    }

    /// Run one turn: send the prompt and consume events until session.idle / error.
    async fn run_turn(
        process: Arc<Mutex<CopilotCliProcess>>,
        session_id: String,
        input: String,
        permission_mode: PermissionMode,
        events: TurnEventSink,
    ) -> Result<ProviderTurnOutput, String> {
        // Set up channels for this turn.
        let (event_tx, mut event_rx) = mpsc::unbounded_channel::<Value>();
        let (callback_tx, mut callback_rx) = mpsc::unbounded_channel::<ServerCallback>();

        {
            let proc = process.lock().await;
            *proc.event_tx.lock().await = Some(event_tx);
            *proc.callback_tx.lock().await = Some(callback_tx);
        }

        // If plan mode, set the session mode first.
        if permission_mode == PermissionMode::Plan {
            let proc = process.lock().await;
            if let Err(e) = proc
                .call(
                    "session.mode.set",
                    serde_json::json!({ "sessionId": session_id, "mode": "plan" }),
                )
                .await
            {
                warn!("copilot CLI: failed to set plan mode: {e}");
            }
        }

        // Send the user prompt.
        {
            let proc = process.lock().await;
            proc.call(
                "session.send",
                serde_json::json!({ "sessionId": session_id, "prompt": input }),
            )
            .await?;
        }

        let mut accumulated_output = String::new();
        let mut has_deltas = false;
        let deadline = tokio::time::Instant::now()
            + std::time::Duration::from_secs(TURN_TIMEOUT_SECS);

        let turn_result: Result<(), String> = loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break Err("Copilot CLI turn timed out".to_string());
            }

            tokio::select! {
                biased;

                // ── server callback (permission / user-input) ─────────────────
                cb = callback_rx.recv() => {
                    match cb {
                        None => break Err("Callback channel closed unexpectedly".to_string()),
                        Some(cb) => {
                            handle_callback(cb, permission_mode, &events, &process).await;
                        }
                    }
                }

                // ── session event ─────────────────────────────────────────────
                event = event_rx.recv() => {
                    match event {
                        None => break Err("Event channel closed before session.idle".to_string()),
                        Some(params) => {
                            match handle_session_event(
                                params,
                                &events,
                                &mut accumulated_output,
                                &mut has_deltas,
                            )
                            .await {
                                EventOutcome::Continue => {}
                                EventOutcome::Idle => break Ok(()),
                                EventOutcome::Error(e) => break Err(e),
                            }
                        }
                    }
                }

                // ── timeout ───────────────────────────────────────────────────
                _ = tokio::time::sleep_until(deadline) => {
                    break Err("Copilot CLI turn timed out".to_string());
                }
            }
        };

        // Tear down the channels.
        {
            let proc = process.lock().await;
            *proc.event_tx.lock().await = None;
            *proc.callback_tx.lock().await = None;
        }

        turn_result.map(|_| ProviderTurnOutput {
            output: accumulated_output,
            provider_state: Some(ProviderSessionState {
                native_thread_id: Some(session_id),
                metadata: None,
            }),
        })
    }
}

// ── Turn event handler ────────────────────────────────────────────────────────

enum EventOutcome {
    Continue,
    Idle,
    Error(String),
}

async fn handle_session_event(
    params: Value,
    events: &TurnEventSink,
    accumulated: &mut String,
    has_deltas: &mut bool,
) -> EventOutcome {
    let event = match params.get("event") {
        Some(e) => e,
        None => return EventOutcome::Continue,
    };

    let event_type = match event.get("type").and_then(Value::as_str) {
        Some(t) => t,
        None => return EventOutcome::Continue,
    };

    let data = event.get("data").unwrap_or(&Value::Null);

    match event_type {
        "assistant.message_delta" => {
            if let Some(delta) = data.get("deltaContent").and_then(Value::as_str) {
                if !delta.is_empty() {
                    *has_deltas = true;
                    accumulated.push_str(delta);
                    events
                        .send(ProviderTurnEvent::AssistantTextDelta {
                            delta: delta.to_string(),
                        })
                        .await;
                }
            }
        }

        "assistant.reasoning_delta" => {
            if let Some(delta) = data.get("deltaContent").and_then(Value::as_str) {
                if !delta.is_empty() {
                    events
                        .send(ProviderTurnEvent::ReasoningDelta {
                            delta: delta.to_string(),
                        })
                        .await;
                }
            }
        }

        // Fallback: emit the full message content if no streaming deltas arrived.
        "assistant.message" => {
            if !*has_deltas {
                if let Some(content) = data.get("content").and_then(Value::as_str) {
                    if !content.is_empty() {
                        accumulated.push_str(content);
                        events
                            .send(ProviderTurnEvent::AssistantTextDelta {
                                delta: content.to_string(),
                            })
                            .await;
                    }
                }
            }
            // Reset delta tracking for the next assistant turn in the same loop.
            *has_deltas = false;
        }

        "tool.execution_start" => {
            let call_id = data
                .get("toolCallId")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let name = data
                .get("toolName")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let args = data
                .get("arguments")
                .cloned()
                .unwrap_or(serde_json::json!({}));
            events
                .send(ProviderTurnEvent::ToolCallStarted {
                    call_id,
                    name,
                    args,
                    parent_call_id: None,
                })
                .await;
        }

        "tool.execution_complete" => {
            let call_id = data
                .get("toolCallId")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let success = data.get("success").and_then(Value::as_bool).unwrap_or(true);
            let output = data
                .get("result")
                .and_then(|r| r.get("content"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let error = if success {
                None
            } else {
                Some(output.clone())
            };
            events
                .send(ProviderTurnEvent::ToolCallCompleted {
                    call_id,
                    output,
                    error,
                })
                .await;
        }

        "exit_plan_mode.requested" => {
            let plan_id = data
                .get("requestId")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| Uuid::new_v4().to_string());
            let raw: String = data
                .get("planContent")
                .or_else(|| data.get("summary"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let steps = parse_plan_steps(&raw);
            events
                .send(ProviderTurnEvent::PlanProposed {
                    plan_id,
                    title: "Copilot plan".to_string(),
                    steps,
                    raw,
                })
                .await;
        }

        "session.idle" => {
            return EventOutcome::Idle;
        }

        "session.error" => {
            let msg = data
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown error")
                .to_string();
            return EventOutcome::Error(msg);
        }

        _ => {}
    }

    EventOutcome::Continue
}

// ── Server callback handler ───────────────────────────────────────────────────

async fn handle_callback(
    cb: ServerCallback,
    permission_mode: PermissionMode,
    events: &TurnEventSink,
    _process: &Arc<Mutex<CopilotCliProcess>>,
) {
    match cb.method.as_str() {
        "permission.request" => {
            let perm_req = cb.params.get("permissionRequest").cloned().unwrap_or(cb.params.clone());
            let kind = perm_req
                .get("kind")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let request_id = perm_req
                .get("requestId")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();

            // Decide permission based on mode. The Copilot CLI adapter
            // doesn't consume a mid-answer permission-mode switch, so the
            // `_mode_override` half of the tuple is dropped.
            let decision = match permission_mode {
                PermissionMode::Bypass => PermissionDecision::Allow,
                PermissionMode::AcceptEdits | PermissionMode::Plan => {
                    if matches!(kind, "read" | "write") {
                        PermissionDecision::Allow
                    } else {
                        let (d, _mode_override) = events
                            .request_permission(
                                kind.to_string(),
                                perm_req.clone(),
                                PermissionDecision::Allow,
                            )
                            .await;
                        d
                    }
                }
                PermissionMode::Default => {
                    let (d, _mode_override) = events
                        .request_permission(
                            kind.to_string(),
                            perm_req.clone(),
                            PermissionDecision::Allow,
                        )
                        .await;
                    d
                }
            };

            let result = match decision {
                PermissionDecision::Allow | PermissionDecision::AllowAlways => {
                    serde_json::json!({ "kind": "approved" })
                }
                PermissionDecision::Deny | PermissionDecision::DenyAlways => {
                    serde_json::json!({ "kind": "denied-interactively-by-user" })
                }
            };

            let response = make_response(&cb.rpc_id, result);
            let _ = cb.response_tx.send(response);
            let _ = request_id; // used for debug only
        }

        "userInput.request" => {
            let question = cb
                .params
                .get("question")
                .and_then(Value::as_str)
                .unwrap_or("What would you like to do?")
                .to_string();
            let choices: Vec<String> = cb
                .params
                .get("choices")
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            let allow_freeform = cb
                .params
                .get("allowFreeform")
                .and_then(Value::as_bool)
                .unwrap_or(true);

            let q = UserInputQuestion {
                id: Uuid::new_v4().to_string(),
                text: question,
                header: None,
                options: choices
                    .iter()
                    .enumerate()
                    .map(|(i, c)| UserInputOption {
                        id: i.to_string(),
                        label: c.clone(),
                        description: None,
                    })
                    .collect(),
                multi_select: false,
                allow_freeform,
                is_secret: false,
            };

            let result = match events.ask_user(vec![q]).await {
                Some(answers) => {
                    let answer = answers
                        .first()
                        .map(|a| a.answer.clone())
                        .unwrap_or_default();
                    let was_freeform = answers
                        .first()
                        .map(|a| a.option_ids.is_empty())
                        .unwrap_or(true);
                    serde_json::json!({ "answer": answer, "wasFreeform": was_freeform })
                }
                None => serde_json::json!({ "answer": "", "wasFreeform": true }),
            };

            let response = make_response(&cb.rpc_id, result);
            let _ = cb.response_tx.send(response);
        }

        other => {
            // Unknown callback — send an error response so the server doesn't hang.
            warn!("copilot CLI: unhandled server request '{other}', sending error response");
            let response = make_error_response(&cb.rpc_id, -32601, "method not found");
            let _ = cb.response_tx.send(response);
        }
    }
}

// ── ProviderAdapter impl ──────────────────────────────────────────────────────

#[async_trait]
impl ProviderAdapter for GitHubCopilotCliAdapter {
    fn kind(&self) -> ProviderKind {
        ProviderKind::GitHubCopilotCli
    }

    async fn health(&self) -> ProviderStatus {
        let kind = ProviderKind::GitHubCopilotCli;
        let label = kind.label();
        let binary = Self::find_copilot_binary();

        // Check the binary exists.
        if !std::path::Path::new(&binary).exists() && binary != "copilot" {
            return ProviderStatus {
                kind,
                label: label.to_string(),
                installed: false,
                authenticated: false,
                version: None,
                status: ProviderStatusLevel::Error,
                message: Some(
                    "Copilot CLI not found. Run: gh extension install github/gh-copilot"
                        .to_string(),
                ),
                models: copilot_cli_models(),
                enabled: true,
            };
        }

        // Try to spawn and ping the binary.
        let spawn_result = tokio::time::timeout(
            std::time::Duration::from_secs(HEALTH_TIMEOUT_SECS),
            Self::spawn_process(&binary, &self.working_directory),
        )
        .await;

        let mut process = match spawn_result {
            Ok(Ok(p)) => p,
            Ok(Err(e)) => {
                return ProviderStatus {
                    kind,
                    label: label.to_string(),
                    installed: false,
                    authenticated: false,
                    version: None,
                    status: ProviderStatusLevel::Error,
                    message: Some(format!("Failed to start copilot CLI: {e}")),
                    models: copilot_cli_models(),
                    enabled: true,
                };
            }
            Err(_) => {
                return ProviderStatus {
                    kind,
                    label: label.to_string(),
                    installed: false,
                    authenticated: false,
                    version: None,
                    status: ProviderStatusLevel::Error,
                    message: Some("Timed out starting copilot CLI".to_string()),
                    models: copilot_cli_models(),
                    enabled: true,
                };
            }
        };

        // Run all three RPC calls concurrently — they have no data dependencies.
        let timeout = std::time::Duration::from_secs(HEALTH_TIMEOUT_SECS);
        let (ping_raw, status_raw, auth_raw) = tokio::join!(
            tokio::time::timeout(timeout, process.call("ping", serde_json::json!({}))),
            tokio::time::timeout(timeout, process.call("status.get", serde_json::json!({}))),
            tokio::time::timeout(timeout, process.call("auth.getStatus", serde_json::json!({}))),
        );

        let ping_ok = ping_raw.ok().and_then(|r| r.ok()).is_some();

        let version = status_raw
            .ok()
            .and_then(|r| r.ok())
            .and_then(|v| v.get("version").and_then(Value::as_str).map(str::to_string));

        let auth_result = auth_raw.ok().and_then(|r| r.ok());

        let authenticated = auth_result
            .as_ref()
            .and_then(|v| {
                v.get("status")
                    .and_then(Value::as_str)
                    .map(|s| s == "ok" || s == "authenticated")
            })
            .unwrap_or(ping_ok); // fall back to ping success

        // Kill the health-check process.
        let _ = process.child.start_kill();

        if !ping_ok {
            return ProviderStatus {
                kind,
                label: label.to_string(),
                installed: true,
                authenticated: false,
                version,
                status: ProviderStatusLevel::Error,
                message: Some(
                    "Copilot CLI found but did not respond to ping. Is it properly installed?"
                        .to_string(),
                ),
                models: copilot_cli_models(),
                enabled: true,
            };
        }

        let (status, message) = if authenticated {
            (
                ProviderStatusLevel::Ready,
                Some(format!("{label} is installed and authenticated.")),
            )
        } else {
            (
                ProviderStatusLevel::Warning,
                Some(format!(
                    "{label} is installed but not authenticated. Run: gh auth login"
                )),
            )
        };

        ProviderStatus {
            kind,
            label: label.to_string(),
            installed: true,
            authenticated,
            version,
            status,
            message,
            models: copilot_cli_models(),
            enabled: true,
        }
    }

    async fn fetch_models(&self) -> Result<Vec<ProviderModel>, String> {
        let binary = Self::find_copilot_binary();
        let process = Self::spawn_process(&binary, &self.working_directory).await?;

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(HEALTH_TIMEOUT_SECS),
            process.call("models.list", serde_json::json!({})),
        )
        .await
        .map_err(|_| "Timed out fetching models".to_string())?
        .map_err(|e| format!("models.list error: {e}"))?;

        let mut proc = process;
        let _ = proc.child.start_kill();

        let models_arr = match result.get("models").and_then(Value::as_array) {
            Some(arr) => arr.clone(),
            None => return Ok(copilot_cli_models()),
        };

        let models: Vec<ProviderModel> = models_arr
            .iter()
            .filter_map(|m| {
                let value = m.get("id").and_then(Value::as_str)?.to_string();
                let label = m
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or(&value)
                    .to_string();
                Some(ProviderModel { value, label })
            })
            .collect();

        if models.is_empty() {
            Ok(copilot_cli_models())
        } else {
            Ok(models)
        }
    }

    async fn start_session(
        &self,
        session: &SessionDetail,
    ) -> Result<Option<ProviderSessionState>, String> {
        // Defer spawning the copilot CLI binary and the session.create
        // round-trip to first execute_turn. For brand new threads we
        // generate the native session id here and tag it with a
        // metadata marker so ensure_session_process knows to use
        // session.create instead of session.resume on its first call.
        // run_turn returns provider_state with metadata: None after the
        // first successful turn, which clears the marker on disk so
        // subsequent restarts use session.resume.
        if let Some(state) = &session.provider_state {
            if state.native_thread_id.is_some() {
                // Restored session: keep the existing native_thread_id,
                // no marker. ensure_session_process will use resume.
                return Ok(Some(state.clone()));
            }
        }
        let native_session_id = Uuid::new_v4().to_string();
        Ok(Some(ProviderSessionState {
            native_thread_id: Some(native_session_id),
            metadata: Some(serde_json::json!({ "needs_create": true })),
        }))
    }

    async fn execute_turn(
        &self,
        session: &SessionDetail,
        input: &UserInput,
        permission_mode: PermissionMode,
        _reasoning_effort: Option<ReasoningEffort>,
        events: TurnEventSink,
    ) -> Result<ProviderTurnOutput, String> {
        if !input.images.is_empty() {
            tracing::warn!(
                provider = ?ProviderKind::GitHubCopilotCli,
                count = input.images.len(),
                "github copilot CLI adapter dropping image attachments; not implemented"
            );
        }
        let cached = self.ensure_session_process(session).await?;
        // Held for the entire turn. Drops after run_turn completes,
        // stamping last_activity = now and decrementing in_flight so
        // the 2-minute idle timer starts ticking.
        let _activity = cached.activity_guard();

        let native_session_id = session
            .provider_state
            .as_ref()
            .and_then(|s| s.native_thread_id.as_deref())
            .unwrap_or(&session.summary.session_id)
            .to_string();

        Self::run_turn(
            cached.process.clone(),
            native_session_id,
            input.text.clone(),
            permission_mode,
            events,
        )
        .await
    }

    async fn interrupt_turn(&self, session: &SessionDetail) -> Result<String, String> {
        let cached = self
            .active_processes
            .lock()
            .await
            .get(&session.summary.session_id)
            .cloned();

        if let Some(cached) = cached {
            let native_id = session
                .provider_state
                .as_ref()
                .and_then(|s| s.native_thread_id.as_deref())
                .unwrap_or(&session.summary.session_id)
                .to_string();

            let proc = cached.process.lock().await;
            if let Err(e) = proc
                .call("session.abort", serde_json::json!({ "sessionId": native_id }))
                .await
            {
                warn!("copilot CLI: interrupt failed: {e}");
            }
        }

        Ok("Interrupt sent to Copilot CLI.".to_string())
    }

    async fn end_session(&self, session: &SessionDetail) -> Result<(), String> {
        let cached = self
            .active_processes
            .lock()
            .await
            .remove(&session.summary.session_id);

        if let Some(cached) = cached {
            let native_id = session
                .provider_state
                .as_ref()
                .and_then(|s| s.native_thread_id.as_deref())
                .unwrap_or(&session.summary.session_id)
                .to_string();

            let proc = cached.process.lock().await;
            // Best-effort destroy then kill.
            let _ = proc
                .call("session.destroy", serde_json::json!({ "sessionId": native_id }))
                .await;
            drop(proc);

            let mut proc = cached.process.lock().await;
            let _ = proc.child.start_kill();
        }

        Ok(())
    }
}

// ── Utilities ─────────────────────────────────────────────────────────────────

fn copilot_cli_models() -> Vec<ProviderModel> {
    vec![
        ProviderModel { value: "gpt-4o".to_string(),          label: "GPT-4o".to_string() },
        ProviderModel { value: "gpt-4.1".to_string(),         label: "GPT-4.1".to_string() },
        ProviderModel { value: "gpt-5".to_string(),           label: "GPT-5".to_string() },
        ProviderModel { value: "claude-sonnet-4-5".to_string(), label: "Claude Sonnet 4.5".to_string() },
        ProviderModel { value: "claude-sonnet-4-6".to_string(), label: "Claude Sonnet 4.6".to_string() },
        ProviderModel { value: "o3".to_string(),              label: "o3".to_string() },
        ProviderModel { value: "o4-mini".to_string(),         label: "o4-mini".to_string() },
        ProviderModel { value: "gemini-2.5-pro".to_string(),  label: "Gemini 2.5 Pro".to_string() },
    ]
}

/// Parse markdown bullet/numbered list into PlanStep items.
fn parse_plan_steps(raw: &str) -> Vec<zenui_provider_api::PlanStep> {
    raw.lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            let content = trimmed
                .strip_prefix("- ")
                .or_else(|| trimmed.strip_prefix("* "))
                .or_else(|| {
                    // numbered: "1. ", "12. ", etc.
                    let mut chars = trimmed.chars();
                    let digits: String = chars.by_ref().take_while(|c| c.is_ascii_digit()).collect();
                    if !digits.is_empty() && chars.next() == Some('.') {
                        Some(trimmed[digits.len() + 1..].trim())
                    } else {
                        None
                    }
                });
            content.map(|c| zenui_provider_api::PlanStep {
                title: c.to_string(),
                detail: None,
            })
        })
        .collect()
}
