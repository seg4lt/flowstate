mod bridge_runtime;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{Mutex, mpsc};
use tracing::{debug, info, warn};
use zenui_provider_api::{
    PermissionDecision, PermissionMode, ProviderAdapter, ProviderKind, ProviderModel,
    ProviderSessionState, ProviderStatus, ProviderStatusLevel, ProviderTurnEvent,
    ProviderTurnOutput, ReasoningEffort, SessionDetail, TurnEventSink, UserInputAnswer,
    UserInputOption, UserInputQuestion,
};

const BRIDGE_TIMEOUT_MS: u64 = 600_000;

fn session_cwd(session: &SessionDetail, fallback: &Path) -> PathBuf {
    session
        .cwd
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(|| fallback.to_path_buf())
}

/// Result of asking the user a question: either they answered or dismissed.
/// Carried over the writer-task channel so the bridge can be told which
/// BridgeRequest to emit.
enum QuestionOutcome {
    Answered(Vec<UserInputAnswer>),
    Cancelled,
}

#[derive(Debug)]
struct ClaudeBridgeProcess {
    child: Child,
    stdin: Arc<Mutex<ChildStdin>>,
    stdout: Lines<BufReader<ChildStdout>>,
    bridge_session_id: String,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type")]
enum BridgeRequest {
    #[serde(rename = "create_session")]
    CreateSession {
        cwd: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        /// Persisted Claude SDK session id from a prior turn. When present the
        /// bridge hydrates `resumeSessionId` before the next send_prompt, so a
        /// zenui restart or bridge crash recovers the conversation.
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
    #[serde(rename = "answer_question")]
    AnswerQuestion {
        request_id: String,
        answers: Vec<UserInputAnswer>,
    },
    #[serde(rename = "cancel_question")]
    CancelQuestion { request_id: String },
    #[serde(rename = "list_models")]
    ListModels,
    #[serde(rename = "interrupt")]
    Interrupt,
}

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
    Response {
        output: String,
        /// Claude SDK session id captured from the init/result messages in the
        /// bridge. Round-tripped back to the Rust side so we can persist it on
        /// `session.provider_state.native_thread_id` and resume on the next turn.
        #[serde(default)]
        session_id: Option<String>,
    },
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
        args: Option<Value>,
        #[serde(default)]
        output: Option<String>,
        #[serde(default)]
        error: Option<String>,
        #[serde(default)]
        message: Option<String>,
        #[serde(default)]
        request_id: Option<String>,
        #[serde(default)]
        tool_name: Option<String>,
        #[serde(default)]
        input: Option<Value>,
        #[serde(default)]
        suggested: Option<String>,
        #[serde(default)]
        question: Option<String>,
        #[serde(default)]
        questions: Option<Value>,
        #[serde(default)]
        path: Option<String>,
        #[serde(default)]
        operation: Option<String>,
        #[serde(default)]
        before: Option<String>,
        #[serde(default)]
        after: Option<String>,
        #[serde(default)]
        parent_call_id: Option<String>,
        #[serde(default)]
        agent_id: Option<String>,
        #[serde(default)]
        agent_type: Option<String>,
        #[serde(default)]
        prompt: Option<String>,
        #[serde(default)]
        plan_id: Option<String>,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        steps: Option<Value>,
        #[serde(default)]
        raw: Option<String>,
        #[serde(default)]
        nested_event: Option<Value>,
    },
}

#[derive(Debug, Clone)]
pub struct ClaudeSdkAdapter {
    working_directory: PathBuf,
    sessions: Arc<Mutex<HashMap<String, Arc<Mutex<ClaudeBridgeProcess>>>>>,
}

impl ClaudeSdkAdapter {
    pub fn new(working_directory: PathBuf) -> Self {
        Self {
            working_directory,
            sessions: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    async fn spawn_bridge(&self) -> Result<ClaudeBridgeProcess, String> {
        info!("Spawning Claude SDK bridge process...");

        let node = zenui_embedded_node::ensure_extracted()
            .map_err(|e| format!("embedded Node.js setup failed: {e:?}"))?;
        let bridge = bridge_runtime::ensure_extracted()
            .map_err(|e| format!("Claude SDK bridge extraction failed: {e:?}"))?;

        info!("Using bridge at: {}", bridge.script.display());
        info!("Using embedded node at: {}", node.node_bin.display());

        // The Claude Agent SDK spawns a child `node` process internally,
        // so the embedded node's directory must be on PATH or the SDK
        // fails with ENOENT when it tries to re-exec itself.
        let existing_path = std::env::var("PATH").unwrap_or_default();
        let new_path = if existing_path.is_empty() {
            node.bin_dir.to_string_lossy().into_owned()
        } else {
            format!("{}:{}", node.bin_dir.display(), existing_path)
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
                        info!(target: "claude-sdk-bridge", "{}", line);
                    }
                }
            });
        }

        let mut process = ClaudeBridgeProcess {
            child,
            stdin: Arc::new(Mutex::new(stdin)),
            stdout: BufReader::new(stdout).lines(),
            bridge_session_id: String::new(),
        };

        debug!("Waiting for bridge ready signal...");
        match tokio::time::timeout(
            std::time::Duration::from_secs(15),
            process.read_response(),
        )
        .await
        {
            Ok(Ok(BridgeResponse::Ready)) => {
                info!("Claude SDK bridge is ready");
            }
            Ok(Ok(other)) => {
                return Err(format!("Expected ready signal, got: {:?}", other));
            }
            Ok(Err(e)) => {
                return Err(format!("Failed to read ready signal: {e}"));
            }
            Err(_) => {
                return Err("Timeout waiting for Claude SDK bridge ready signal".to_string());
            }
        }

        Ok(process)
    }

    async fn ensure_session_process(
        &self,
        session: &SessionDetail,
    ) -> Result<Arc<Mutex<ClaudeBridgeProcess>>, String> {
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

        // If the session was previously resumed on disk, pass the persisted
        // Claude SDK session id to the bridge so it can set `resume:` on the
        // first SDK query. This recovers conversation history after a zenui
        // restart or bridge crash.
        let resume_session_id = session
            .provider_state
            .as_ref()
            .and_then(|state| state.native_thread_id.clone());
        let request = BridgeRequest::CreateSession {
            cwd: session_cwd(session, &self.working_directory)
                .display()
                .to_string(),
            model: session.summary.model.clone(),
            resume_session_id,
        };
        write_request(&bridge.stdin, &request).await?;

        let response = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            bridge.read_response(),
        )
        .await
        .map_err(|_| "Timeout creating Claude SDK session".to_string())?
        .map_err(|e| format!("Bridge read error: {e}"))?;

        match response {
            BridgeResponse::SessionCreated { session_id } => {
                info!("Claude SDK session created: {}", session_id);
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

    async fn invalidate_session(&self, session_id: &str) {
        let process = self.sessions.lock().await.remove(session_id);
        if let Some(process) = process {
            let mut process = process.lock().await;
            let _ = process.child.start_kill();
        }
    }

    async fn run_turn(
        &self,
        process: Arc<Mutex<ClaudeBridgeProcess>>,
        prompt: String,
        permission_mode: PermissionMode,
        reasoning_effort: Option<ReasoningEffort>,
        events: TurnEventSink,
    ) -> Result<(String, Option<String>), String> {
        let mut process = process.lock().await;

        let mode_str = permission_mode_to_str(permission_mode);
        let request = BridgeRequest::SendPrompt {
            prompt,
            permission_mode: mode_str.to_string(),
            reasoning_effort: reasoning_effort.map(|e| e.as_str().to_string()),
        };
        write_request(&process.stdin, &request).await?;

        let stdin = process.stdin.clone();
        let (perm_tx, mut perm_rx) = mpsc::unbounded_channel::<(String, PermissionDecision)>();
        let (q_tx, mut q_rx) = mpsc::unbounded_channel::<(String, QuestionOutcome)>();

        // Background task: forwards permission/question answers from the sink helpers back
        // into the bridge stdin while the turn is in flight.
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
                            warn!("failed to forward permission answer: {e}");
                            break;
                        }
                    }
                    Some((request_id, outcome)) = q_rx.recv() => {
                        let req = match outcome {
                            QuestionOutcome::Answered(answers) => {
                                BridgeRequest::AnswerQuestion { request_id, answers }
                            }
                            QuestionOutcome::Cancelled => {
                                BridgeRequest::CancelQuestion { request_id }
                            }
                        };
                        if let Err(e) = write_request(&stdin_for_writer, &req).await {
                            warn!("failed to forward question outcome: {e}");
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
                break Err("Claude SDK turn timed out".to_string());
            }
            let line = match tokio::time::timeout(remaining, process.read_response()).await {
                Ok(Ok(resp)) => resp,
                Ok(Err(e)) => break Err(format!("Bridge read error: {e}")),
                Err(_) => break Err("Claude SDK turn timed out".to_string()),
            };

            match line {
                BridgeResponse::Response { output, session_id } => {
                    break Ok((output, session_id));
                }
                BridgeResponse::Error { error } => break Err(format!("Claude error: {error}")),
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
                    question: _question,
                    questions,
                    path,
                    operation,
                    before,
                    after,
                    parent_call_id,
                    agent_id,
                    agent_type,
                    prompt: subagent_prompt,
                    plan_id,
                    title,
                    steps,
                    raw,
                    nested_event,
                } => match event.as_str() {
                    "permission_request" => {
                        let request_id = request_id.unwrap_or_default();
                        let tool_name = tool_name.unwrap_or_default();
                        let input = input.unwrap_or(Value::Null);
                        let suggested = suggested
                            .as_deref()
                            .map(parse_decision)
                            .unwrap_or(PermissionDecision::Allow);

                        // request_permission() emits its own PermissionRequest event
                        // with an internal `perm-...` id; do NOT duplicate it here. The
                        // writer task still uses the bridge's request_id (`request_id`)
                        // when forwarding the decision back to the bridge, because the
                        // bridge keeps its own pending-permissions map keyed by that id.
                        let events_clone = events.clone();
                        let perm_tx = perm_tx.clone();
                        let req_id_for_writer = request_id;
                        tokio::spawn(async move {
                            let decision = events_clone
                                .request_permission(tool_name, input, suggested)
                                .await;
                            let _ = perm_tx.send((req_id_for_writer, decision));
                        });
                    }
                    "user_question" => {
                        let request_id = request_id.unwrap_or_default();
                        let structured = parse_claude_questions(questions.as_ref());

                        // ask_user() emits its own UserQuestion event with an
                        // internal `q-...` id; do NOT duplicate it here. The
                        // writer task still uses the bridge's `request_id` when
                        // forwarding the answer because the bridge keeps its own
                        // pendingQuestions map keyed by that id.
                        let events_clone = events.clone();
                        let q_tx = q_tx.clone();
                        let req_id_for_writer = request_id;
                        tokio::spawn(async move {
                            let outcome = match events_clone.ask_user(structured).await {
                                Some(answers) => QuestionOutcome::Answered(answers),
                                None => QuestionOutcome::Cancelled,
                            };
                            let _ = q_tx.send((req_id_for_writer, outcome));
                        });
                    }
                    other_event => {
                        forward_stream(
                            &events,
                            other_event,
                            delta,
                            call_id,
                            name,
                            args,
                            output,
                            error,
                            message,
                            path,
                            operation,
                            before,
                            after,
                            parent_call_id,
                            agent_id,
                            agent_type,
                            subagent_prompt,
                            plan_id,
                            title,
                            steps,
                            raw,
                            nested_event,
                        )
                        .await;
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
}

#[async_trait]
impl ProviderAdapter for ClaudeSdkAdapter {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Claude
    }

    async fn health(&self) -> ProviderStatus {
        let kind = ProviderKind::Claude;
        let label = kind.label();

        // The embedded Node.js runtime and SDK bridge both live in the
        // binary itself; the health check just confirms we can extract
        // them to the per-user cache dir. Any filesystem or permission
        // error surfaces here instead of at first turn.
        if let Err(err) = zenui_embedded_node::ensure_extracted() {
            return ProviderStatus {
                kind,
                label: label.to_string(),
                installed: false,
                authenticated: false,
                version: None,
                status: ProviderStatusLevel::Error,
                message: Some(format!("embedded Node.js extraction failed: {err:?}")),
                models: claude_models(),
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
                message: Some(format!("Claude SDK bridge extraction failed: {err:?}")),
                models: claude_models(),
            };
        }

        ProviderStatus {
            kind,
            label: label.to_string(),
            installed: true,
            authenticated: true,
            version: None,
            status: ProviderStatusLevel::Ready,
            message: Some("Claude Agent SDK bridge ready".to_string()),
            models: claude_models(),
        }
    }

    async fn start_session(
        &self,
        _session: &SessionDetail,
    ) -> Result<Option<ProviderSessionState>, String> {
        // Defer the bridge spawn to the first execute_turn (which already
        // calls ensure_session_process). Spawning eagerly here used to add
        // 300-800ms to "create new thread" for no UX benefit — the bridge
        // session id isn't persisted across restarts anyway, since it's a
        // zenui-internal UUID rather than a real Claude SDK resume id.
        Ok(None)
    }

    async fn execute_turn(
        &self,
        session: &SessionDetail,
        input: &str,
        permission_mode: PermissionMode,
        reasoning_effort: Option<ReasoningEffort>,
        events: TurnEventSink,
    ) -> Result<ProviderTurnOutput, String> {
        let process = self.ensure_session_process(session).await?;
        let result = self
            .run_turn(
                process,
                input.to_string(),
                permission_mode,
                reasoning_effort,
                events,
            )
            .await;

        match result {
            Ok((output, session_id)) => {
                // Prefer the freshly-captured session id from this turn so a
                // resume after restart works. Fall back to whatever was already
                // persisted if the bridge didn't return one (e.g. init failed
                // to carry a session_id in this SDK version).
                let provider_state = session_id
                    .map(|id| ProviderSessionState {
                        native_thread_id: Some(id),
                        metadata: None,
                    })
                    .or_else(|| session.provider_state.clone());
                Ok(ProviderTurnOutput {
                    output,
                    provider_state,
                })
            }
            Err(error) => {
                self.invalidate_session(&session.summary.session_id).await;
                Err(error)
            }
        }
    }

    async fn interrupt_turn(&self, session: &SessionDetail) -> Result<String, String> {
        // Look up the live bridge process (if any) and send it an `interrupt`
        // message. The bridge calls `abortController.abort()` on the in-flight
        // SDK query, which returns `'[interrupted]'` and flips `inFlight = false`
        // so the bridge is ready to accept the next send_prompt.
        //
        // We intentionally do NOT invalidate the session — the bridge's in-memory
        // `resumeSessionId` must survive so the next turn resumes the same Claude
        // conversation.
        let process = self
            .sessions
            .lock()
            .await
            .get(&session.summary.session_id)
            .cloned();
        let Some(process) = process else {
            return Ok(format!(
                "Claude SDK interrupt requested for session `{}` (no active bridge).",
                session.summary.session_id
            ));
        };
        let process = process.lock().await;
        write_request(&process.stdin, &BridgeRequest::Interrupt).await?;
        Ok(format!(
            "Claude SDK turn interrupted for session `{}`.",
            session.summary.session_id
        ))
    }

    async fn end_session(&self, session: &SessionDetail) -> Result<(), String> {
        self.invalidate_session(&session.summary.session_id).await;
        Ok(())
    }

    async fn fetch_models(&self) -> Result<Vec<ProviderModel>, String> {
        // Spawn an ephemeral bridge process, ask it for the model list, kill it.
        // The bridge calls query() with a noop prompt and reads supportedModels()
        // off the init response — no actual SDK call is made.
        let mut bridge = self.spawn_bridge().await?;
        write_request(&bridge.stdin, &BridgeRequest::ListModels).await?;
        let response = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            bridge.read_response(),
        )
        .await
        .map_err(|_| "Timeout fetching Claude models".to_string())?
        .map_err(|e| format!("Bridge read error: {e}"))?;
        let _ = bridge.child.start_kill();

        match response {
            BridgeResponse::Models { models } => {
                if models.is_empty() {
                    Err("Claude bridge returned no models".to_string())
                } else {
                    Ok(models)
                }
            }
            BridgeResponse::Error { error } => Err(format!("Claude list_models error: {error}")),
            other => Err(format!(
                "Unexpected bridge response for list_models: {:?}",
                other
            )),
        }
    }
}

impl ClaudeBridgeProcess {
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
        PermissionMode::AcceptEdits => "acceptEdits",
        PermissionMode::Plan => "plan",
        PermissionMode::Bypass => "bypassPermissions",
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

/// Parse Claude SDK's `AskUserQuestion` tool input into zenui's cross-provider
/// question list. Claude's shape is
/// `{ questions: [{ question, header, options: [{label, description}], multiSelect }] }`,
/// per https://code.claude.com/docs/en/agent-sdk/user-input. We synthesize `id` as
/// the question's array index so `answerQuestion` in the bridge can map answers
/// back to the original question text (Claude's answer map is keyed by question text).
fn parse_claude_questions(raw: Option<&Value>) -> Vec<UserInputQuestion> {
    let Some(array) = raw.and_then(Value::as_array) else {
        return Vec::new();
    };
    array
        .iter()
        .enumerate()
        .map(|(i, q)| {
            let options = q
                .get("options")
                .and_then(Value::as_array)
                .map(|opts| {
                    opts.iter()
                        .enumerate()
                        .map(|(j, o)| UserInputOption {
                            id: j.to_string(),
                            label: o
                                .get("label")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string(),
                            description: o
                                .get("description")
                                .and_then(Value::as_str)
                                .map(str::to_string),
                        })
                        .collect()
                })
                .unwrap_or_default();
            UserInputQuestion {
                id: i.to_string(),
                text: q
                    .get("question")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                header: q.get("header").and_then(Value::as_str).map(str::to_string),
                options,
                multi_select: q
                    .get("multiSelect")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                // Claude docs: no explicit allowFreeform flag; the client may always
                // accept a free-form answer by passing the user's typed text as the value.
                allow_freeform: true,
                is_secret: false,
            }
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
async fn forward_stream(
    events: &TurnEventSink,
    event: &str,
    delta: Option<String>,
    call_id: Option<String>,
    name: Option<String>,
    args: Option<Value>,
    output: Option<String>,
    error: Option<String>,
    message: Option<String>,
    path: Option<String>,
    operation: Option<String>,
    before: Option<String>,
    after: Option<String>,
    parent_call_id: Option<String>,
    agent_id: Option<String>,
    agent_type: Option<String>,
    prompt: Option<String>,
    plan_id: Option<String>,
    title: Option<String>,
    steps: Option<Value>,
    raw: Option<String>,
    nested_event: Option<Value>,
) {
    use zenui_provider_api::{FileOperation, PlanStep};
    match event {
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
                        args: args.unwrap_or(Value::Null),
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
        "file_change" => {
            if let (Some(cid), Some(path), Some(op)) = (call_id, path, operation) {
                let operation = match op.as_str() {
                    "edit" => FileOperation::Edit,
                    "delete" => FileOperation::Delete,
                    _ => FileOperation::Write,
                };
                events
                    .send(ProviderTurnEvent::FileChange {
                        call_id: cid,
                        path,
                        operation,
                        before,
                        after,
                    })
                    .await;
            }
        }
        "subagent_started" => {
            if let (Some(parent_id), Some(aid), Some(atype)) =
                (parent_call_id, agent_id, agent_type)
            {
                events
                    .send(ProviderTurnEvent::SubagentStarted {
                        parent_call_id: parent_id,
                        agent_id: aid,
                        agent_type: atype,
                        prompt: prompt.unwrap_or_default(),
                    })
                    .await;
            }
        }
        "subagent_event" => {
            if let Some(aid) = agent_id {
                events
                    .send(ProviderTurnEvent::SubagentEvent {
                        agent_id: aid,
                        event: nested_event.unwrap_or(Value::Null),
                    })
                    .await;
            }
        }
        "subagent_completed" => {
            if let Some(aid) = agent_id {
                events
                    .send(ProviderTurnEvent::SubagentCompleted {
                        agent_id: aid,
                        output: output.unwrap_or_default(),
                        error,
                    })
                    .await;
            }
        }
        "plan_proposed" => {
            if let (Some(pid), Some(t)) = (plan_id, title) {
                let parsed_steps: Vec<PlanStep> = steps
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
        "info" | "warning" => {
            if let Some(msg) = message {
                events.send(ProviderTurnEvent::Info { message: msg }).await;
            }
        }
        _ => {
            debug!("Unknown bridge stream event: {event}");
        }
    }
}

fn claude_models() -> Vec<ProviderModel> {
    vec![
        ProviderModel {
            value: "sonnet".to_string(),
            label: "Claude Sonnet".to_string(),
        },
        ProviderModel {
            value: "opus".to_string(),
            label: "Claude Opus".to_string(),
        },
        ProviderModel {
            value: "haiku".to_string(),
            label: "Claude Haiku".to_string(),
        },
    ]
}
