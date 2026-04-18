use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tracing::warn;
use zenui_provider_api::{
    PermissionMode, ProviderAdapter, ProviderKind, ProviderModel, ProviderSessionState,
    ProviderStatus, ProviderStatusLevel, ProviderTurnEvent, ProviderTurnOutput, ReasoningEffort,
    SessionDetail, TurnEventSink, UserInput, UserInputAnswer, UserInputOption, UserInputQuestion,
};

const REQUEST_TIMEOUT_MS: u64 = 20_000;

fn session_cwd(session: &SessionDetail, fallback: &Path) -> PathBuf {
    session
        .cwd
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(|| fallback.to_path_buf())
}
const RECOVERABLE_THREAD_RESUME_ERRORS: &[&str] = &[
    "not found",
    "missing thread",
    "no such thread",
    "unknown thread",
    "does not exist",
    "no rollout found",
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
    active_mode: PermissionMode,
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
        params: Value,
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
            binary_path: Self::find_codex_binary(),
            working_directory,
            sessions: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Locate the `codex` binary using the cross-platform resolver in
    /// `zenui-provider-api`. Walks PATH (with PATHEXT on Windows)
    /// then falls back to OS-specific install locations
    /// (`~/.local/bin/codex`, `/opt/homebrew/bin/codex`, ...). Returns
    /// the bare name as a last resort so `Command::new("codex")`
    /// still attempts a runtime PATH lookup; the resulting ENOENT
    /// will surface to the caller through `spawn_process`.
    fn find_codex_binary() -> String {
        zenui_provider_api::find_cli_binary("codex")
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| "codex".to_string())
    }

    async fn ensure_session_process(
        &self,
        session: &SessionDetail,
        permission_mode: PermissionMode,
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

        let process = self.create_session_process(session, permission_mode).await?;
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
        permission_mode: PermissionMode,
    ) -> Result<CodexSessionProcess, String> {
        let cwd = session_cwd(session, &self.working_directory);
        let mut child = Command::new(&self.binary_path)
            .arg("app-server")
            .current_dir(&cwd)
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
            active_mode: permission_mode,
        };

        process
            .send_request("initialize", initialize_params())
            .await?;
        process.send_notification("initialized").await?;

        let resume_thread_id = session
            .provider_state
            .as_ref()
            .and_then(|state| state.native_thread_id.as_deref());

        let (approval_policy, sandbox) = map_permission_mode(permission_mode);
        let mut base_params = json!({
            "approvalPolicy": approval_policy,
            "cwd": cwd.display().to_string(),
            "personality": "pragmatic",
            "sandbox": sandbox,
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

    fn default_enabled(&self) -> bool {
        false
    }

    async fn health(&self) -> ProviderStatus {
        probe_cli(
            &self.binary_path,
            ProviderKind::Codex,
            &["--version"],
            &["login", "status"],
            codex_models(),
            zenui_provider_api::features_for_kind(ProviderKind::Codex),
        )
        .await
    }

    async fn fetch_models(&self) -> Result<Vec<ProviderModel>, String> {
        // Spawn an ephemeral codex app-server, run the JSON-RPC handshake, send
        // model/list, parse, and kill the process. The response shape isn't
        // documented anywhere we can audit, so we're defensive: any parsing
        // failure falls back to the hardcoded list.
        let mut child = Command::new(&self.binary_path)
            .arg("app-server")
            .current_dir(&self.working_directory)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("failed to launch codex app-server for model list: {e}"))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "codex stdin unavailable".to_string())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "codex stdout unavailable".to_string())?;
        let _stderr_drain = child.stderr.take().map(spawn_stderr_drain);

        let mut process = CodexSessionProcess {
            child,
            stdin,
            stdout: BufReader::new(stdout).lines(),
            stderr_task: tokio::spawn(async {}),
            next_request_id: 1,
            provider_thread_id: String::new(),
            active_mode: PermissionMode::default(),
        };

        process
            .send_request("initialize", initialize_params())
            .await
            .map_err(|e| format!("codex initialize failed: {e}"))?;
        process.send_notification("initialized").await.ok();
        let response = process
            .send_request("model/list", json!({}))
            .await
            .map_err(|e| format!("codex model/list failed: {e}"))?;
        let _ = process.child.start_kill();

        tracing::info!(
            "codex model/list raw response: {}",
            serde_json::to_string(&response).unwrap_or_else(|_| "<unserializable>".to_string())
        );

        let models = parse_codex_model_list(&response);
        if models.is_empty() {
            Err(format!(
                "codex model/list returned no parseable models from: {}",
                serde_json::to_string(&response).unwrap_or_else(|_| "<unserializable>".to_string())
            ))
        } else {
            Ok(models)
        }
    }

    async fn start_session(
        &self,
        _session: &SessionDetail,
    ) -> Result<Option<ProviderSessionState>, String> {
        // Defer the codex CLI spawn (and the thread/start RPC handshake) to
        // the first execute_turn. Spawning eagerly here used to add 1-3s to
        // "create new thread" because thread/start waits on the codex
        // binary to fully initialize. execute_turn already calls
        // ensure_session_process lazily, and the captured provider_thread_id
        // flows back via ProviderTurnOutput.provider_state on the first
        // successful turn — the runtime persists it from there.
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
        if !input.images.is_empty() {
            tracing::warn!(
                provider = ?ProviderKind::Codex,
                count = input.images.len(),
                "codex adapter dropping image attachments; not implemented"
            );
        }
        // Codex's approvalPolicy/sandbox are bound at thread/start, so a mid-session
        // mode switch requires tearing down and recreating the thread. The runtime's
        // provider_state round-trips native_thread_id, so the recreated process
        // can call thread/resume and conversation history is preserved.
        let process = self.ensure_session_process(session, permission_mode).await?;
        {
            let current_mode = process.lock().await.active_mode;
            if current_mode != permission_mode {
                drop(process);
                self.invalidate_session(&session.summary.session_id).await;
            }
        }
        let process = self.ensure_session_process(session, permission_mode).await?;

        let result = {
            let mut process = process.lock().await;
            let provider_thread_id = process.provider_thread_id.clone();
            let effort_str = reasoning_effort
                .unwrap_or(ReasoningEffort::Medium)
                .as_str();
            let mut turn_params = json!({
                "input": [{
                    "text": input.text.as_str(),
                    "text_elements": [],
                    "type": "text",
                }],
                "threadId": provider_thread_id,
            });
            // Codex accepts reasoning_effort as a turn-level hint on every turn.
            // Unknown fields are ignored by the server, so this is safe on older CLIs.
            if let Some(obj) = turn_params.as_object_mut() {
                obj.insert("reasoning_effort".to_string(), json!(effort_str));
            }
            if permission_mode == PermissionMode::Plan {
                if let Some(obj) = turn_params.as_object_mut() {
                    let model = session.summary.model.clone().unwrap_or_default();
                    // Do NOT set `developer_instructions` — when it's absent/null,
                    // codex's `normalize_turn_start_collaboration_mode` fills in the
                    // Plan preset's system prompt (see codex-rs/models-manager/src/
                    // collaboration_mode_presets.rs::plan_preset), which is what tells
                    // the model to use `request_user_input` for clarifying questions.
                    // Sending `""` counts as `Some("")` in serde and suppresses the
                    // preset, causing the model to fall back to plaintext selection.
                    obj.insert(
                        "collaborationMode".to_string(),
                        json!({
                            "mode": "plan",
                            "settings": {
                                "model": model,
                                "reasoning_effort": effort_str,
                            },
                        }),
                    );
                }
            }
            let response = process.send_request("turn/start", turn_params).await?;
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
                session.summary.session_id
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
                session.summary.session_id
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
            session.summary.session_id
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
                        ProtocolMessage::ServerRequest { id, method, .. } => {
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
                    tracing::debug!(method = %method, "codex: notification received");
                    map_codex_notification(&method, &params, events).await;
                }
                ProtocolMessage::ServerRequest { id, method, params } => {
                    if method == "item/tool/requestUserInput" {
                        // Codex's ask_user tool: parse the questions array (schema at
                        // codex-rs/app-server-protocol/src/protocol/v2.rs::ToolRequestUserInputParams),
                        // ask the user, then respond with ToolRequestUserInputResponse shape:
                        // `{ answers: { [questionId]: { answers: [string] } } }`. While
                        // ask_user() is awaited, codex is blocked on our response.
                        let questions = parse_codex_questions(&params);
                        match events.ask_user(questions).await {
                            Some(answers) => {
                                let result = build_codex_response(&answers);
                                self.write_message(json!({ "id": id, "result": result }))
                                    .await?;
                            }
                            None => {
                                self.write_message(json!({
                                    "id": id,
                                    "error": {
                                        "code": -32000,
                                        "message": "User did not provide an answer.",
                                    },
                                }))
                                .await?;
                            }
                        }
                    } else {
                        self.respond_unsupported(id, &method).await?;
                    }
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
                        params: value.get("params").cloned().unwrap_or(Value::Null),
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
    models: Vec<ProviderModel>,
    features: zenui_provider_api::ProviderFeatures,
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
                        models,
                        enabled: true,
                        features: features.clone(),
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
                    models,
                    enabled: true,
                    features,
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
            models,
            enabled: true,
            features,
        },
    }
}

/// Heuristic parser for the codex `model/list` response. The shape isn't
/// formally documented, so we recursively walk the response looking for any
/// JSON array whose objects have a model-identifier-shaped field. Accepts:
///   - id / value / slug / model / name (for the identifier)
///   - displayName / display_name / label / name / title (for the label)
fn parse_codex_model_list(response: &Value) -> Vec<ProviderModel> {
    let mut out = Vec::new();
    walk_for_models(response, &mut out);
    out
}

fn walk_for_models(value: &Value, out: &mut Vec<ProviderModel>) {
    match value {
        Value::Array(arr) => {
            // If every entry looks like a model object, harvest the array.
            let parsed: Vec<ProviderModel> = arr
                .iter()
                .filter_map(extract_model_entry)
                .collect();
            if !parsed.is_empty() && parsed.len() >= arr.len() / 2 {
                out.extend(parsed);
                return;
            }
            // Otherwise recurse into each element (in case it's nested).
            for v in arr {
                walk_for_models(v, out);
            }
        }
        Value::Object(obj) => {
            for (_k, v) in obj {
                walk_for_models(v, out);
                if !out.is_empty() {
                    return;
                }
            }
        }
        _ => {}
    }
}

fn extract_model_entry(entry: &Value) -> Option<ProviderModel> {
    let obj = entry.as_object()?;
    // Codex marks deprecated/private models with hidden: true. Skip them so
    // the dropdown only lists models the user can actually pick.
    if obj.get("hidden").and_then(Value::as_bool).unwrap_or(false) {
        return None;
    }
    let value = obj
        .get("id")
        .or_else(|| obj.get("value"))
        .or_else(|| obj.get("slug"))
        .or_else(|| obj.get("model"))
        .or_else(|| obj.get("name"))
        .and_then(Value::as_str)?
        .to_string();
    let label = obj
        .get("displayName")
        .or_else(|| obj.get("display_name"))
        .or_else(|| obj.get("label"))
        .or_else(|| obj.get("title"))
        .or_else(|| obj.get("name"))
        .and_then(Value::as_str)
        .unwrap_or(&value)
        .to_string();
    // Pick up context window / max output tokens when Codex's
    // `model/list` response carries them. Supports a few common key
    // spellings so a minor schema tweak doesn't silently drop the
    // value on the floor.
    let context_window = obj
        .get("contextWindow")
        .or_else(|| obj.get("context_window"))
        .or_else(|| obj.get("maxContextWindowTokens"))
        .or_else(|| obj.get("max_context_window_tokens"))
        .and_then(Value::as_u64);
    let max_output_tokens = obj
        .get("maxOutputTokens")
        .or_else(|| obj.get("max_output_tokens"))
        .and_then(Value::as_u64);
    Some(ProviderModel {
        value,
        label,
        context_window,
        max_output_tokens,
        ..ProviderModel::default()
    })
}

/// Parse a `ToolRequestUserInputParams` value (from `item/tool/requestUserInput`)
/// into zenui's cross-provider question list.
fn parse_codex_questions(params: &Value) -> Vec<UserInputQuestion> {
    let Some(array) = params.get("questions").and_then(Value::as_array) else {
        return Vec::new();
    };
    array
        .iter()
        .map(|q| {
            let options = q
                .get("options")
                .and_then(Value::as_array)
                .map(|opts| {
                    opts.iter()
                        .enumerate()
                        .map(|(i, o)| UserInputOption {
                            id: i.to_string(),
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
                id: q
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                text: q
                    .get("question")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                header: q.get("header").and_then(Value::as_str).map(str::to_string),
                options,
                multi_select: false,
                allow_freeform: q
                    .get("isOther")
                    .and_then(Value::as_bool)
                    .unwrap_or(true),
                is_secret: q
                    .get("isSecret")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            }
        })
        .collect()
}

/// Build a `ToolRequestUserInputResponse`-shaped JSON value:
/// `{ answers: { [questionId]: { answers: [string] } } }`.
fn build_codex_response(answers: &[UserInputAnswer]) -> Value {
    let mut map = serde_json::Map::new();
    for a in answers {
        map.insert(
            a.question_id.clone(),
            json!({ "answers": [a.answer.clone()] }),
        );
    }
    json!({ "answers": Value::Object(map) })
}

/// Map zenui's PermissionMode → codex's (approvalPolicy, sandbox) tuple at thread/start.
fn map_permission_mode(mode: PermissionMode) -> (&'static str, &'static str) {
    match mode {
        PermissionMode::Default => ("untrusted", "read-only"),
        PermissionMode::AcceptEdits => ("on-request", "workspace-write"),
        // Plan mode reuses AcceptEdits's policy; the actual plan-mode toggle
        // happens per turn via `collaborationMode.mode = "plan"` in turn/start.
        PermissionMode::Plan => ("on-request", "workspace-write"),
        PermissionMode::Bypass => ("never", "danger-full-access"),
        // Codex has no model-classifier permission mode. The UI gates
        // the "Auto" option on `supports_auto_permission_mode`, which
        // this adapter never sets to true, so this arm is defensive.
        PermissionMode::Auto => ("untrusted", "read-only"),
    }
}

fn codex_models() -> Vec<ProviderModel> {
    // Fallback capability values for when Codex doesn't return
    // model metadata via `model/list`. Numbers follow OpenAI's
    // current public model cards.
    vec![
        ProviderModel {
            value: "gpt-5".to_string(),
            label: "GPT-5 (Codex)".to_string(),
            context_window: Some(400_000),
            max_output_tokens: Some(128_000),
            ..ProviderModel::default()
        },
        ProviderModel {
            value: "o3".to_string(),
            label: "o3".to_string(),
            context_window: Some(200_000),
            max_output_tokens: Some(100_000),
            ..ProviderModel::default()
        },
        ProviderModel {
            value: "gpt-4o".to_string(),
            label: "GPT-4o".to_string(),
            context_window: Some(128_000),
            max_output_tokens: Some(16_384),
            ..ProviderModel::default()
        },
    ]
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

/// Map a codex app-server notification into zenui streaming events.
///
/// Uses the actual Codex app-server JSON-RPC method names documented at
/// <https://developers.openai.com/codex/app-server>. The delta events
/// already carry incremental chunks (not accumulated text), so no diffing
/// is needed here — we forward each chunk directly.
async fn map_codex_notification(method: &str, params: &Value, events: &TurnEventSink) {
    match method {
        // Agent message streaming: params.delta is an incremental text chunk.
        "item/agentMessage/delta" => {
            if let Some(delta) = params.get("delta").and_then(Value::as_str) {
                if !delta.is_empty() {
                    events
                        .send(ProviderTurnEvent::AssistantTextDelta {
                            delta: delta.to_string(),
                        })
                        .await;
                }
            }
        }

        // Reasoning streaming: raw chain-of-thought chunks in params.textDelta.
        "item/reasoning/textDelta" => {
            let delta = params
                .get("textDelta")
                .and_then(Value::as_str)
                .or_else(|| params.get("delta").and_then(Value::as_str))
                .unwrap_or("");
            if !delta.is_empty() {
                events
                    .send(ProviderTurnEvent::ReasoningDelta {
                        delta: delta.to_string(),
                    })
                    .await;
            }
        }

        // Reasoning summary streaming: readable chain-of-thought summaries.
        "item/reasoning/summaryTextDelta" => {
            let delta = params
                .get("delta")
                .and_then(Value::as_str)
                .or_else(|| params.get("textDelta").and_then(Value::as_str))
                .unwrap_or("");
            if !delta.is_empty() {
                events
                    .send(ProviderTurnEvent::ReasoningDelta {
                        delta: delta.to_string(),
                    })
                    .await;
            }
        }

        // Item lifecycle: `item/started` fires for tool-call-ish items as well as
        // agent messages / reasoning. We emit ToolCallStarted only for the tool-ish
        // item types so the UI can track them.
        "item/started" => {
            let Some(item) = params.get("item") else { return };
            let item_type = item.get("type").and_then(Value::as_str).unwrap_or("");
            if is_tool_like_item_type(item_type) {
                let call_id = item
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let name = tool_item_display_name(item, item_type);
                let args = tool_item_args(item).unwrap_or(Value::Null);
                if !call_id.is_empty() {
                    events
                        .send(ProviderTurnEvent::ToolCallStarted {
                            call_id,
                            name,
                            args,
                            parent_call_id: None,
                        })
                        .await;
                }
            }
        }

        // `item/completed` is authoritative final state for all item types.
        // For tool-like items → emit ToolCallCompleted. For fileChange items →
        // emit FileChange.
        "item/completed" => {
            let Some(item) = params.get("item") else { return };
            let item_type = item.get("type").and_then(Value::as_str).unwrap_or("");

            if is_tool_like_item_type(item_type) {
                let call_id = item
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let output = item
                    .get("output")
                    .or_else(|| item.get("result"))
                    .or_else(|| item.get("stdout"))
                    .map(|v| match v.as_str() {
                        Some(s) => s.to_string(),
                        None => v.to_string(),
                    })
                    .unwrap_or_default();
                let error = item
                    .get("error")
                    .and_then(|e| e.get("message").or(Some(e)))
                    .and_then(Value::as_str)
                    .map(str::to_string);
                if !call_id.is_empty() {
                    events
                        .send(ProviderTurnEvent::ToolCallCompleted {
                            call_id: call_id.clone(),
                            output,
                            error,
                        })
                        .await;
                }
            }

            if item_type == "fileChange" {
                if let Some(fc) = extract_file_change(item) {
                    events.send(fc).await;
                }
            }
        }

        // Plan updates. `turn/plan/updated` carries the full plan; `item/plan/delta`
        // carries incremental step text.
        "turn/plan/updated" | "item/plan/delta" => {
            let plan_steps = params
                .get("plan")
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(|entry| {
                            entry
                                .get("step")
                                .and_then(Value::as_str)
                                .map(|s| zenui_provider_api::PlanStep {
                                    title: s.to_string(),
                                    detail: entry
                                        .get("status")
                                        .and_then(Value::as_str)
                                        .map(str::to_string),
                                })
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            if plan_steps.is_empty() {
                return;
            }
            let plan_id = notification_turn_id(params).unwrap_or_else(|| "codex-plan".to_string());
            let raw = params
                .get("explanation")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_default();
            events
                .send(ProviderTurnEvent::PlanProposed {
                    plan_id,
                    title: "Codex plan".to_string(),
                    steps: plan_steps,
                    raw,
                })
                .await;
        }

        _ => {}
    }
}

/// Returns true for Codex item types that map to a tool-call-style entry
/// in the UI's work log (commands, file changes, MCP tool invocations, etc).
fn is_tool_like_item_type(item_type: &str) -> bool {
    matches!(
        item_type,
        "commandExecution"
            | "fileChange"
            | "mcpToolCall"
            | "dynamicToolCall"
            | "collabToolCall"
            | "webSearch"
    )
}

/// Pick the best display name for a tool-call item (e.g., `Bash`, `Write`,
/// or the raw item type if nothing more specific is available).
fn tool_item_display_name(item: &Value, item_type: &str) -> String {
    match item_type {
        "commandExecution" => "Bash".to_string(),
        "fileChange" => "File change".to_string(),
        "mcpToolCall" | "dynamicToolCall" | "collabToolCall" => item
            .get("toolName")
            .or_else(|| item.get("name"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| item_type.to_string()),
        "webSearch" => "Web search".to_string(),
        _ => item_type.to_string(),
    }
}

/// Extract the raw args payload from a tool-like item. For commandExecution
/// we surface `{ command: "..." }` so the UI's "Ran command" renderer picks
/// it up; for other types we forward the whole item as args.
fn tool_item_args(item: &Value) -> Option<Value> {
    let item_type = item.get("type").and_then(Value::as_str).unwrap_or("");
    match item_type {
        "commandExecution" => {
            let command = item
                .get("command")
                .and_then(Value::as_str)
                .or_else(|| {
                    item.get("argv")
                        .and_then(Value::as_array)
                        .and_then(|arr| arr.first())
                        .and_then(Value::as_str)
                });
            command.map(|c| json!({ "command": c }))
        }
        _ => item.get("args").cloned().or_else(|| Some(item.clone())),
    }
}

/// Translate a Codex `fileChange` item into a zenui FileChange event. Codex
/// file change items carry `path`, an operation kind, and before/after text.
fn extract_file_change(item: &Value) -> Option<ProviderTurnEvent> {
    let call_id = item
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let path = item.get("path").and_then(Value::as_str)?.to_string();
    let operation = match item
        .get("operation")
        .or_else(|| item.get("kind"))
        .and_then(Value::as_str)
    {
        Some("write") | Some("create") | Some("add") => zenui_provider_api::FileOperation::Write,
        Some("delete") | Some("remove") => zenui_provider_api::FileOperation::Delete,
        _ => zenui_provider_api::FileOperation::Edit,
    };
    let before = item
        .get("before")
        .or_else(|| item.get("originalContent"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let after = item
        .get("after")
        .or_else(|| item.get("newContent"))
        .and_then(Value::as_str)
        .map(str::to_string);
    Some(ProviderTurnEvent::FileChange {
        call_id,
        path,
        operation,
        before,
        after,
    })
}

fn first_non_empty_line(bytes: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(bytes);
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(ToOwned::to_owned)
}
