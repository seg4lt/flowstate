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
    ProviderAdapter, ProviderKind, ProviderSessionState, ProviderStatus, ProviderStatusLevel,
    ProviderTurnOutput, SessionDetail,
};

const REQUEST_TIMEOUT_MS: u64 = 30_000;

#[derive(Debug, Clone)]
pub struct GitHubCopilotAdapter {
    binary_path: String,
    working_directory: PathBuf,
    sessions: Arc<Mutex<HashMap<String, Arc<Mutex<CopilotSessionProcess>>>>>,
}

#[derive(Debug)]
struct CopilotSessionProcess {
    child: Child,
    stdin: ChildStdin,
    stdout: Lines<BufReader<ChildStdout>>,
    stderr_task: JoinHandle<()>,
    next_request_id: u64,
    session_id: String,
}

#[derive(Debug)]
#[allow(dead_code)]
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

impl GitHubCopilotAdapter {
    pub fn new(working_directory: PathBuf) -> Self {
        Self {
            binary_path: "copilot".to_string(),
            working_directory,
            sessions: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    async fn ensure_session_process(
        &self,
        session: &SessionDetail,
    ) -> Result<Arc<Mutex<CopilotSessionProcess>>, String> {
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
    ) -> Result<CopilotSessionProcess, String> {
        // Spawn copilot CLI in server mode
        let mut child = Command::new(&self.binary_path)
            .arg("--server")
            .current_dir(&self.working_directory)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| format!("failed to launch Copilot CLI server: {error}"))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "Copilot CLI stdin is unavailable.".to_string())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "Copilot CLI stdout is unavailable.".to_string())?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| "Copilot CLI stderr is unavailable.".to_string())?;

        let stderr_task = spawn_stderr_drain(stderr);
        let mut process = CopilotSessionProcess {
            child,
            stdin,
            stdout: BufReader::new(stdout).lines(),
            stderr_task,
            next_request_id: 1,
            session_id: String::new(),
        };

        // Initialize the SDK connection
        process
            .send_request("initialize", initialize_params())
            .await?;

        // Create or resume session
        let existing_session_id = session
            .provider_state
            .as_ref()
            .and_then(|state| state.native_thread_id.as_deref());

        let session_result = if let Some(existing_id) = existing_session_id {
            // Try to resume existing session
            match process
                .send_request(
                    "session/resume",
                    json!({ "sessionId": existing_id }),
                )
                .await
            {
                Ok(response) => response,
                Err(_) => {
                    // If resume fails, create new session
                    process
                        .send_request(
                            "session/create",
                            json!({
                                "clientName": "zenui",
                                "cwd": self.working_directory.display().to_string(),
                            }),
                        )
                        .await?
                }
            }
        } else {
            process
                .send_request(
                    "session/create",
                    json!({
                        "clientName": "zenui",
                        "cwd": self.working_directory.display().to_string(),
                    }),
                )
                .await?
        };

        process.session_id = extract_session_id(&session_result)
            .ok_or_else(|| "Copilot session create did not return a session id.".to_string())?;

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
impl ProviderAdapter for GitHubCopilotAdapter {
    fn kind(&self) -> ProviderKind {
        ProviderKind::GitHubCopilot
    }

    async fn health(&self) -> ProviderStatus {
        probe_copilot_cli(&self.binary_path).await
    }

    async fn start_session(
        &self,
        session: &SessionDetail,
    ) -> Result<Option<ProviderSessionState>, String> {
        let process = self.ensure_session_process(session).await?;
        let process = process.lock().await;
        Ok(Some(provider_state(process.session_id.clone())))
    }

    async fn execute_turn(
        &self,
        session: &SessionDetail,
        input: &str,
    ) -> Result<ProviderTurnOutput, String> {
        let process = self.ensure_session_process(session).await?;
        let result = {
            let mut process = process.lock().await;
            let session_id = process.session_id.clone();
            
            // Send the prompt
            let response = process
                .send_request(
                    "session/send",
                    json!({
                        "sessionId": session_id,
                        "prompt": input,
                        "streaming": false,
                    }),
                )
                .await?;

            // Wait for completion and get response
            let output = extract_response_content(&response)?;

            Ok(ProviderTurnOutput {
                output: if output.trim().is_empty() {
                    "Copilot completed without returning text output.".to_string()
                } else {
                    output
                },
                provider_state: Some(provider_state(process.session_id.clone())),
            })
        };

        if result.is_err() {
            self.invalidate_session(&session.summary.session_id).await;
        }

        result
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
                "Copilot interrupt requested for session `{}`.",
                session.summary.title
            ));
        };

        let mut process = process.lock().await;
        let session_id = process.session_id.clone();
        
        process
            .send_request(
                "session/interrupt",
                json!({ "sessionId": session_id }),
            )
            .await?;

        Ok(format!(
            "Copilot interrupt requested for session `{}`.",
            session.summary.title
        ))
    }
}

impl CopilotSessionProcess {
    async fn send_request(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let request_id = self.next_request_id;
        self.next_request_id += 1;
        
        // Copilot SDK uses Content-Length framing
        let message = json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "method": method,
            "params": params,
        });
        
        self.write_message(message).await?;

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

    async fn respond_unsupported(&mut self, id: Value, method: &str) -> Result<(), String> {
        self.write_message(json!({
            "jsonrpc": "2.0",
            "error": {
                "code": -32601,
                "message": format!("Unsupported server request: {method}"),
            },
            "id": id,
        }))
        .await
    }

    async fn write_message(&mut self, value: Value) -> Result<(), String> {
        let body =
            serde_json::to_string(&value).map_err(|error| format!("invalid JSON payload: {error}"))?;
        let message = format!("Content-Length: {}\r\n\r\n{}", body.len(), body);
        
        self.stdin
            .write_all(message.as_bytes())
            .await
            .map_err(|error| format!("failed to write to Copilot CLI stdin: {error}"))?;
        self.stdin
            .flush()
            .await
            .map_err(|error| format!("failed to flush Copilot CLI stdin: {error}"))
    }

    async fn read_message(&mut self) -> Result<ProtocolMessage, String> {
        let mut content_length: Option<usize> = None;
        
        // Read headers
        loop {
            let line = self
                .stdout
                .next_line()
                .await
                .map_err(|error| format!("failed to read Copilot CLI stdout: {error}"))?
                .ok_or_else(|| "Copilot CLI exited unexpectedly.".to_string())?;

            let trimmed = line.trim();
            if trimmed.is_empty() {
                // End of headers
                break;
            }

            if let Some(length_str) = trimmed.strip_prefix("Content-Length: ") {
                content_length = length_str
                    .parse()
                    .map_err(|_| "Invalid Content-Length header".to_string())
                    .ok();
            }
        }

        let length = content_length.ok_or("Missing Content-Length header")?;
        
        // Read body
        let mut body = String::new();
        while body.len() < length {
            let line = self
                .stdout
                .next_line()
                .await
                .map_err(|error| format!("failed to read Copilot CLI stdout: {error}"))?
                .ok_or_else(|| "Copilot CLI exited unexpectedly.".to_string())?;
            body.push_str(&line);
            if body.len() < length {
                body.push('\n');
            }
        }

        let value: Value = serde_json::from_str(&body).map_err(|error| {
            format!("received invalid JSON from Copilot CLI: {error}")
        })?;

        // Parse the JSON-RPC message
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

        Err("Invalid JSON-RPC message received".to_string())
    }
}

async fn probe_copilot_cli(binary: &str) -> ProviderStatus {
    let kind = ProviderKind::GitHubCopilot;
    let label = kind.label();

    // Check version
    match Command::new(binary).arg("--version").output().await {
        Ok(version_output) => {
            let version = first_non_empty_line(&version_output.stdout)
                .or_else(|| first_non_empty_line(&version_output.stderr));

            // Check authentication status
            match Command::new(binary).arg("auth").arg("status").output().await {
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
        "protocolVersion": "3",
        "capabilities": {},
        "clientInfo": {
            "name": "zenui",
            "version": env!("CARGO_PKG_VERSION"),
        },
    })
}

fn provider_state(session_id: String) -> ProviderSessionState {
    ProviderSessionState {
        native_thread_id: Some(session_id),
        metadata: None,
    }
}

fn extract_session_id(value: &Value) -> Option<String> {
    value.get("session")
        .and_then(|session| session.get("id"))
        .and_then(Value::as_str)
        .or_else(|| value.get("sessionId").and_then(Value::as_str))
        .map(ToOwned::to_owned)
}

fn extract_response_content(value: &Value) -> Result<String, String> {
    value.get("response")
        .and_then(|r| r.get("content"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .or_else(|| {
            value.get("content")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        })
        .ok_or_else(|| "Copilot response did not include content.".to_string())
}

fn normalize_id(value: &Value) -> String {
    match value {
        Value::Number(number) => number.to_string(),
        Value::String(text) => text.clone(),
        _ => value.to_string(),
    }
}

fn spawn_stderr_drain(stderr: ChildStderr) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut stderr = BufReader::new(stderr).lines();
        loop {
            match stderr.next_line().await {
                Ok(Some(line)) if !line.trim().is_empty() => {
                    warn!("copilot cli stderr: {line}");
                }
                Ok(Some(_)) => {}
                Ok(None) => break,
                Err(error) => {
                    warn!("failed to read copilot cli stderr: {error}");
                    break;
                }
            }
        }
    })
}

fn first_non_empty_line(bytes: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(bytes);
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(ToOwned::to_owned)
}
