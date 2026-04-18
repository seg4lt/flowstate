mod bridge_runtime;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;
use tracing::{debug, info, warn};
use zenui_provider_api::{
    CommandCatalog, CommandKind, McpServerInfo, PermissionDecision, PermissionMode,
    ProviderAdapter, ProviderAgent, ProviderCommand, ProviderKind, ProviderModel,
    ProviderSessionState, ProviderStatus, ProviderStatusLevel, ProviderTurnEvent,
    ProviderTurnOutput, ReasoningEffort, SessionDetail, TurnEventSink, UserInput, UserInputOption,
    UserInputQuestion, skills_disk,
};

const BRIDGE_TIMEOUT_MS: u64 = 120_000;

fn session_cwd(session: &SessionDetail, fallback: &Path) -> PathBuf {
    session
        .cwd
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(|| fallback.to_path_buf())
}

/// Result of asking the user a question: either they picked / typed, or
/// dismissed the dialog. Carried over the writer-task channel so the bridge
/// knows whether to send AnswerUserInput or CancelUserInput.
enum UserInputOutcome {
    Answered { answer: String, was_freeform: bool },
    Cancelled,
}

/// Bridge process wrapper for GitHub Copilot SDK
#[derive(Debug)]
struct CopilotBridgeProcess {
    child: Child,
    // Wrapped in Arc<Mutex> so a background writer task can forward
    // permission/user-input answers back to the bridge concurrently with the
    // main read loop. Mirrors the pattern in provider-claude-sdk/src/lib.rs.
    stdin: Arc<Mutex<ChildStdin>>,
    stdout: Lines<BufReader<ChildStdout>>,
    bridge_session_id: String,
}

/// Idle timeout: a cached bridge with no in-flight turn is killed after
/// this many seconds of inactivity.
const BRIDGE_IDLE_TIMEOUT_SECS: u64 = 120;
/// Watchdog tick interval. Determines the worst-case delay between a
/// bridge crossing the idle threshold and actually being killed.
const BRIDGE_WATCHDOG_INTERVAL_SECS: u64 = 30;

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Cached bridge entry with activity tracking. Wraps the long-lived
/// bridge process with two atomics so a background watchdog can safely
/// cull idle entries without racing against `bridge_request_streaming`.
#[derive(Debug, Clone)]
struct CachedBridge {
    process: Arc<Mutex<CopilotBridgeProcess>>,
    /// Unix epoch seconds at which the last turn finished (or the bridge
    /// was created). Only consulted when `in_flight == 0`.
    last_activity: Arc<AtomicU64>,
    /// Number of turns currently running on this bridge. Incremented at
    /// turn start and decremented via RAII in `ActivityGuard::drop`.
    in_flight: Arc<AtomicU32>,
}

impl CachedBridge {
    fn new(process: CopilotBridgeProcess) -> Self {
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

/// RAII guard held for the duration of a turn. On drop, decrements the
/// in-flight counter and stamps `last_activity = now`, starting the
/// idle clock.
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

/// Wire shape of one skill inside a `capabilities` response. Mirrors
/// the Copilot SDK's `SessionSkillsListResult.skills[]` entry.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BridgeSkill {
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    #[allow(dead_code)]
    source: String,
    #[serde(default)]
    user_invocable: bool,
    #[serde(default)]
    #[allow(dead_code)]
    enabled: bool,
}

/// Wire shape of one sub-agent inside a `capabilities` response.
/// Mirrors `SessionAgentListResult.agents[]`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BridgeCopilotAgent {
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    #[allow(dead_code)]
    display_name: Option<String>,
}

/// Wire shape of one MCP server inside a `capabilities` response.
/// Mirrors `SessionMcpListResult.servers[]`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BridgeCopilotMcp {
    name: String,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    source: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    error: Option<String>,
}

/// ZenUI Bridge Protocol Messages (Rust → TS)
#[derive(Debug, Serialize)]
#[serde(tag = "type")]
enum BridgeRequest {
    #[serde(rename = "create_session")]
    CreateSession {
        cwd: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        /// When `Some`, the bridge calls `client.resumeSession(id, …)`
        /// and only falls back to a fresh create if the SDK rejects the
        /// resume (session expired, deleted, or the upstream Copilot CLI
        /// doesn't recognise it). Sourced from
        /// `session.provider_state.native_thread_id`, which we stamp
        /// after the first successful turn.
        #[serde(skip_serializing_if = "Option::is_none")]
        resume_session_id: Option<String>,
    },
    #[serde(rename = "send_prompt")]
    SendPrompt {
        prompt: String,
        permission_mode: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        reasoning_effort: Option<String>,
    },
    #[serde(rename = "answer_permission")]
    AnswerPermission {
        request_id: String,
        decision: String,
    },
    #[serde(rename = "answer_user_input")]
    AnswerUserInput {
        request_id: String,
        answer: String,
        was_freeform: bool,
    },
    #[serde(rename = "cancel_user_input")]
    CancelUserInput { request_id: String },
    #[serde(rename = "list_models")]
    ListModels,
    /// Enumerate the session's Copilot skills, sub-agents, and MCP
    /// servers by calling `session.rpc.{skills,agent,mcp}.list()` in
    /// the bridge. Requires a live session — callers must go through
    /// `ensure_session_process` first. Fired from
    /// [`ProviderAdapter::session_command_catalog`] on popup open.
    #[serde(rename = "list_capabilities")]
    ListCapabilities,
    #[serde(rename = "interrupt")]
    Interrupt,
}

/// ZenUI Bridge Protocol Messages (TS → Rust)
#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum BridgeResponse {
    #[serde(rename = "ready")]
    Ready,
    #[serde(rename = "session_created")]
    SessionCreated { session_id: String },
    #[serde(rename = "models")]
    Models { models: Vec<ProviderModel> },
    /// Response to `BridgeRequest::ListCapabilities`. Skills come with
    /// the SDK's `userInvocable` flag preserved — the frontend filters
    /// `!user_invocable` entries out of the popup in
    /// `mergeCommandsWithCatalog`, so the wire stays rich enough for
    /// future surfaces (e.g. a Settings pane) to inspect the complete
    /// set.
    #[serde(rename = "capabilities")]
    Capabilities {
        #[serde(default)]
        skills: Vec<BridgeSkill>,
        #[serde(default)]
        agents: Vec<BridgeCopilotAgent>,
        #[serde(default)]
        mcp_servers: Vec<BridgeCopilotMcp>,
    },
    #[serde(rename = "response")]
    Response { output: String },
    #[serde(rename = "interrupted")]
    #[allow(dead_code)]
    Interrupted,
    #[serde(rename = "error")]
    Error { error: String },
    /// Streaming event emitted during send_prompt.
    #[serde(rename = "stream")]
    Stream {
        event: String,
        #[serde(default)]
        delta: Option<String>,
        #[serde(default)]
        call_id: Option<String>,
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        args: Option<serde_json::Value>,
        #[serde(default)]
        output: Option<String>,
        #[serde(default)]
        error: Option<String>,
        #[serde(default)]
        message: Option<String>,
        // Round-trip / plan-mode fields
        #[serde(default)]
        request_id: Option<String>,
        #[serde(default)]
        tool_name: Option<String>,
        #[serde(default)]
        input: Option<serde_json::Value>,
        #[serde(default)]
        suggested: Option<String>,
        #[serde(default)]
        question: Option<String>,
        #[serde(default)]
        choices: Option<Vec<String>>,
        #[serde(default)]
        allow_freeform: Option<bool>,
        #[serde(default)]
        plan_id: Option<String>,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        steps: Option<serde_json::Value>,
        #[serde(default)]
        raw: Option<String>,
        #[serde(default)]
        usage: Option<serde_json::Value>,
        #[serde(default)]
        rate_limit_info: Option<serde_json::Value>,
    },
}

/// GitHub Copilot Provider Adapter
#[derive(Debug, Clone)]
pub struct GitHubCopilotAdapter {
    working_directory: PathBuf,
    sessions: Arc<Mutex<HashMap<String, CachedBridge>>>,
    /// Latches true the first time `ensure_session_process` runs so the
    /// idle-kill watchdog is spawned exactly once per adapter instance.
    watchdog_started: Arc<AtomicBool>,
}

impl GitHubCopilotAdapter {
    pub fn new(working_directory: PathBuf) -> Self {
        Self {
            working_directory,
            sessions: Arc::new(Mutex::new(HashMap::new())),
            watchdog_started: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Spawn the idle-kill watchdog exactly once. Called lazily from
    /// `ensure_session_process` (rather than `new()`) so we don't rely
    /// on `tokio::spawn` being available at adapter construction time.
    ///
    /// Ticks every 30s, scans the sessions map, and kills any bridge
    /// whose `in_flight == 0` and whose `last_activity` is older than
    /// 2 minutes. Removal happens under the outer `sessions` Mutex so a
    /// concurrent `ensure_session_process` either wins the race or
    /// misses and spawns a fresh bridge — no torn state.
    fn ensure_watchdog(&self) {
        if self.watchdog_started.swap(true, Ordering::AcqRel) {
            return;
        }
        let sessions = self.sessions.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(
                BRIDGE_WATCHDOG_INTERVAL_SECS,
            ));
            // Consume the immediate first tick so we don't cull on boot.
            tick.tick().await;
            loop {
                tick.tick().await;
                let now = unix_now();
                let victims: Vec<(String, CachedBridge)> = {
                    let mut map = sessions.lock().await;
                    let stale: Vec<String> = map
                        .iter()
                        .filter(|(_, c)| {
                            c.in_flight.load(Ordering::Acquire) == 0
                                && now.saturating_sub(
                                    c.last_activity.load(Ordering::Acquire),
                                ) > BRIDGE_IDLE_TIMEOUT_SECS
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
                        "copilot bridge idle {}s, killing",
                        BRIDGE_IDLE_TIMEOUT_SECS
                    );
                    let mut process = cached.process.lock().await;
                    let _ = process.child.start_kill();
                }
            }
        });
    }

    /// Spawn the Node.js bridge process
    async fn spawn_bridge(&self) -> Result<CopilotBridgeProcess, String> {
        info!("Spawning GitHub Copilot bridge process...");

        let node = zenui_embedded_node::ensure_extracted()
            .map_err(|e| format!("embedded Node.js setup failed: {e:?}"))?;
        let bridge = bridge_runtime::ensure_extracted()
            .map_err(|e| format!("Copilot bridge extraction failed: {e:?}"))?;

        info!("Using bridge at: {}", bridge.script.display());
        info!("Using embedded node at: {}", node.node_bin.display());

        // Put the embedded node on PATH so the Copilot SDK's internal
        // `node` subprocess calls resolve to the same runtime.
        let existing_path = std::env::var("PATH").unwrap_or_default();
        let new_path = if existing_path.is_empty() {
            node.bin_dir.to_string_lossy().into_owned()
        } else {
            let sep = if cfg!(windows) { ";" } else { ":" };
            format!("{}{sep}{}", node.bin_dir.display(), existing_path)
        };

        let mut child = Command::new(&node.node_bin)
            .arg(&bridge.script)
            .current_dir(&bridge.dir)
            .env("PATH", new_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("Failed to spawn bridge: {e}"))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "Bridge stdin unavailable".to_string())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "Bridge stdout unavailable".to_string())?;

        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    if !line.trim().is_empty() {
                        info!(target: "copilot-bridge", "{}", line);
                    }
                }
            });
        }

        let mut process = CopilotBridgeProcess {
            child,
            stdin: Arc::new(Mutex::new(stdin)),
            stdout: BufReader::new(stdout).lines(),
            bridge_session_id: String::new(),
        };

        debug!("Waiting for bridge ready signal...");
        match tokio::time::timeout(
            std::time::Duration::from_secs(10),
            process.read_response(),
        )
        .await
        {
            Ok(Ok(BridgeResponse::Ready)) => {
                info!("Bridge is ready");
            }
            Ok(Ok(other)) => {
                return Err(format!("Expected ready signal, got: {:?}", other));
            }
            Ok(Err(e)) => {
                return Err(format!("Failed to read ready signal: {e}"));
            }
            Err(_) => {
                return Err("Timeout waiting for bridge ready signal".to_string());
            }
        }

        Ok(process)
    }

    /// Send a request and wait for the final (non-stream) response. Used for create_session.
    async fn bridge_request(
        &self,
        process: &mut CopilotBridgeProcess,
        request: BridgeRequest,
    ) -> Result<BridgeResponse, String> {
        write_request(&process.stdin, &request).await?;

        match tokio::time::timeout(
            std::time::Duration::from_millis(BRIDGE_TIMEOUT_MS),
            process.read_response(),
        )
        .await
        {
            Ok(Ok(response)) => {
                debug!("Bridge response: {:?}", response);
                Ok(response)
            }
            Ok(Err(e)) => Err(format!("Bridge read error: {e}")),
            Err(_) => Err("Bridge request timeout".to_string()),
        }
    }

    /// Send a send_prompt request, forwarding streaming events to the sink while awaiting
    /// the final response line. Spawns a writer task so permission/user-input answers can
    /// be forwarded back to the bridge concurrently with the read loop.
    async fn bridge_request_streaming(
        &self,
        process: &mut CopilotBridgeProcess,
        prompt: String,
        permission_mode: PermissionMode,
        reasoning_effort: Option<ReasoningEffort>,
        events: &TurnEventSink,
    ) -> Result<String, String> {
        write_request(
            &process.stdin,
            &BridgeRequest::SendPrompt {
                prompt,
                permission_mode: permission_mode_to_str(permission_mode).to_string(),
                reasoning_effort: reasoning_effort.map(|e| e.as_str().to_string()),
            },
        )
        .await?;

        debug!("Sent streaming prompt to copilot bridge");

        // Writer task: forwards permission/user-input answers from spawned
        // ask_user / request_permission tasks back into bridge stdin while the
        // turn is in flight. Mirrors provider-claude-sdk/src/lib.rs writer task.
        let stdin = process.stdin.clone();
        let (perm_tx, mut perm_rx) =
            tokio::sync::mpsc::unbounded_channel::<(String, PermissionDecision)>();
        let (q_tx, mut q_rx) =
            tokio::sync::mpsc::unbounded_channel::<(String, UserInputOutcome)>();
        let stdin_for_writer = stdin.clone();
        let writer_task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    Some((request_id, decision)) = perm_rx.recv() => {
                        let req = BridgeRequest::AnswerPermission {
                            request_id,
                            decision: permission_decision_to_str(decision).to_string(),
                        };
                        if let Err(e) = write_request(&stdin_for_writer, &req).await {
                            debug!("failed to forward permission answer: {e}");
                            break;
                        }
                    }
                    Some((request_id, outcome)) = q_rx.recv() => {
                        let req = match outcome {
                            UserInputOutcome::Answered { answer, was_freeform } => {
                                BridgeRequest::AnswerUserInput {
                                    request_id,
                                    answer,
                                    was_freeform,
                                }
                            }
                            UserInputOutcome::Cancelled => {
                                BridgeRequest::CancelUserInput { request_id }
                            }
                        };
                        if let Err(e) = write_request(&stdin_for_writer, &req).await {
                            debug!("failed to forward user input outcome: {e}");
                            break;
                        }
                    }
                    else => break,
                }
            }
        });

        let deadline = tokio::time::Instant::now()
            + std::time::Duration::from_millis(BRIDGE_TIMEOUT_MS);

        let result = loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break Err("Bridge streaming timeout".to_string());
            }
            let line = match tokio::time::timeout(remaining, process.read_response()).await {
                Ok(Ok(resp)) => resp,
                Ok(Err(e)) => break Err(format!("Bridge read error: {e}")),
                Err(_) => break Err("Bridge streaming timeout".to_string()),
            };

            match line {
                BridgeResponse::Response { output } => break Ok(output),
                BridgeResponse::Error { error } => break Err(format!("Copilot error: {error}")),
                BridgeResponse::Stream {
                    event,
                    delta,
                    call_id,
                    name,
                    args,
                    output,
                    error,
                    message,
                    request_id,
                    tool_name,
                    input,
                    suggested,
                    question,
                    choices,
                    allow_freeform,
                    plan_id,
                    title,
                    steps,
                    raw,
                    usage,
                    rate_limit_info,
                } => match event.as_str() {
                    "text_delta" => {
                        if let Some(d) = delta {
                            if !d.is_empty() {
                                events
                                    .send(ProviderTurnEvent::AssistantTextDelta { delta: d })
                                    .await;
                            }
                        }
                    }
                    "reasoning_delta" => {
                        if let Some(d) = delta {
                            if !d.is_empty() {
                                events
                                    .send(ProviderTurnEvent::ReasoningDelta { delta: d })
                                    .await;
                            }
                        }
                    }
                    "tool_started" => {
                        if let (Some(cid), Some(n)) = (call_id, name) {
                            events
                                .send(ProviderTurnEvent::ToolCallStarted {
                                    call_id: cid,
                                    name: n,
                                    args: args.unwrap_or(serde_json::Value::Null),
                                    parent_call_id: None,
                                })
                                .await;
                        }
                    }
                    "tool_completed" => {
                        if let Some(cid) = call_id {
                            events
                                .send(ProviderTurnEvent::ToolCallCompleted {
                                    call_id: cid,
                                    output: output.unwrap_or_default(),
                                    error,
                                })
                                .await;
                        }
                    }
                    "info" | "warning" => {
                        if let Some(msg) = message {
                            events.send(ProviderTurnEvent::Info { message: msg }).await;
                        }
                    }
                    "permission_request" => {
                        let request_id = request_id.unwrap_or_default();
                        let tool_name = tool_name.unwrap_or_default();
                        let input = input.unwrap_or(serde_json::Value::Null);
                        let suggested = suggested
                            .as_deref()
                            .map(parse_decision)
                            .unwrap_or(PermissionDecision::Allow);

                        // request_permission() emits its own PermissionRequest event
                        // with an internal `perm-...` id; don't duplicate it here.
                        // The writer task forwards the decision to the bridge using
                        // the bridge's request_id, since the bridge keeps its own
                        // pending-permissions map keyed by that id.
                        let events_clone = events.clone();
                        let perm_tx = perm_tx.clone();
                        let req_id_for_writer = request_id;
                        tokio::spawn(async move {
                            // The Copilot bridge doesn't honor a mid-answer
                            // permission-mode change, so drop that part of the
                            // tuple. Adapters that do want it (Claude SDK) keep
                            // both halves.
                            let (decision, _mode_override) = events_clone
                                .request_permission(tool_name, input, suggested)
                                .await;
                            let _ = perm_tx.send((req_id_for_writer, decision));
                        });
                    }
                    "user_question" => {
                        let request_id = request_id.unwrap_or_default();
                        let question_text = question.unwrap_or_default();
                        let options: Vec<UserInputOption> = choices
                            .map(|cs| {
                                cs.into_iter()
                                    .enumerate()
                                    .map(|(i, l)| UserInputOption {
                                        id: i.to_string(),
                                        label: l,
                                        description: None,
                                    })
                                    .collect()
                            })
                            .unwrap_or_default();
                        let allow_freeform = allow_freeform.unwrap_or(true);
                        let structured = UserInputQuestion {
                            id: request_id.clone(),
                            text: question_text,
                            header: None,
                            options,
                            multi_select: false,
                            allow_freeform,
                            is_secret: false,
                        };
                        // ask_user() emits its own UserQuestion event with an
                        // internal `q-...` id; don't duplicate it here. The writer
                        // task forwards the answer back to the bridge using the
                        // bridge's request_id, since the bridge's pendingUserInputs
                        // map is keyed by that id.
                        let events_clone = events.clone();
                        let q_tx = q_tx.clone();
                        let req_id_for_writer = request_id;
                        tokio::spawn(async move {
                            let outcome = match events_clone.ask_user(vec![structured]).await {
                                Some(answers) => match answers.into_iter().next() {
                                    Some(a) => UserInputOutcome::Answered {
                                        was_freeform: a.option_ids.is_empty(),
                                        answer: a.answer,
                                    },
                                    None => UserInputOutcome::Cancelled,
                                },
                                None => UserInputOutcome::Cancelled,
                            };
                            let _ = q_tx.send((req_id_for_writer, outcome));
                        });
                    }
                    "plan_proposed" => {
                        if let (Some(pid), Some(t)) = (plan_id, title) {
                            let parsed_steps: Vec<zenui_provider_api::PlanStep> = steps
                                .and_then(|v| serde_json::from_value(v).ok())
                                .unwrap_or_default();
                            events
                                .send(ProviderTurnEvent::PlanProposed {
                                    plan_id: pid,
                                    title: t,
                                    steps: parsed_steps,
                                    raw: raw.unwrap_or_default(),
                                })
                                .await;
                        }
                    }
                    "turn_usage" => {
                        if let Some(u) = usage.and_then(|v| {
                            serde_json::from_value::<zenui_provider_api::TokenUsage>(v).ok()
                        }) {
                            events
                                .send(ProviderTurnEvent::TurnUsage { usage: u })
                                .await;
                        }
                    }
                    "rate_limit_update" => {
                        if let Some(info) = rate_limit_info.and_then(|v| {
                            serde_json::from_value::<zenui_provider_api::RateLimitInfo>(v).ok()
                        }) {
                            events
                                .send(ProviderTurnEvent::RateLimitUpdated { info })
                                .await;
                        }
                    }
                    _ => {
                        debug!("Unknown bridge stream event: {event}");
                    }
                },
                other => {
                    debug!("Unexpected mid-stream bridge message: {:?}", other);
                }
            }
        };

        // Drain the writer task. Dropping the senders closes the channels and lets it exit.
        drop(perm_tx);
        drop(q_tx);
        let _ = writer_task.await;

        result
    }

    /// Return the cached bridge for this session, spawning one if none exists yet.
    async fn ensure_session_process(
        &self,
        session: &SessionDetail,
    ) -> Result<CachedBridge, String> {
        self.ensure_watchdog();
        if let Some(existing) = self
            .sessions
            .lock()
            .await
            .get(&session.summary.session_id)
            .cloned()
        {
            return Ok(existing);
        }

        let mut bridge = self.spawn_bridge().await?;

        // If we've persisted a native_thread_id from a prior run, hand
        // it to the bridge so it calls `client.resumeSession(...)` and
        // picks up the Copilot server's stored conversation. On a cold
        // session or after a resume failure the bridge silently falls
        // back to createSession and returns a fresh id — we then write
        // the fresh id back into provider_state at the end of
        // `execute_turn`, so the next restart resumes against that.
        let resume_session_id = session
            .provider_state
            .as_ref()
            .and_then(|s| s.native_thread_id.clone())
            .filter(|s| !s.is_empty());

        let response = self
            .bridge_request(
                &mut bridge,
                BridgeRequest::CreateSession {
                    cwd: session_cwd(session, &self.working_directory)
                        .display()
                        .to_string(),
                    model: session.summary.model.clone(),
                    resume_session_id,
                },
            )
            .await?;

        match response {
            BridgeResponse::SessionCreated { session_id } => {
                info!("Session created with bridge ID: {}", session_id);
                bridge.bridge_session_id = session_id;
            }
            BridgeResponse::Error { error } => {
                return Err(format!("Failed to create session: {error}"));
            }
            other => {
                return Err(format!("Unexpected bridge response: {:?}", other));
            }
        }

        let cached = CachedBridge::new(bridge);
        let mut sessions = self.sessions.lock().await;
        Ok(sessions
            .entry(session.summary.session_id.clone())
            .or_insert_with(|| cached.clone())
            .clone())
    }

    /// Remove a session's bridge from the cache and kill its process.
    async fn invalidate_session(&self, session_id: &str) {
        let cached = self.sessions.lock().await.remove(session_id);
        if let Some(cached) = cached {
            let mut process = cached.process.lock().await;
            let _ = process.child.start_kill();
        }
    }
}

impl CopilotBridgeProcess {
    async fn read_response(&mut self) -> Result<BridgeResponse, String> {
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

#[async_trait]
impl ProviderAdapter for GitHubCopilotAdapter {
    fn kind(&self) -> ProviderKind {
        ProviderKind::GitHubCopilot
    }

    async fn health(&self) -> ProviderStatus {
        let kind = ProviderKind::GitHubCopilot;
        let label = kind.label();

        info!("Checking GitHub Copilot health...");

        if let Err(err) = zenui_embedded_node::ensure_extracted() {
            return ProviderStatus {
                kind,
                label: label.to_string(),
                installed: false,
                authenticated: false,
                version: None,
                status: ProviderStatusLevel::Error,
                message: Some(format!("embedded Node.js extraction failed: {err:?}")),
                models: copilot_models(),
                enabled: true,
                features: zenui_provider_api::ProviderFeatures::default(),
            };
        }
        if let Err(err) = bridge_runtime::ensure_extracted() {
            return ProviderStatus {
                kind,
                label: label.to_string(),
                installed: false,
                authenticated: false,
                version: None,
                status: ProviderStatusLevel::Error,
                message: Some(format!("Copilot bridge extraction failed: {err:?}")),
                models: copilot_models(),
                enabled: true,
                features: zenui_provider_api::ProviderFeatures::default(),
            };
        }

        let copilot_paths = [
            "/opt/homebrew/bin/copilot",
            "/usr/local/bin/copilot",
            "/home/linuxbrew/.linuxbrew/bin/copilot",
        ];

        let copilot_found = copilot_paths
            .iter()
            .find(|p| std::path::Path::new(p).exists());

        match copilot_found {
            Some(path) => ProviderStatus {
                kind,
                label: label.to_string(),
                installed: true,
                authenticated: true,
                version: None,
                status: ProviderStatusLevel::Ready,
                message: Some(format!("Copilot SDK ready (found at {})", path)),
                models: copilot_models(),
                enabled: true,
                features: zenui_provider_api::ProviderFeatures::default(),
            },
            None => ProviderStatus {
                kind,
                label: label.to_string(),
                installed: true,
                authenticated: false,
                version: None,
                status: ProviderStatusLevel::Warning,
                message: Some(
                    "Copilot CLI not found. Run: gh extension install github/gh-copilot"
                        .to_string(),
                ),
                models: copilot_models(),
                enabled: true,
                features: zenui_provider_api::ProviderFeatures::default(),
            },
        }
    }

    async fn start_session(
        &self,
        _session: &SessionDetail,
    ) -> Result<Option<ProviderSessionState>, String> {
        // Defer the bridge spawn (and the CreateSession round-trip to the
        // Copilot CLI it sits on top of) to the first execute_turn. The
        // bridge_session_id used to be returned here as native_thread_id,
        // but it's a Copilot-CLI-assigned id we can't pre-generate, so we
        // capture it inside execute_turn's result instead.
        Ok(None)
    }

    async fn execute_turn(
        &self,
        session: &SessionDetail,
        input: &UserInput,
        permission_mode: PermissionMode,
        reasoning_effort: Option<ReasoningEffort>,
        events: TurnEventSink,
    ) -> Result<ProviderTurnOutput, String> {
        info!(
            "Executing turn with GitHub Copilot (mode={:?}, effort={:?})",
            permission_mode, reasoning_effort
        );
        if !input.images.is_empty() {
            tracing::warn!(
                provider = ?ProviderKind::GitHubCopilot,
                count = input.images.len(),
                "github copilot SDK adapter dropping image attachments; not implemented"
            );
        }

        let cached = self.ensure_session_process(session).await?;
        // Held for the entire turn. Drops after `process` is released,
        // decrementing in_flight and stamping last_activity = now so the
        // 2-minute idle timer starts ticking.
        let _activity = cached.activity_guard();
        let result = {
            let mut process = cached.process.lock().await;
            // Capture the bridge session id BEFORE the streaming call so
            // we can return it as native_thread_id even on first turn.
            // ensure_session_process populates it during CreateSession.
            let bridge_session_id = process.bridge_session_id.clone();
            let output = self
                .bridge_request_streaming(
                    &mut process,
                    input.text.clone(),
                    permission_mode,
                    reasoning_effort,
                    &events,
                )
                .await?;
            Ok(ProviderTurnOutput {
                output,
                provider_state: Some(ProviderSessionState {
                    native_thread_id: Some(bridge_session_id),
                    metadata: None,
                }),
            })
        };

        if result.is_err() {
            self.invalidate_session(&session.summary.session_id).await;
        }

        result
    }

    async fn end_session(&self, session: &SessionDetail) -> Result<(), String> {
        self.invalidate_session(&session.summary.session_id).await;
        Ok(())
    }

    async fn fetch_models(&self) -> Result<Vec<ProviderModel>, String> {
        // Spawn an ephemeral bridge so we don't disturb any active session,
        // call client.listModels(), kill the process.
        let mut bridge = self.spawn_bridge().await?;
        write_request(&bridge.stdin, &BridgeRequest::ListModels).await?;
        let response = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            bridge.read_response(),
        )
        .await
        .map_err(|_| "Timeout fetching Copilot models".to_string())?
        .map_err(|e| format!("Bridge read error: {e}"))?;
        let _ = bridge.child.start_kill();

        match response {
            BridgeResponse::Models { models } => {
                if models.is_empty() {
                    Err("Copilot bridge returned no models".to_string())
                } else {
                    Ok(models)
                }
            }
            BridgeResponse::Error { error } => Err(format!("Copilot list_models error: {error}")),
            other => Err(format!(
                "Unexpected bridge response for list_models: {:?}",
                other
            )),
        }
    }

    async fn interrupt_turn(&self, session: &SessionDetail) -> Result<String, String> {
        info!("Interrupting GitHub Copilot session...");

        // Look up the live bridge process for this session. The bridge's
        // readline loop now runs send_prompt as a background promise (see
        // bridge/src/index.ts `promptInFlight`), so an `interrupt` message
        // written to stdin is dispatched concurrently with the in-flight turn
        // and calls `session.interrupt()` on the Copilot SDK. We deliberately
        // do NOT drop the session — the bridge's in-memory `this.session`
        // must survive so the next send_prompt continues the same conversation.
        let cached = self
            .sessions
            .lock()
            .await
            .get(&session.summary.session_id)
            .cloned();
        let Some(cached) = cached else {
            return Ok(format!(
                "GitHub Copilot interrupt requested for session '{}' (no active bridge).",
                session.summary.session_id
            ));
        };
        let stdin = {
            let guard = cached.process.lock().await;
            guard.stdin.clone()
        };
        write_request(&stdin, &BridgeRequest::Interrupt).await?;
        Ok(format!(
            "GitHub Copilot turn interrupted for session '{}'.",
            session.summary.session_id
        ))
    }

    /// Copilot-specific scan roots: skills can live in any of the
    /// Copilot, Claude, `agents`, or `.github` conventions depending on
    /// how the user's repo is organised. We walk all four so the popup
    /// surfaces every SKILL.md regardless of convention.
    fn skill_scan_roots(&self) -> (&'static [&'static str], &'static [&'static str]) {
        (
            &[".copilot", ".claude"],
            &[
                ".copilot/skills",
                ".claude/skills",
                ".agents/skills",
                ".github/skills",
            ],
        )
    }

    /// Merge disk SKILL.md entries with the Copilot session's live
    /// skills / agents / MCP servers. Requires a bridge — calls
    /// `ensure_session_process(session)` to boot one if needed — and
    /// invokes `session.rpc.{skills,agent,mcp}.list()` via the
    /// `list_capabilities` RPC.
    ///
    /// On any failure (bridge spawn fail, timeout, malformed response)
    /// we fall through to disk-only so the popup is resilient. The
    /// `!user_invocable` filter is intentionally NOT applied here —
    /// the frontend's `mergeCommandsWithCatalog` handles that so the
    /// wire stays rich enough for future inspectors.
    async fn session_command_catalog(
        &self,
        session: &SessionDetail,
    ) -> Result<CommandCatalog, String> {
        let (home_dirs, project_dirs) = self.skill_scan_roots();
        let cwd_path = session.cwd.as_deref().map(Path::new);
        let roots = skills_disk::scan_roots_for(home_dirs, project_dirs, cwd_path);
        let mut commands = skills_disk::scan(&roots, self.kind());

        let capabilities = self.fetch_capabilities(session).await;
        let (sdk_skills, sdk_agents, sdk_mcp) = match capabilities {
            Ok(c) => c,
            Err(err) => {
                warn!("copilot session_command_catalog: falling back to disk-only ({err})");
                (Vec::new(), Vec::new(), Vec::new())
            }
        };

        let disk_names: std::collections::HashSet<String> =
            commands.iter().map(|c| c.name.clone()).collect();
        for skill in sdk_skills {
            if disk_names.contains(&skill.name) {
                // On-disk SKILL.md wins — it carries the real source
                // (project vs global) from the scanner.
                continue;
            }
            commands.push(ProviderCommand {
                id: format!("github_copilot:builtin:{}", skill.name),
                name: skill.name,
                description: skill.description,
                kind: CommandKind::Builtin,
                user_invocable: skill.user_invocable,
                arg_hint: None,
            });
        }
        commands.sort_by(|a, b| a.name.cmp(&b.name));

        let agents = sdk_agents
            .into_iter()
            .map(|a| ProviderAgent {
                id: format!("github_copilot:agent:{}", a.name),
                name: a.name,
                description: a.description,
            })
            .collect();

        let mcp_servers = sdk_mcp
            .into_iter()
            .map(|m| McpServerInfo {
                enabled: matches!(
                    m.status.as_deref(),
                    Some("connected") | Some("pending")
                ),
                id: format!("github_copilot:mcp:{}", m.name),
                name: m.name,
            })
            .collect();

        Ok(CommandCatalog {
            commands,
            agents,
            mcp_servers,
        })
    }
}

impl GitHubCopilotAdapter {
    /// Send `list_capabilities` over the session's cached bridge. The
    /// bridge must have a live session — callers go through
    /// `ensure_session_process` first so this is safe to invoke.
    async fn fetch_capabilities(
        &self,
        session: &SessionDetail,
    ) -> Result<
        (Vec<BridgeSkill>, Vec<BridgeCopilotAgent>, Vec<BridgeCopilotMcp>),
        String,
    > {
        let cached = self.ensure_session_process(session).await?;
        let _guard = cached.activity_guard();
        let mut process = cached.process.lock().await;
        let response = self
            .bridge_request(&mut process, BridgeRequest::ListCapabilities)
            .await?;
        match response {
            BridgeResponse::Capabilities {
                skills,
                agents,
                mcp_servers,
            } => Ok((skills, agents, mcp_servers)),
            BridgeResponse::Error { error } => {
                Err(format!("copilot list_capabilities error: {error}"))
            }
            other => Err(format!(
                "Unexpected bridge response for list_capabilities: {:?}",
                other
            )),
        }
    }
}

async fn write_request(
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

fn permission_mode_to_str(mode: PermissionMode) -> &'static str {
    match mode {
        PermissionMode::Default => "default",
        PermissionMode::AcceptEdits => "accept_edits",
        PermissionMode::Plan => "plan",
        PermissionMode::Bypass => "bypass",
    }
}

fn permission_decision_to_str(decision: PermissionDecision) -> &'static str {
    match decision {
        PermissionDecision::Allow => "allow",
        PermissionDecision::AllowAlways => "allow_always",
        PermissionDecision::Deny => "deny",
        PermissionDecision::DenyAlways => "deny_always",
    }
}

fn parse_decision(value: &str) -> PermissionDecision {
    match value {
        "allow_always" => PermissionDecision::AllowAlways,
        "deny" => PermissionDecision::Deny,
        "deny_always" => PermissionDecision::DenyAlways,
        _ => PermissionDecision::Allow,
    }
}

fn copilot_models() -> Vec<ProviderModel> {
    // Fallback capability values used only when the live
    // `listModels()` call fails or returns an empty list. Live
    // responses beat these via `fetch_models` (which now carries
    // `context_window` / `max_output_tokens` straight through from
    // the Copilot SDK's ModelCapabilities.limits). Numbers follow
    // each vendor's public model cards.
    vec![
        ProviderModel {
            value: "gpt-4.1".to_string(),
            label: "GPT-4.1".to_string(),
            context_window: Some(1_047_576),
            max_output_tokens: Some(32_768),
        },
        ProviderModel {
            value: "gpt-4o".to_string(),
            label: "GPT-4o".to_string(),
            context_window: Some(128_000),
            max_output_tokens: Some(16_384),
        },
        ProviderModel {
            value: "gpt-5".to_string(),
            label: "GPT-5".to_string(),
            context_window: Some(400_000),
            max_output_tokens: Some(128_000),
        },
        ProviderModel {
            value: "claude-sonnet-4-5".to_string(),
            label: "Claude Sonnet 4.5".to_string(),
            context_window: Some(200_000),
            max_output_tokens: Some(64_000),
        },
    ]
}
