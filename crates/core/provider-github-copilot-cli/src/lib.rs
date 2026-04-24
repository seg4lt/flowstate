mod config;
mod process;
mod rpc;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::{Mutex, mpsc};
use tracing::{debug, info, warn};
use uuid::Uuid;
use zenui_provider_api::{
    CommandCatalog, CommandKind, McpServerInfo, OrchestrationIpcHandle, PermissionDecision,
    PermissionMode, ProviderAdapter, ProviderAgent, ProviderCommand, ProviderKind, ProviderModel,
    ProviderSessionState, ProviderStatus, ProviderStatusLevel, ProviderTurnEvent,
    ProviderTurnOutput, ReasoningEffort, SessionDetail, TurnEventSink, UserInput, UserInputOption,
    UserInputQuestion, flowstate_mcp_config_file, session_cwd, skills_disk, write_mcp_config_file,
};

// Effectively no turn-level wall clock. The adapter previously
// enforced a 10-minute cap, but long legitimate agent runs routinely
// exceed that. Users cancel stuck turns manually via the UI.
// `u32::MAX` seconds (~136 years) is the sentinel — large enough
// that real turns never trip it, small enough to keep tokio's
// deadline math safe from Instant overflow.
pub(crate) const TURN_TIMEOUT_SECS: u64 = u32::MAX as u64;
const HEALTH_TIMEOUT_SECS: u64 = 10;

use crate::config::{
    AgentsList, CliAgent, CliMcpServer, CliSkill, McpList, SkillsList, copilot_cli_models,
    parse_plan_steps,
};
use crate::process::{
    CLI_IDLE_TIMEOUT_SECS, CLI_WATCHDOG_INTERVAL_SECS, CachedProcess, CallbackSender,
    CopilotCliProcess, EventSender, PendingMap, ServerCallback, run_dispatcher,
};
use crate::rpc::{make_error_response, make_response};

// ── Adapter ───────────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct GitHubCopilotCliAdapter {
    working_directory: PathBuf,
    /// Shared handle over the runtime's loopback HTTP transport — see
    /// `zenui_provider_api::orchestration_ipc` for the full contract.
    /// When populated, each per-session spawn registers a
    /// session-scoped `flowstate.mcp.json` via
    /// `--additional-mcp-config @PATH` (Copilot CLI's equivalent of
    /// Claude CLI's `--mcp-config`). `None` disables orchestration
    /// wiring on this adapter.
    orchestration: Option<OrchestrationIpcHandle>,
    /// One process per ZenUI session. Backed by the shared
    /// `ProcessCache` helper so the idle-kill watchdog logic stays in
    /// lockstep with the SDK/bridge adapters.
    active_processes: Arc<zenui_provider_api::ProcessCache<CopilotCliProcess>>,
}

impl GitHubCopilotCliAdapter {
    /// Construct without orchestration wiring — sessions on this
    /// adapter won't see flowstate's cross-provider tools. Kept as
    /// the default constructor so existing call sites compile
    /// unchanged.
    pub fn new(working_directory: PathBuf) -> Self {
        Self::new_with_orchestration(working_directory, None)
    }

    /// Construct with an optional [`OrchestrationIpcHandle`]. When
    /// populated, every spawn picks up a `--additional-mcp-config`
    /// flag pointing at a session-scoped `flowstate.mcp.json`.
    pub fn new_with_orchestration(
        working_directory: PathBuf,
        orchestration: Option<OrchestrationIpcHandle>,
    ) -> Self {
        Self {
            working_directory,
            orchestration,
            active_processes: Arc::new(zenui_provider_api::ProcessCache::new(
                CLI_IDLE_TIMEOUT_SECS,
                CLI_WATCHDOG_INTERVAL_SECS,
                "provider-github-copilot-cli",
            )),
        }
    }

    /// Spawn the idle-kill watchdog exactly once via the shared helper.
    fn ensure_watchdog(&self) {
        self.active_processes.ensure_watchdog(|cached| async move {
            let mut proc = cached.inner().lock().await;
            // Reap the process group first so MCP/subshell
            // grandchildren die with the CLI; tokio's `start_kill`
            // on `child` only terminates the direct `copilot`
            // process.
            if let Some(pgid) = proc.pgid {
                zenui_provider_api::kill_process_group_best_effort(pgid);
            }
            let _ = proc.child.start_kill();
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
    /// `extra_args` is appended to the base arg vector — primarily
    /// used by `ensure_session_process` to inject
    /// `--additional-mcp-config @PATH` for cross-provider
    /// orchestration wiring. Non-session callers (health check, list
    /// models) pass an empty slice.
    async fn spawn_process(
        binary: &str,
        cwd: &PathBuf,
        extra_args: &[String],
    ) -> Result<CopilotCliProcess, String> {
        info!("Spawning copilot CLI: {} {:?}", binary, extra_args);
        let base_args: &[&str] = &[
            "--headless",
            "--no-auto-update",
            "--log-level",
            "warning",
            "--stdio",
        ];
        let mut cmd = Command::new(binary);
        cmd.args(base_args);
        for arg in extra_args {
            cmd.arg(arg);
        }
        cmd.current_dir(cwd)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        // `copilot` CLI forks per-session agent workers + MCP
        // subprocesses; place them all in this child's process
        // group so `killpg` at teardown reaps the subtree.
        zenui_provider_api::enter_own_process_group(&mut cmd);
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("Failed to spawn copilot CLI ('{binary}'): {e}"))?;
        let pgid: Option<i32> = child.id().and_then(|p| i32::try_from(p).ok());

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
                        debug!(target: "provider-github-copilot-cli", "{}", t);
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
            pgid,
            stdin,
            pending,
            next_id: Arc::new(Mutex::new(1)),
            event_tx,
            callback_tx,
            // Populated by the caller (`ensure_session_process`) after a
            // successful `session.create` / `session.resume` handshake.
            native_session_id: String::new(),
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
        if let Some(existing) = self.active_processes.get(&session.summary.session_id).await {
            return Ok(existing);
        }

        let binary = Self::find_copilot_binary();
        let resolved_cwd = session_cwd(session, &self.working_directory);

        // Cross-provider orchestration: when the Tauri app has mounted
        // the loopback HTTP transport, write a session-scoped
        // `flowstate.mcp.json` and pass it via
        // `--additional-mcp-config @PATH`. Copilot CLI merges this
        // with its global `~/.copilot/mcp-config.json` and any
        // workspace `.mcp.json`, so user-authored MCP configs are
        // preserved. Any failure here is non-fatal: the session just
        // skips registering flowstate's MCP server, and agents don't
        // see cross-provider orchestration tools.
        let extra_args = self
            .orchestration
            .as_ref()
            .and_then(|h| h.get())
            .and_then(|ipc| {
                let cfg = flowstate_mcp_config_file(&ipc, &session.summary.session_id);
                let config_path = self
                    .working_directory
                    .join("sessions")
                    .join(&session.summary.session_id)
                    .join("flowstate.mcp.json");
                match write_mcp_config_file(&config_path, &cfg) {
                    Ok(path) => Some(vec![
                        "--additional-mcp-config".to_string(),
                        format!("@{}", path.to_string_lossy()),
                    ]),
                    Err(err) => {
                        warn!(
                            target: "provider-github-copilot-cli",
                            %err,
                            "failed to write flowstate.mcp.json; \
                             session will not expose cross-provider orchestration tools"
                        );
                        None
                    }
                }
            })
            .unwrap_or_default();

        let mut process = Self::spawn_process(&binary, &resolved_cwd, &extra_args).await?;

        let native_session_id = session
            .provider_state
            .as_ref()
            .and_then(|s| s.native_thread_id.as_deref())
            .map(str::to_string)
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        process.native_session_id = native_session_id.clone();

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

        // Double-check under the cache lock to preserve first-writer-wins
        // on concurrent misses.
        if let Some(existing) = self.active_processes.get(&session.summary.session_id).await {
            let mut dropped = process;
            let _ = dropped.child.start_kill();
            return Ok(existing);
        }
        Ok(self
            .active_processes
            .insert(session.summary.session_id.clone(), process)
            .await)
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
        let deadline =
            tokio::time::Instant::now() + std::time::Duration::from_secs(TURN_TIMEOUT_SECS);

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
            let error = if success { None } else { Some(output.clone()) };
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
            let perm_req = cb
                .params
                .get("permissionRequest")
                .cloned()
                .unwrap_or(cb.params.clone());
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
            //
            // `Auto` is not exposed for Copilot-CLI (the adapter doesn't
            // set `supports_auto_permission_mode`), but the arm is
            // defensive and mirrors `Default`: always ask.
            let decision = match permission_mode {
                PermissionMode::Bypass => PermissionDecision::Allow,
                PermissionMode::AcceptEdits | PermissionMode::Plan => {
                    if matches!(kind, "read" | "write") {
                        PermissionDecision::Allow
                    } else {
                        let (d, _mode_override, _deny_reason) = events
                            .request_permission(
                                kind.to_string(),
                                perm_req.clone(),
                                PermissionDecision::Allow,
                            )
                            .await;
                        d
                    }
                }
                PermissionMode::Default | PermissionMode::Auto => {
                    let (d, _mode_override, _deny_reason) = events
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

    fn default_enabled(&self) -> bool {
        false
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
                features: zenui_provider_api::ProviderFeatures::default(),
            };
        }

        // Try to spawn and ping the binary. No MCP config injection
        // for health — the probe is a transient subprocess and the
        // cost of a stale `flowstate.mcp.json` here isn't worth the
        // complexity.
        let spawn_result = tokio::time::timeout(
            std::time::Duration::from_secs(HEALTH_TIMEOUT_SECS),
            Self::spawn_process(&binary, &self.working_directory, &[]),
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
                    features: zenui_provider_api::ProviderFeatures::default(),
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
                    features: zenui_provider_api::ProviderFeatures::default(),
                };
            }
        };

        // Run all three RPC calls concurrently — they have no data dependencies.
        let timeout = std::time::Duration::from_secs(HEALTH_TIMEOUT_SECS);
        let (ping_raw, status_raw, auth_raw) = tokio::join!(
            tokio::time::timeout(timeout, process.call("ping", serde_json::json!({}))),
            tokio::time::timeout(timeout, process.call("status.get", serde_json::json!({}))),
            tokio::time::timeout(
                timeout,
                process.call("auth.getStatus", serde_json::json!({}))
            ),
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
                features: zenui_provider_api::ProviderFeatures::default(),
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
            features: zenui_provider_api::ProviderFeatures::default(),
        }
    }

    async fn fetch_models(&self) -> Result<Vec<ProviderModel>, String> {
        let binary = Self::find_copilot_binary();
        // Model listing is a read-only ephemeral subprocess — skip
        // flowstate MCP injection.
        let process = Self::spawn_process(&binary, &self.working_directory, &[]).await?;

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
                // Live response may carry the model's ceilings at a few
                // spellings; grab whichever is present. Falls back to
                // the static capability table below when absent.
                let context_window = m
                    .get("contextWindow")
                    .or_else(|| m.get("maxContextWindowTokens"))
                    .or_else(|| {
                        m.get("capabilities")
                            .and_then(|c| c.get("limits"))
                            .and_then(|l| l.get("max_context_window_tokens"))
                    })
                    .and_then(Value::as_u64);
                let max_output_tokens = m
                    .get("maxOutputTokens")
                    .or_else(|| m.get("max_output_tokens"))
                    .or_else(|| {
                        m.get("capabilities")
                            .and_then(|c| c.get("limits"))
                            .and_then(|l| l.get("max_output_tokens"))
                    })
                    .and_then(Value::as_u64);
                Some(ProviderModel {
                    value,
                    label,
                    context_window,
                    max_output_tokens,
                    ..ProviderModel::default()
                })
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
        _thinking_mode: Option<zenui_provider_api::ThinkingMode>,
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
            cached.inner().clone(),
            native_session_id,
            input.text.clone(),
            permission_mode,
            events,
        )
        .await
    }

    async fn interrupt_turn(&self, session: &SessionDetail) -> Result<String, String> {
        let cached = self.active_processes.get(&session.summary.session_id).await;

        if let Some(cached) = cached {
            let native_id = session
                .provider_state
                .as_ref()
                .and_then(|s| s.native_thread_id.as_deref())
                .unwrap_or(&session.summary.session_id)
                .to_string();

            let proc = cached.inner().lock().await;
            if let Err(e) = proc
                .call(
                    "session.abort",
                    serde_json::json!({ "sessionId": native_id }),
                )
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
            .remove(&session.summary.session_id)
            .await;

        if let Some(cached) = cached {
            let native_id = session
                .provider_state
                .as_ref()
                .and_then(|s| s.native_thread_id.as_deref())
                .unwrap_or(&session.summary.session_id)
                .to_string();

            let proc = cached.inner().lock().await;
            // Best-effort destroy then kill.
            let _ = proc
                .call(
                    "session.destroy",
                    serde_json::json!({ "sessionId": native_id }),
                )
                .await;
            drop(proc);

            let mut proc = cached.inner().lock().await;
            let _ = proc.child.start_kill();
        }

        Ok(())
    }

    /// Same scan convention as the Copilot SDK adapter: broad support
    /// for the different skill directory layouts that Copilot-flavoured
    /// repos use in the wild.
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

    /// Merge the disk SKILL.md scan with Copilot CLI's live
    /// `session.skills.list` / `.agent.list` / `.mcp.list` results. The
    /// CLI wraps the same SDK as the Copilot bridge, so the wire shapes
    /// match — skills carry `userInvocable`, which we pass through for
    /// the frontend filter. Requires a live CLI session; on any RPC
    /// error we fall back to disk-only.
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
                warn!("copilot-cli session_command_catalog: falling back to disk-only ({err})");
                (Vec::new(), Vec::new(), Vec::new())
            }
        };

        let disk_names: std::collections::HashSet<String> =
            commands.iter().map(|c| c.name.clone()).collect();
        for skill in sdk_skills {
            if disk_names.contains(&skill.name) {
                continue;
            }
            commands.push(ProviderCommand {
                id: format!("github_copilot_cli:builtin:{}", skill.name),
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
                id: format!("github_copilot_cli:agent:{}", a.name),
                name: a.name,
                description: a.description,
            })
            .collect();

        let mcp_servers = sdk_mcp
            .into_iter()
            .map(|m| McpServerInfo {
                enabled: matches!(m.status.as_deref(), Some("connected") | Some("pending")),
                id: format!("github_copilot_cli:mcp:{}", m.name),
                name: m.name,
            })
            .collect();

        Ok(CommandCatalog {
            commands,
            agents,
            mcp_servers,
        })
    }

    /// Daemon-shutdown hook: kill every cached Copilot CLI child.
    /// Mirrors `end_session`'s `start_kill` path but sweeps the whole
    /// cache in one pass. The CLI's per-process `run_dispatcher` task
    /// exits naturally when stdout closes on child kill, so no
    /// separate abort handle is needed.
    async fn shutdown(&self) {
        for (session_id, cached) in self.active_processes.drain_all().await {
            let mut proc = cached.inner().lock().await;
            if let Err(e) = proc.child.start_kill() {
                debug!(
                    %session_id,
                    "github-copilot-cli shutdown: start_kill failed (child likely already exited): {e}"
                );
            }
        }
    }
}

impl GitHubCopilotCliAdapter {
    /// Call Copilot CLI's `session.skills.list` / `.agent.list` /
    /// `.mcp.list` JSON-RPC methods in parallel via the cached process.
    /// Booting a process if necessary via `ensure_session_process`.
    async fn fetch_capabilities(
        &self,
        session: &SessionDetail,
    ) -> Result<(Vec<CliSkill>, Vec<CliAgent>, Vec<CliMcpServer>), String> {
        let cached = self.ensure_session_process(session).await?;
        let _guard = cached.activity_guard();
        let process = cached.inner().lock().await;
        let native = process.native_session_id.clone();

        let (skills, agents, mcp) = tokio::try_join!(
            process.call(
                "session.skills.list",
                serde_json::json!({ "sessionId": native.clone() }),
            ),
            process.call(
                "session.agent.list",
                serde_json::json!({ "sessionId": native.clone() }),
            ),
            process.call(
                "session.mcp.list",
                serde_json::json!({ "sessionId": native }),
            ),
        )?;

        let parsed_skills: SkillsList =
            serde_json::from_value(skills).map_err(|e| format!("parse skills.list: {e}"))?;
        let parsed_agents: AgentsList =
            serde_json::from_value(agents).map_err(|e| format!("parse agent.list: {e}"))?;
        let parsed_mcp: McpList =
            serde_json::from_value(mcp).map_err(|e| format!("parse mcp.list: {e}"))?;

        Ok((
            parsed_skills.skills,
            parsed_agents.agents,
            parsed_mcp.servers,
        ))
    }
}
