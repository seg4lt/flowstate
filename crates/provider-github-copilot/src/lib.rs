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
    bridge_session_id: String,
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

        // Find Node.js binary - first try embedded, then system
        let out_dir = option_env!("OUT_DIR").map(PathBuf::from);
        let exe_dir = std::env::current_exe().ok().and_then(|p| p.parent().map(|p| p.to_path_buf()));
        
        let mut node_paths: Vec<PathBuf> = vec![];
        
        // Embedded Node.js (downloaded by build.rs)
        if let Some(ref dir) = out_dir {
            node_paths.push(dir.join("node/bin/node"));
        }
        if let Some(ref dir) = exe_dir {
            node_paths.push(dir.join("node/bin/node"));
        }
        
        // System Node.js
        node_paths.push(PathBuf::from("/opt/homebrew/bin/node"));
        node_paths.push(PathBuf::from("/usr/local/bin/node"));
        node_paths.push(PathBuf::from("/usr/bin/node"));
        node_paths.push(PathBuf::from("node"));
        
        let node_path = node_paths
            .iter()
            .find(|p| p.to_string_lossy() == "node" || p.exists())
            .cloned()
            .ok_or_else(|| "Node.js not found. Install from https://nodejs.org".to_string())?;

        // Run bridge from the directory containing package.json (for ES module support)
        let bridge_dir = bridge_path.parent().unwrap_or(&self.working_directory);
        let bridge_file = bridge_path.file_name()
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
            bridge_session_id: String::new(),
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
        
        info!("Checking GitHub Copilot health...");

        // Check for embedded or system Node.js
        let out_dir = option_env!("OUT_DIR").map(PathBuf::from);
        let exe_dir = std::env::current_exe().ok().and_then(|p| p.parent().map(|p| p.to_path_buf()));
        
        let mut node_paths: Vec<PathBuf> = vec![];
        
        // Embedded Node.js (downloaded by build.rs)
        if let Some(ref dir) = out_dir {
            node_paths.push(dir.join("node/bin/node"));
        }
        if let Some(ref dir) = exe_dir {
            node_paths.push(dir.join("node/bin/node"));
        }
        
        // System Node.js
        node_paths.push(PathBuf::from("/opt/homebrew/bin/node"));
        node_paths.push(PathBuf::from("/usr/local/bin/node"));
        node_paths.push(PathBuf::from("/usr/bin/node"));
        
        let node_found = node_paths.iter().find(|p| p.exists());
        
        if node_found.is_none() {
            // Try 'which node' as fallback
            if let Ok(output) = Command::new("which").arg("node").output().await {
                if !output.status.success() {
                    return ProviderStatus {
                        kind,
                        label: label.to_string(),
                        installed: false,
                        authenticated: false,
                        version: None,
                        status: ProviderStatusLevel::Error,
                        message: Some("Node.js not found. Install from https://nodejs.org".to_string()),
                    };
                }
            }
        }

        // Check if Copilot CLI binary exists (don't run it, just check file)
        let copilot_paths = [
            "/opt/homebrew/bin/copilot",
            "/usr/local/bin/copilot",
            "/home/linuxbrew/.linuxbrew/bin/copilot",
        ];
        
        let copilot_found = copilot_paths.iter().find(|p| std::path::Path::new(p).exists());
        
        match copilot_found {
            Some(path) => ProviderStatus {
                kind,
                label: label.to_string(),
                installed: true,
                authenticated: true,
                version: None,
                status: ProviderStatusLevel::Ready,
                message: Some(format!("Copilot SDK ready (found at {})", path)),
            },
            None => ProviderStatus {
                kind,
                label: label.to_string(),
                installed: true,
                authenticated: false,
                version: None,
                status: ProviderStatusLevel::Warning,
                message: Some("Copilot CLI not found. Run: gh extension install github/gh-copilot".to_string()),
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
    ) -> Result<ProviderTurnOutput, String> {
        info!("Executing turn with GitHub Copilot...");

        let process = self.ensure_session_process(session).await?;
        let result = {
            let mut process = process.lock().await;
            let response = self
                .bridge_request(
                    &mut process,
                    BridgeRequest::SendPrompt {
                        prompt: input.to_string(),
                    },
                )
                .await?;

            match response {
                BridgeResponse::Response { output } => Ok(ProviderTurnOutput {
                    output,
                    provider_state: session.provider_state.clone(),
                }),
                BridgeResponse::Error { error } => Err(format!("Copilot error: {error}")),
                other => Err(format!("Unexpected response: {:?}", other)),
            }
        };

        if result.is_err() {
            self.invalidate_session(&session.summary.session_id).await;
        }

        result
    }

    async fn interrupt_turn(&self, session: &SessionDetail) -> Result<String, String> {
        info!("Interrupting GitHub Copilot session...");

        // TODO: interrupt is not functional — the TS bridge's readline loop (bridge/src/index.ts:161)
        // awaits sendPrompt() inline and cannot receive an interrupt message while a prompt is in
        // flight. Fixing this requires making that loop handle new stdin messages concurrently.
        Ok(format!(
            "GitHub Copilot interrupt requested for session '{}'.",
            session.summary.title
        ))
    }
}
