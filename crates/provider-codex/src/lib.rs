use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tracing::warn;
use zenui_provider_api::{
    PermissionMode, ProviderAdapter, ProviderKind, ProviderSessionState, ProviderStatus,
    ProviderStatusLevel, ProviderTurnEvent, ProviderTurnOutput, SessionDetail, TurnEventSink,
};

const REQUEST_TIMEOUT_MS: u64 = 20_000;
const RECOVERABLE_THREAD_RESUME_ERRORS: &[&str] = &[
    "not found",
    "missing thread",
    "no such thread",
    "unknown thread",
    "does not exist",
];

#[derive(Debug, Clone)]
pub struct CodexAdapter {
    binary_path: String,
    working_directory: PathBuf,
    sessions: Arc<Mutex<HashMap<String, Arc<Mutex<CodexSessionProcess>>>>>,
}

#[derive(Debug)]
struct CodexSessionProcess {
    child: Child,
    stdin: ChildStdin,
    stdout: Lines<BufReader<ChildStdout>>,
    stderr_task: JoinHandle<()>,
    next_request_id: u64,
    provider_thread_id: String,
}

#[derive(Debug)]
enum ProtocolMessage {
    Response {
        id: String,
        result: Value,
    },
    ResponseError {
        id: String,
        message: String,
    },
    Notification {
        method: String,
        params: Value,
    },
    ServerRequest {
        id: Value,
        method: String,
    },
}

#[derive(Debug)]
struct TurnCompletion {
    status: String,
    error_message: Option<String>,
}

impl CodexAdapter {
    pub fn new(working_directory: PathBuf) -> Self {
        Self {
            binary_path: "codex".to_string(),
            working_directory,
            sessions: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    async fn ensure_session_process(
        &self,
        session: &SessionDetail,
    ) -> Result<Arc<Mutex<CodexSessionProcess>>, String> {
        if let Some(existing) = self
            .sessions
            .lock()
            .await
            .get(&session.summary.session_id)
            .cloned()
        {
            return Ok(existing);
        }

        let process = self.create_session_process(session).await?;
        let process = Arc::new(Mutex::new(process));
        let mut sessions = self.sessions.lock().await;
        Ok(sessions
            .entry(session.summary.session_id.clone())
            .or_insert_with(|| process.clone())
            .clone())
    }

    async fn create_session_process(
        &self,
        session: &SessionDetail,
    ) -> Result<CodexSessionProcess, String> {
        let mut child = Command::new(&self.binary_path)
            .arg("app-server")
            .current_dir(&self.working_directory)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| format!("failed to launch Codex app-server: {error}"))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "Codex app-server stdin is unavailable.".to_string())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "Codex app-server stdout is unavailable.".to_string())?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| "Codex app-server stderr is unavailable.".to_string())?;

        let stderr_task = spawn_stderr_drain(stderr);
        let mut process = CodexSessionProcess {
            child,
            stdin,
            stdout: BufReader::new(stdout).lines(),
            stderr_task,
            next_request_id: 1,
            provider_thread_id: String::new(),
        };

        process
            .send_request("initialize", initialize_params())
            .await?;
        process.send_notification("initialized").await?;

        let resume_thread_id = session
            .provider_state
            .as_ref()
            .and_then(|state| state.native_thread_id.as_deref());

        let mut base_params = json!({
            "approvalPolicy": "never",
            "cwd": self.working_directory.display().to_string(),
            "personality": "pragmatic",
            "sandbox": "workspace-write",
        });
        if let Some(model) = session.summary.model.as_deref() {
            if let Some(obj) = base_params.as_object_mut() {
                obj.insert("model".to_string(), Value::String(model.to_string()));
            }
        }

        let thread_result = if let Some(thread_id) = resume_thread_id {
            match process
                .send_request(
                    "thread/resume",
                    merge_object(base_params.clone(), json!({ "threadId": thread_id })),
                )
                .await
            {
                Ok(response) => response,
                Err(error) if is_recoverable_thread_resume_error(&error) => {
                    process.send_request("thread/start", base_params).await?
                }
                Err(error) => return Err(error),
            }
        } else {
            process.send_request("thread/start", base_params).await?
        };

        process.provider_thread_id = extract_thread_id(&thread_result)
            .ok_or_else(|| "Codex thread start did not return a thread id.".to_string())?;

        Ok(process)
    }

    async fn invalidate_session(&self, session_id: &str) {
        let process = self.sessions.lock().await.remove(session_id);
        if let Some(process) = process {
            let mut process = process.lock().await;
            process.stderr_task.abort();
            let _ = process.child.start_kill();
        }
    }
}

#[async_trait]
impl ProviderAdapter for CodexAdapter {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Codex
    }

    async fn health(&self) -> ProviderStatus {
        probe_cli(
            &self.binary_path,
            ProviderKind::Codex,
            &["--version"],
            &["login", "status"],
        )
        .await
    }

    async fn start_session(
        &self,
        session: &SessionDetail,
    ) -> Result<Option<ProviderSessionState>, String> {
        let process = self.ensure_session_process(session).await?;
        let process = process.lock().await;
        Ok(Some(provider_state(process.provider_thread_id.clone())))
    }

    async fn execute_turn(
        &self,
        session: &SessionDetail,
        input: &str,
        permission_mode: PermissionMode,
        events: TurnEventSink,
    ) -> Result<ProviderTurnOutput, String> {
        let _ = permission_mode;
        let process = self.ensure_session_process(session).await?;
        let result = {
            let mut process = process.lock().await;
            let provider_thread_id = process.provider_thread_id.clone();
            let response = process
                .send_request(
                    "turn/start",
                    json!({
                        "input": [{
                            "text": input,
                            "text_elements": [],
                            "type": "text",
                        }],
                        "threadId": provider_thread_id,
                    }),
                )
                .await?;
            let turn_id = extract_turn_id(&response)
                .ok_or_else(|| "Codex turn start did not return a turn id.".to_string())?;
            let completion = process.wait_for_turn_completion(&turn_id, &events).await?;
            match completion.status.as_str() {
                "completed" => {
                    let output = process.read_turn_output(&turn_id).await?;
                    Ok(ProviderTurnOutput {
                        output: if output.trim().is_empty() {
                            "Codex completed without returning text output.".to_string()
                        } else {
                            output
                        },
                        provider_state: Some(provider_state(process.provider_thread_id.clone())),
                    })
                }
                "interrupted" => Ok(ProviderTurnOutput {
                    output: "Codex turn interrupted.".to_string(),
                    provider_state: Some(provider_state(process.provider_thread_id.clone())),
                }),
                _ => Err(completion
                    .error_message
                    .unwrap_or_else(|| "Codex turn failed.".to_string())),
            }
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

    async fn interrupt_turn(&self, session: &SessionDetail) -> Result<String, String> {
        let process = self
            .sessions
            .lock()
            .await
            .get(&session.summary.session_id)
            .cloned();

        let Some(process) = process else {
            return Ok(format!(
                "Codex interrupt requested for session `{}`.",
                session.summary.title
            ));
        };

        let running_turn_id = session
            .turns
            .iter()
            .rev()
            .find(|turn| turn.status == zenui_provider_api::TurnStatus::Running)
            .map(|turn| turn.turn_id.clone());

        let Some(turn_id) = running_turn_id else {
            return Ok(format!(
                "Codex interrupt requested for session `{}`.",
                session.summary.title
            ));
        };

        let mut process = process.lock().await;
        let provider_thread_id = process.provider_thread_id.clone();
        process
            .send_request(
                "turn/interrupt",
                json!({
                    "threadId": provider_thread_id,
                    "turnId": turn_id,
                }),
            )
            .await?;

        Ok(format!(
            "Codex interrupt requested for session `{}`.",
            session.summary.title
        ))
    }
}

impl CodexSessionProcess {
    async fn send_request(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let request_id = self.next_request_id;
        self.next_request_id += 1;
        self.write_message(json!({
            "id": request_id,
            "method": method,
            "params": params,
        }))
        .await?;

        let request_id = request_id.to_string();
        let response = tokio::time::timeout(
            std::time::Duration::from_millis(REQUEST_TIMEOUT_MS),
            async {
                loop {
                    match self.read_message().await? {
                        ProtocolMessage::Response { id, result } if id == request_id => {
                            return Ok(result);
                        }
                        ProtocolMessage::ResponseError { id, message } if id == request_id => {
                            return Err(format!("{method} failed: {message}"));
                        }
                        ProtocolMessage::ServerRequest { id, method } => {
                            self.respond_unsupported(id, &method).await?;
                        }
                        ProtocolMessage::Notification { .. }
                        | ProtocolMessage::Response { .. }
                        | ProtocolMessage::ResponseError { .. } => {}
                    }
                }
            },
        )
        .await
        .map_err(|_| format!("Timed out waiting for {method}."))??;

        Ok(response)
    }

    async fn send_notification(&mut self, method: &str) -> Result<(), String> {
        self.write_message(json!({ "method": method })).await
    }

    async fn wait_for_turn_completion(
        &mut self,
        turn_id: &str,
        events: &TurnEventSink,
    ) -> Result<TurnCompletion, String> {
        let mut last_agent_text_len = 0usize;
        let mut last_reasoning_len = 0usize;
        loop {
            match self.read_message().await? {
                ProtocolMessage::Notification { method, params }
                    if method == "turn/completed"
                        && notification_turn_id(&params).as_deref() == Some(turn_id) =>
                {
                    return Ok(TurnCompletion {
                        status: notification_turn_status(&params)
                            .unwrap_or_else(|| "failed".to_string()),
                        error_message: notification_turn_error(&params),
                    });
                }
                ProtocolMessage::Notification { method, params } => {
                    tracing::warn!(method = %method, params = %params, "codex: notification received");
                    map_codex_notification(
                        &method,
                        &params,
                        &mut last_agent_text_len,
                        &mut last_reasoning_len,
                        events,
                    )
                    .await;
                }
                ProtocolMessage::ServerRequest { id, method } => {
                    self.respond_unsupported(id, &method).await?;
                }
                ProtocolMessage::Response { .. } | ProtocolMessage::ResponseError { .. } => {}
            }
        }
    }

    async fn read_turn_output(&mut self, turn_id: &str) -> Result<String, String> {
        let response = self
            .send_request(
                "thread/read",
                json!({
                    "includeTurns": true,
                    "threadId": self.provider_thread_id.clone(),
                }),
            )
            .await?;

        let turns = response
            .get("thread")
            .and_then(|thread| thread.get("turns"))
            .and_then(Value::as_array)
            .ok_or_else(|| "Codex thread/read did not include turns.".to_string())?;

        let Some(turn) = turns.iter().find(|turn| {
            turn.get("id")
                .and_then(Value::as_str)
                .map(|candidate| candidate == turn_id)
                .unwrap_or(false)
        }) else {
            return Err("Codex thread/read did not include the completed turn.".to_string());
        };

        let items = turn
            .get("items")
            .and_then(Value::as_array)
            .ok_or_else(|| "Codex thread/read did not include turn items.".to_string())?;

        let output = items
            .iter()
            .filter_map(|item| {
                if item.get("type").and_then(Value::as_str) == Some("agentMessage") {
                    item.get("text").and_then(Value::as_str).map(str::trim)
                } else {
                    None
                }
            })
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n");

        Ok(output)
    }

    async fn respond_unsupported(&mut self, id: Value, method: &str) -> Result<(), String> {
        self.write_message(json!({
            "error": {
                "code": -32601,
                "message": format!("Unsupported server request: {method}"),
            },
            "id": id,
        }))
        .await
    }

    async fn write_message(&mut self, value: Value) -> Result<(), String> {
        let encoded =
            serde_json::to_string(&value).map_err(|error| format!("invalid JSON payload: {error}"))?;
        self.stdin
            .write_all(encoded.as_bytes())
            .await
            .map_err(|error| format!("failed to write to Codex app-server stdin: {error}"))?;
        self.stdin
            .write_all(b"\n")
            .await
            .map_err(|error| format!("failed to write to Codex app-server stdin: {error}"))?;
        self.stdin
            .flush()
            .await
            .map_err(|error| format!("failed to flush Codex app-server stdin: {error}"))
    }

    async fn read_message(&mut self) -> Result<ProtocolMessage, String> {
        loop {
            let line = self
                .stdout
                .next_line()
                .await
                .map_err(|error| format!("failed to read Codex app-server stdout: {error}"))?
                .ok_or_else(|| "Codex app-server exited unexpectedly.".to_string())?;

            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            let value: Value = serde_json::from_str(trimmed).map_err(|error| {
                format!("received invalid JSON from Codex app-server: {error}")
            })?;

            if let Some(method) = value.get("method").and_then(Value::as_str) {
                if value.get("id").is_some() {
                    return Ok(ProtocolMessage::ServerRequest {
                        id: value.get("id").cloned().unwrap_or(Value::Null),
                        method: method.to_string(),
                    });
                }

                return Ok(ProtocolMessage::Notification {
                    method: method.to_string(),
                    params: value.get("params").cloned().unwrap_or(Value::Null),
                });
            }

            if let Some(id) = value.get("id") {
                let id = normalize_id(id);
                if let Some(message) = value
                    .get("error")
                    .and_then(|error| error.get("message"))
                    .and_then(Value::as_str)
                {
                    return Ok(ProtocolMessage::ResponseError {
                        id,
                        message: message.to_string(),
                    });
                }

                return Ok(ProtocolMessage::Response {
                    id,
                    result: value.get("result").cloned().unwrap_or(Value::Null),
                });
            }
        }
    }
}

async fn probe_cli(
    binary: &str,
    kind: ProviderKind,
    version_args: &[&str],
    auth_args: &[&str],
) -> ProviderStatus {
    let label = kind.label();
    match Command::new(binary).args(version_args).output().await {
        Ok(version_output) => {
            let version = first_non_empty_line(&version_output.stdout)
                .or_else(|| first_non_empty_line(&version_output.stderr));

            match Command::new(binary).args(auth_args).output().await {
                Ok(auth_output) => {
                    let authenticated = auth_output.status.success();
                    let message = if authenticated {
                        Some(format!("{label} CLI is installed and authenticated."))
                    } else {
                        first_non_empty_line(&auth_output.stderr)
                            .or_else(|| first_non_empty_line(&auth_output.stdout))
                            .or_else(|| {
                                Some(format!("{label} CLI is installed but not authenticated."))
                            })
                    };

                    ProviderStatus {
                        kind,
                        label: label.to_string(),
                        installed: true,
                        authenticated,
                        version,
                        status: if authenticated {
                            ProviderStatusLevel::Ready
                        } else {
                            ProviderStatusLevel::Warning
                        },
                        message,
                    }
                }
                Err(error) => ProviderStatus {
                    kind,
                    label: label.to_string(),
                    installed: true,
                    authenticated: false,
                    version,
                    status: ProviderStatusLevel::Warning,
                    message: Some(format!(
                        "{label} CLI is installed, but auth probing failed: {error}"
                    )),
                },
            }
        }
        Err(error) => ProviderStatus {
            kind,
            label: label.to_string(),
            installed: false,
            authenticated: false,
            version: None,
            status: ProviderStatusLevel::Error,
            message: Some(format!("{label} CLI is unavailable: {error}")),
        },
    }
}

fn initialize_params() -> Value {
    json!({
        "capabilities": {
            "experimentalApi": true,
        },
        "clientInfo": {
            "name": "zenui",
            "title": "ZenUI",
            "version": env!("CARGO_PKG_VERSION"),
        },
    })
}

fn provider_state(native_thread_id: String) -> ProviderSessionState {
    ProviderSessionState {
        native_thread_id: Some(native_thread_id),
        metadata: None,
    }
}

fn merge_object(base: Value, extra: Value) -> Value {
    let mut merged = base.as_object().cloned().unwrap_or_default();
    if let Some(extra) = extra.as_object() {
        merged.extend(extra.clone());
    }
    Value::Object(merged)
}

fn normalize_id(value: &Value) -> String {
    match value {
        Value::Number(number) => number.to_string(),
        Value::String(text) => text.clone(),
        _ => value.to_string(),
    }
}

fn extract_thread_id(value: &Value) -> Option<String> {
    value.get("thread")
        .and_then(|thread| thread.get("id"))
        .and_then(Value::as_str)
        .or_else(|| value.get("threadId").and_then(Value::as_str))
        .map(ToOwned::to_owned)
}

fn extract_turn_id(value: &Value) -> Option<String> {
    value.get("turn")
        .and_then(|turn| turn.get("id"))
        .and_then(Value::as_str)
        .or_else(|| value.get("turnId").and_then(Value::as_str))
        .map(ToOwned::to_owned)
}

fn notification_turn_id(value: &Value) -> Option<String> {
    value.get("turn")
        .and_then(|turn| turn.get("id"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

fn notification_turn_status(value: &Value) -> Option<String> {
    value.get("turn")
        .and_then(|turn| turn.get("status"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

fn notification_turn_error(value: &Value) -> Option<String> {
    value.get("turn")
        .and_then(|turn| turn.get("error"))
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

fn is_recoverable_thread_resume_error(error: &str) -> bool {
    let lower = error.to_ascii_lowercase();
    RECOVERABLE_THREAD_RESUME_ERRORS
        .iter()
        .any(|snippet| lower.contains(snippet))
}

fn spawn_stderr_drain(stderr: ChildStderr) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut stderr = BufReader::new(stderr).lines();
        loop {
            match stderr.next_line().await {
                Ok(Some(line)) if !line.trim().is_empty() => {
                    warn!("codex app-server stderr: {line}");
                }
                Ok(Some(_)) => {}
                Ok(None) => break,
                Err(error) => {
                    warn!("failed to read codex app-server stderr: {error}");
                    break;
                }
            }
        }
    })
}

/// Map a codex app-server notification into a streaming event and forward it.
/// Handles text accumulation: codex sends the growing full text on each update, so we diff
/// against the previous length to produce a true delta.
async fn map_codex_notification(
    method: &str,
    params: &Value,
    last_agent_text_len: &mut usize,
    last_reasoning_len: &mut usize,
    events: &TurnEventSink,
) {
    match method {
        // Item-level streaming updates: codex sends the full accumulated text each time.
        "item/update" | "item/append" | "item/created" | "item/updated" => {
            let item = match params.get("item") {
                Some(v) => v,
                None => return,
            };
            let item_type = item.get("type").and_then(Value::as_str).unwrap_or("");
            match item_type {
                "agentMessage" | "agent_message" => {
                    let text = item
                        .get("text")
                        .or_else(|| item.get("content"))
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    if text.len() > *last_agent_text_len {
                        let delta = text[*last_agent_text_len..].to_string();
                        *last_agent_text_len = text.len();
                        events.send(ProviderTurnEvent::AssistantTextDelta { delta }).await;
                    }
                }
                "reasoning" => {
                    let text = item
                        .get("text")
                        .or_else(|| item.get("content"))
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    if text.len() > *last_reasoning_len {
                        let delta = text[*last_reasoning_len..].to_string();
                        *last_reasoning_len = text.len();
                        events.send(ProviderTurnEvent::ReasoningDelta { delta }).await;
                    }
                }
                "toolCall" | "tool_call" => {
                    let call_id = item
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let name = item
                        .get("name")
                        .or_else(|| item.get("toolName"))
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let args = item
                        .get("args")
                        .or_else(|| item.get("params"))
                        .cloned()
                        .unwrap_or(Value::Null);
                    if !call_id.is_empty() && !name.is_empty() {
                        events
                            .send(ProviderTurnEvent::ToolCallStarted { call_id, name, args })
                            .await;
                    }
                }
                _ => {}
            }
        }
        // Tool execution completion events.
        "item/complete" | "item/completed" => {
            let item = match params.get("item") {
                Some(v) => v,
                None => return,
            };
            let item_type = item.get("type").and_then(Value::as_str).unwrap_or("");
            if matches!(item_type, "toolCall" | "tool_call") {
                let call_id = item
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let output = item
                    .get("result")
                    .or_else(|| item.get("output"))
                    .map(|v| v.to_string())
                    .unwrap_or_default();
                let error = item
                    .get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(Value::as_str)
                    .map(str::to_string);
                if !call_id.is_empty() {
                    events
                        .send(ProviderTurnEvent::ToolCallCompleted { call_id, output, error })
                        .await;
                }
            }
        }
        _ => {}
    }
}

fn first_non_empty_line(bytes: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(bytes);
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(ToOwned::to_owned)
}
