use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;
use tracing::{debug, error, info};
use zenui_provider_api::{
    ProviderAdapter, ProviderKind, ProviderSessionState, ProviderStatus, ProviderStatusLevel,
    ProviderTurnOutput, SessionDetail,
};

const BRIDGE_TIMEOUT_MS: u64 = 120_000;

/// Bridge process wrapper for GitHub Copilot SDK
#[derive(Debug)]
struct CopilotBridgeProcess {
    child: Child,
    stdin: ChildStdin,
    stdout: Lines<BufReader<ChildStdout>>,
    next_request_id: u64,
}

/// ZenUI Bridge Protocol Messages
#[derive(Debug, Serialize)]
#[serde(tag = "type")]
enum BridgeRequest {
    #[serde(rename = "create_session")]
    CreateSession { cwd: String },
    #[serde(rename = "send_prompt")]
    SendPrompt { prompt: String },
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
    #[serde(rename = "response")]
    Response { output: String },
    #[serde(rename = "interrupted")]
    Interrupted,
    #[serde(rename = "error")]
    Error { error: String },
}

/// GitHub Copilot Provider Adapter
#[derive(Debug, Clone)]
pub struct GitHubCopilotAdapter {
    working_directory: PathBuf,
    sessions: Arc<Mutex<HashMap<String, String>>>, // session_id -> bridge_session_id
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

        // Find the bridge script - look in several locations
        // First, try to find relative to the current executable
        let exe_path = std::env::current_exe().ok();
        let exe_dir = exe_path.as_ref().and_then(|p| p.parent());
        
        // Build-time location (via build.rs)
        let out_dir = option_env!("OUT_DIR").map(PathBuf::from);
        
        let mut bridge_paths = vec![];
        
        // First check build output directory (embedded by build.rs)
        if let Some(ref dir) = out_dir {
            bridge_paths.push(dir.join("copilot-bridge.js"));
        }
        
        // Development paths relative to working directory
        bridge_paths.push(PathBuf::from("bridge/dist/index.js"));
        bridge_paths.push(PathBuf::from("crates/provider-github-copilot/bridge/dist/index.js"));
        bridge_paths.push(PathBuf::from("../crates/provider-github-copilot/bridge/dist/index.js"));
        bridge_paths.push(PathBuf::from("../../crates/provider-github-copilot/bridge/dist/index.js"));
        
        // Add paths relative to the executable
        if let Some(dir) = exe_dir {
            bridge_paths.push(dir.join("copilot-bridge.js"));
            bridge_paths.push(dir.join("bridge/dist/index.js"));
            bridge_paths.push(dir.join("crates/provider-github-copilot/bridge/dist/index.js"));
            bridge_paths.push(dir.join("../crates/provider-github-copilot/bridge/dist/index.js"));
        }
        
        // Production system paths
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

        let mut child = Command::new("node")
            .arg(&bridge_path)
            .current_dir(&self.working_directory)
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

        // Spawn stderr reader for logging
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
            stdin,
            stdout: BufReader::new(stdout).lines(),
            next_request_id: 1,
        };

        // Wait for ready signal
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

    /// Send a request to the bridge and wait for response
    async fn bridge_request(
        &self,
        process: &mut CopilotBridgeProcess,
        request: BridgeRequest,
    ) -> Result<BridgeResponse, String> {
        let request_json = serde_json::to_string(&request)
            .map_err(|e| format!("Failed to serialize request: {e}"))?;

        debug!("Sending to bridge: {}", request_json);

        process
            .stdin
            .write_all(request_json.as_bytes())
            .await
            .map_err(|e| format!("Failed to write to bridge: {e}"))?;
        process
            .stdin
            .write_all(b"\n")
            .await
            .map_err(|e| format!("Failed to write newline: {e}"))?;
        process
            .stdin
            .flush()
            .await
            .map_err(|e| format!("Failed to flush: {e}"))?;

        // Read response with timeout
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
                        .map_err(|e| format!("Failed to parse bridge response: {e}"));
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

        // Check if Node.js is available
        match Command::new("node").arg("--version").output().await {
            Ok(node_version) => {
                let version = String::from_utf8_lossy(&node_version.stdout).trim().to_string();
                let installed = node_version.status.success();

                if !installed {
                    return ProviderStatus {
                        kind,
                        label: label.to_string(),
                        installed: false,
                        authenticated: false,
                        version: None,
                        status: ProviderStatusLevel::Error,
                        message: Some("Node.js is not available. Required for GitHub Copilot SDK.".to_string()),
                    };
                }

                // Check if Copilot CLI is available (search common paths)
                let copilot_paths = [
                    "copilot",
                    "/opt/homebrew/bin/copilot",
                    "/usr/local/bin/copilot",
                    "/home/linuxbrew/.linuxbrew/bin/copilot",
                ];
                
                let mut copilot_found = None;
                for path in &copilot_paths {
                    if Command::new(path).arg("--version").output().await.is_ok() {
                        copilot_found = Some(*path);
                        break;
                    }
                }
                
                match copilot_found {
                    Some(copilot_path) => {
                        let copilot_ver = Command::new(copilot_path)
                            .arg("--version")
                            .output()
                            .await
                            .ok()
                            .and_then(|o| String::from_utf8(o.stdout).ok())
                            .and_then(|s| s.lines().next().map(|l| l.trim().to_string()));
                        
                        ProviderStatus {
                            kind,
                            label: label.to_string(),
                            installed: true,
                            authenticated: true,
                            version: copilot_ver,
                            status: ProviderStatusLevel::Ready,
                            message: Some(format!(
                                "Node.js: {}, Copilot CLI found at {}",
                                version,
                                copilot_path
                            )),
                        }
                    }
                    None => ProviderStatus {
                        kind,
                        label: label.to_string(),
                        installed: true,
                        authenticated: false,
                        version: Some(version),
                        status: ProviderStatusLevel::Warning,
                        message: Some(
                            "Node.js available but Copilot CLI not found. Install with: gh extension install github/gh-copilot".to_string(),
                        ),
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
                message: Some(format!("Node.js is unavailable: {error}")),
            },
        }
    }

    async fn start_session(
        &self,
        session: &SessionDetail,
    ) -> Result<Option<ProviderSessionState>, String> {
        info!("Starting GitHub Copilot session...");

        let mut bridge = self.spawn_bridge().await?;

        // Create session via bridge
        let response = self
            .bridge_request(
                &mut bridge,
                BridgeRequest::CreateSession {
                    cwd: self.working_directory.display().to_string(),
                },
            )
            .await?;

        match response {
            BridgeResponse::SessionCreated { session_id } => {
                info!("Session created with ID: {}", session_id);
                
                // Store session mapping
                self.sessions
                    .lock()
                    .await
                    .insert(session.summary.session_id.clone(), session_id);

                Ok(Some(ProviderSessionState {
                    native_thread_id: Some(session.summary.session_id.clone()),
                    metadata: None,
                }))
            }
            BridgeResponse::Error { error } => {
                Err(format!("Failed to create session: {error}"))
            }
            _ => Err(format!("Unexpected bridge response: {:?}", response)),
        }
    }

    async fn execute_turn(
        &self,
        session: &SessionDetail,
        input: &str,
    ) -> Result<ProviderTurnOutput, String> {
        info!("Executing turn with GitHub Copilot...");

        // Get the bridge session ID
        let bridge_session_id = self
            .sessions
            .lock()
            .await
            .get(&session.summary.session_id)
            .cloned()
            .ok_or_else(|| "Session not found".to_string())?;

        // Spawn a new bridge for this turn (bridges are stateless per request)
        let mut bridge = self.spawn_bridge().await?;

        // We need to re-create the session context for the bridge
        // In a full implementation, we'd maintain persistent bridge connections
        let _ = self
            .bridge_request(
                &mut bridge,
                BridgeRequest::CreateSession {
                    cwd: self.working_directory.display().to_string(),
                },
            )
            .await?;

        // Send the prompt
        let response = self
            .bridge_request(
                &mut bridge,
                BridgeRequest::SendPrompt {
                    prompt: input.to_string(),
                },
            )
            .await?;

        match response {
            BridgeResponse::Response { output } => {
                Ok(ProviderTurnOutput {
                    output,
                    provider_state: session.provider_state.clone(),
                })
            }
            BridgeResponse::Error { error } => {
                Err(format!("Copilot error: {error}"))
            }
            _ => Err(format!("Unexpected response: {:?}", response)),
        }
    }

    async fn interrupt_turn(&self, session: &SessionDetail) -> Result<String, String> {
        info!("Interrupting GitHub Copilot session...");

        // In a full implementation, we'd send interrupt to active bridge
        // For now, just acknowledge
        Ok(format!(
            "GitHub Copilot interrupt requested for session '{}'.",
            session.summary.title
        ))
    }
}
