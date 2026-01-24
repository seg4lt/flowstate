use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;
use tracing::{debug, info};
use zenui_provider_api::{
    PermissionDecision, PermissionMode, ProviderAdapter, ProviderKind, ProviderModel,
    ProviderSessionState, ProviderStatus, ProviderStatusLevel, ProviderTurnEvent,
    ProviderTurnOutput, SessionDetail, TurnEventSink,
};

const BRIDGE_TIMEOUT_MS: u64 = 120_000;

/// Bridge process wrapper for GitHub Copilot SDK
#[derive(Debug)]
struct CopilotBridgeProcess {
    child: Child,
    // Wrapped in Arc<Mutex> so a background writer task can forward
    // permission/user-input answers back to the bridge concurrently with the
    // main read loop. Mirrors the pattern in provider-claude-sdk/src/lib.rs.
    stdin: Arc<Mutex<ChildStdin>>,
    stdout: Lines<BufReader<ChildStdout>>,
    next_request_id: u64,
    bridge_session_id: String,
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
    },
    #[serde(rename = "send_prompt")]
    SendPrompt {
        prompt: String,
        permission_mode: String,
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
    },
    #[serde(rename = "list_models")]
    ListModels,
    #[serde(rename = "interrupt")]
    #[allow(dead_code)]
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
        plan_id: Option<String>,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        steps: Option<serde_json::Value>,
        #[serde(default)]
        raw: Option<String>,
    },
}

/// GitHub Copilot Provider Adapter
#[derive(Debug, Clone)]
pub struct GitHubCopilotAdapter {
    working_directory: PathBuf,
    sessions: Arc<Mutex<HashMap<String, Arc<Mutex<CopilotBridgeProcess>>>>>,
}

impl GitHubCopilotAdapter {
    pub fn new(working_directory: PathBuf) -> Self {
        Self {
            working_directory,
            sessions: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Spawn the Node.js bridge process
    async fn spawn_bridge(&self) -> Result<CopilotBridgeProcess, String> {
        info!("Spawning GitHub Copilot bridge process...");

        let exe_path = std::env::current_exe().ok();
        let exe_dir = exe_path.as_ref().and_then(|p| p.parent());
        let out_dir = option_env!("OUT_DIR").map(PathBuf::from);

        let mut bridge_paths = vec![];

        if let Some(ref dir) = out_dir {
            bridge_paths.push(dir.join("copilot-bridge.js"));
        }

        bridge_paths.push(PathBuf::from("bridge/dist/index.js"));
        bridge_paths.push(PathBuf::from("crates/provider-github-copilot/bridge/dist/index.js"));
        bridge_paths.push(PathBuf::from("../crates/provider-github-copilot/bridge/dist/index.js"));
        bridge_paths.push(PathBuf::from(
            "../../crates/provider-github-copilot/bridge/dist/index.js",
        ));

        if let Some(dir) = exe_dir {
            bridge_paths.push(dir.join("copilot-bridge.js"));
            bridge_paths.push(dir.join("bridge/dist/index.js"));
            bridge_paths.push(dir.join("crates/provider-github-copilot/bridge/dist/index.js"));
            bridge_paths.push(dir.join("../crates/provider-github-copilot/bridge/dist/index.js"));
        }

        bridge_paths.push(PathBuf::from("/usr/share/zenui/copilot-bridge/dist/index.js"));

        let bridge_path = bridge_paths
            .iter()
            .find(|p| p.exists())
            .cloned()
            .ok_or_else(|| {
                "Copilot bridge not found. Searched in: ".to_string()
                    + &bridge_paths
                        .iter()
                        .map(|p| p.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
            })?;

        info!("Using bridge at: {}", bridge_path.display());

        let out_dir = option_env!("OUT_DIR").map(PathBuf::from);
        let exe_dir = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|p| p.to_path_buf()));

        let mut node_paths: Vec<PathBuf> = vec![];

        if let Some(ref dir) = out_dir {
            node_paths.push(dir.join("node/bin/node"));
        }
        if let Some(ref dir) = exe_dir {
            node_paths.push(dir.join("node/bin/node"));
        }

        node_paths.push(PathBuf::from("/opt/homebrew/bin/node"));
        node_paths.push(PathBuf::from("/usr/local/bin/node"));
        node_paths.push(PathBuf::from("/usr/bin/node"));
        node_paths.push(PathBuf::from("node"));

        let node_path = node_paths
            .iter()
            .find(|p| p.to_string_lossy() == "node" || p.exists())
            .cloned()
            .ok_or_else(|| "Node.js not found. Install from https://nodejs.org".to_string())?;

        let bridge_dir = bridge_path.parent().unwrap_or(&self.working_directory);
        let bridge_file = bridge_path
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| "Invalid bridge path".to_string())?;

        let mut child = Command::new(node_path)
            .arg(bridge_file)
            .current_dir(bridge_dir)
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
            next_request_id: 1,
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
        events: &TurnEventSink,
    ) -> Result<String, String> {
        write_request(
            &process.stdin,
            &BridgeRequest::SendPrompt {
                prompt,
                permission_mode: permission_mode_to_str(permission_mode).to_string(),
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
        let (q_tx, mut q_rx) = tokio::sync::mpsc::unbounded_channel::<(String, String)>();
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
                    Some((request_id, answer)) = q_rx.recv() => {
                        let req = BridgeRequest::AnswerUserInput { request_id, answer };
                        if let Err(e) = write_request(&stdin_for_writer, &req).await {
                            debug!("failed to forward user input answer: {e}");
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
                    plan_id,
                    title,
                    steps,
                    raw,
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

                        let events_clone = events.clone();
                        let perm_tx = perm_tx.clone();
                        let req_id_for_writer = request_id.clone();
                        let tool_name_clone = tool_name.clone();
                        let input_clone = input.clone();
                        tokio::spawn(async move {
                            let decision = events_clone
                                .request_permission(tool_name_clone, input_clone, suggested)
                                .await;
                            let _ = perm_tx.send((req_id_for_writer, decision));
                        });
                        events
                            .send(ProviderTurnEvent::PermissionRequest {
                                request_id,
                                tool_name,
                                input,
                                suggested_decision: suggested,
                            })
                            .await;
                    }
                    "user_question" => {
                        let request_id = request_id.unwrap_or_default();
                        let question = question.unwrap_or_default();
                        let events_clone = events.clone();
                        let q_tx = q_tx.clone();
                        let req_id_for_writer = request_id.clone();
                        let question_clone = question.clone();
                        tokio::spawn(async move {
                            if let Some(answer) = events_clone.ask_user(question_clone).await {
                                let _ = q_tx.send((req_id_for_writer, answer));
                            }
                        });
                        events
                            .send(ProviderTurnEvent::UserQuestion {
                                request_id,
                                question,
                            })
                            .await;
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
    ) -> Result<Arc<Mutex<CopilotBridgeProcess>>, String> {
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

        let response = self
            .bridge_request(
                &mut bridge,
                BridgeRequest::CreateSession {
                    cwd: self.working_directory.display().to_string(),
                    model: session.summary.model.clone(),
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

        let bridge = Arc::new(Mutex::new(bridge));
        let mut sessions = self.sessions.lock().await;
        Ok(sessions
            .entry(session.summary.session_id.clone())
            .or_insert_with(|| bridge.clone())
            .clone())
    }

    /// Remove a session's bridge from the cache and kill its process.
    async fn invalidate_session(&self, session_id: &str) {
        let process = self.sessions.lock().await.remove(session_id);
        if let Some(process) = process {
            let mut process = process.lock().await;
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

        let out_dir = option_env!("OUT_DIR").map(PathBuf::from);
        let exe_dir = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|p| p.to_path_buf()));

        let mut node_paths: Vec<PathBuf> = vec![];

        if let Some(ref dir) = out_dir {
            node_paths.push(dir.join("node/bin/node"));
        }
        if let Some(ref dir) = exe_dir {
            node_paths.push(dir.join("node/bin/node"));
        }

        node_paths.push(PathBuf::from("/opt/homebrew/bin/node"));
        node_paths.push(PathBuf::from("/usr/local/bin/node"));
        node_paths.push(PathBuf::from("/usr/bin/node"));

        let node_found = node_paths.iter().find(|p| p.exists());

        if node_found.is_none() {
            if let Ok(output) = Command::new("which").arg("node").output().await {
                if !output.status.success() {
                    return ProviderStatus {
                        kind,
                        label: label.to_string(),
                        installed: false,
                        authenticated: false,
                        version: None,
                        status: ProviderStatusLevel::Error,
                        message: Some(
                            "Node.js not found. Install from https://nodejs.org".to_string(),
                        ),
                        models: copilot_models(),
                    };
                }
            }
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
            },
        }
    }

    async fn start_session(
        &self,
        session: &SessionDetail,
    ) -> Result<Option<ProviderSessionState>, String> {
        info!("Starting GitHub Copilot session...");

        let process = self.ensure_session_process(session).await?;
        let process = process.lock().await;

        Ok(Some(ProviderSessionState {
            native_thread_id: Some(process.bridge_session_id.clone()),
            metadata: None,
        }))
    }

    async fn execute_turn(
        &self,
        session: &SessionDetail,
        input: &str,
        permission_mode: PermissionMode,
        events: TurnEventSink,
    ) -> Result<ProviderTurnOutput, String> {
        info!("Executing turn with GitHub Copilot (mode={:?})", permission_mode);

        let process = self.ensure_session_process(session).await?;
        let result = {
            let mut process = process.lock().await;
            let output = self
                .bridge_request_streaming(
                    &mut process,
                    input.to_string(),
                    permission_mode,
                    &events,
                )
                .await?;
            Ok(ProviderTurnOutput {
                output,
                provider_state: session.provider_state.clone(),
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

        // TODO: interrupt is not functional — the TS bridge's readline loop (bridge/src/index.ts)
        // awaits sendPrompt() inline and cannot receive an interrupt message while a prompt is in
        // flight. Fixing this requires making that loop handle new stdin messages concurrently.
        Ok(format!(
            "GitHub Copilot interrupt requested for session '{}'.",
            session.summary.title
        ))
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
    vec![
        ProviderModel {
            value: "gpt-4.1".to_string(),
            label: "GPT-4.1".to_string(),
        },
        ProviderModel {
            value: "gpt-4o".to_string(),
            label: "GPT-4o".to_string(),
        },
        ProviderModel {
            value: "gpt-5".to_string(),
            label: "GPT-5".to_string(),
        },
        ProviderModel {
            value: "claude-sonnet-4-5".to_string(),
            label: "Claude Sonnet 4.5".to_string(),
        },
    ]
}
