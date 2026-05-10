mod bridge_runtime;
mod config;

// Public entry point for the daemon's startup `provision_runtimes()`
// step. Mirrors `zenui_provider_claude_sdk::ensure_bridge_available`.
pub use bridge_runtime::ensure_bridge_available;
mod process;
mod wire;

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use async_trait::async_trait;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};
use zenui_provider_api::{
    CommandCatalog, CommandKind, McpServerInfo, PermissionDecision, PermissionMode,
    ProviderAdapter, ProviderAgent, ProviderCommand, ProviderKind, ProviderModel,
    ProviderSessionState, ProviderStatus, ProviderStatusLevel, ProviderTurnEvent,
    ProviderTurnOutput, ReasoningEffort, SessionDetail, TurnEventSink, UserInput, UserInputOption,
    UserInputQuestion, UserMcpRegistry, session_cwd, skills_disk,
};

use crate::config::copilot_models;
use crate::process::{
    BRIDGE_IDLE_TIMEOUT_SECS, BRIDGE_TIMEOUT_MS, BRIDGE_WATCHDOG_INTERVAL_SECS, CachedBridge,
    CopilotBridgeProcess, write_request,
};
use crate::wire::{
    BridgeCopilotAgent, BridgeCopilotMcp, BridgeRequest, BridgeResponse, BridgeSkill,
    CopilotBridgeImage, CopilotUserMcpEntry, UserInputOutcome, parse_decision,
    permission_decision_to_str, permission_mode_to_str,
};

#[derive(Clone)]
pub struct GitHubCopilotAdapter {
    working_directory: PathBuf,
    /// Shared handle over the runtime's loopback HTTP transport —
    /// populated by the embedder after the HTTP listener binds. When
    /// populated, every Copilot bridge spawn receives
    /// `FLOWSTATE_HTTP_BASE` + `FLOWSTATE_EXECUTABLE_PATH` env vars;
    /// the bridge reads these when constructing
    /// `SessionConfig.mcpServers.flowstate` so the Copilot SDK spawns
    /// the `flowstate mcp-server` subprocess as part of each session.
    /// No auth token — the loopback bind is the only boundary.
    orchestration: Option<zenui_provider_api::OrchestrationIpcHandle>,
    /// User-defined global MCPs from `~/.flowstate/mcp.json`. Loaded
    /// per-bridge-spawn and shipped to the TS bridge via
    /// `set_user_mcp_servers`; the bridge merges them into every
    /// `SessionConfig.mcpServers` it builds, alongside the flowstate
    /// orchestration entry. `None` means no user MCPs.
    user_mcp: Option<UserMcpRegistry>,
    sessions: Arc<zenui_provider_api::ProcessCache<CopilotBridgeProcess>>,
}

impl GitHubCopilotAdapter {
    /// Construct without cross-provider orchestration wiring.
    pub fn new(working_directory: PathBuf) -> Self {
        Self::new_with_orchestration(working_directory, None, None)
    }

    /// Construct with an optional
    /// [`zenui_provider_api::OrchestrationIpcHandle`]. When
    /// populated, the Copilot TS bridge gets the loopback base URL,
    /// auth token, and executable path as env vars at spawn time;
    /// it uses them to register the flowstate MCP server in every
    /// `SessionConfig.mcpServers` payload it builds. Uses
    /// [`BRIDGE_IDLE_TIMEOUT_SECS`] for idle-kill; prefer
    /// [`Self::new_with_orchestration_and_idle_ttl`] when the host has
    /// a user-config store to read the TTL from.
    pub fn new_with_orchestration(
        working_directory: PathBuf,
        orchestration: Option<zenui_provider_api::OrchestrationIpcHandle>,
        user_mcp: Option<UserMcpRegistry>,
    ) -> Self {
        Self::new_with_orchestration_and_idle_ttl(
            working_directory,
            orchestration,
            user_mcp,
            Some(BRIDGE_IDLE_TIMEOUT_SECS),
        )
    }

    /// Construct with an optional orchestration handle, user MCP
    /// registry, and an explicit idle-kill timeout. Pass `None` to
    /// disable idle-kill (useful in tests). Pass `Some(secs)` to
    /// override the compiled-in [`BRIDGE_IDLE_TIMEOUT_SECS`] default.
    pub fn new_with_orchestration_and_idle_ttl(
        working_directory: PathBuf,
        orchestration: Option<zenui_provider_api::OrchestrationIpcHandle>,
        user_mcp: Option<UserMcpRegistry>,
        idle_timeout_secs: Option<u64>,
    ) -> Self {
        Self {
            working_directory,
            orchestration,
            user_mcp,
            sessions: Arc::new(zenui_provider_api::ProcessCache::new(
                idle_timeout_secs.unwrap_or(BRIDGE_IDLE_TIMEOUT_SECS),
                BRIDGE_WATCHDOG_INTERVAL_SECS,
                "provider-github-copilot",
            )),
        }
    }

    /// Spawn the idle-kill watchdog exactly once. Called lazily from
    /// `ensure_session_process` (rather than `new()`) so we don't rely
    /// on `tokio::spawn` being available at adapter construction time.
    /// Delegates to the shared `ProcessCache` helper.
    fn ensure_watchdog(&self) {
        self.sessions.ensure_watchdog(|cached| async move {
            let mut process = cached.inner().lock().await;
            // Reap the process group / Job Object first so the
            // `copilot` CLI the bridge spawned via `useStdio: true`
            // dies alongside the Node bridge — tokio's `start_kill`
            // on the direct child doesn't cascade to that
            // grandchild.
            process.process_group.kill_best_effort();
            let _ = process.child.start_kill();
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

        // Surface the resolved standalone `copilot` CLI path so the
        // logs answer the recurring "is it the bundled bridge or my
        // local install?" question. The bundled bit is the bridge
        // script + embedded Node above; the bridge then drives a
        // SEPARATE local `copilot` CLI as a subprocess (the SDK
        // construct `new CopilotClient({ useStdio: true, cliPath })`
        // — see bridge/src/index.ts:362). Auth state lives in that
        // CLI's own user-config dir, NOT in the bundled bridge, so
        // signing in once via `copilot` propagates to flowstate but
        // not vice-versa.
        match zenui_provider_api::find_cli_binary("copilot") {
            Some(path) => info!("Using local copilot CLI at: {}", path.display()),
            None => info!(
                "No local copilot CLI found on PATH — bridge will fail on start. \
                Install `@github/copilot` (`npm i -g @github/copilot`) and authenticate via `/login`."
            ),
        }

        // Put the embedded node on PATH so the Copilot SDK's internal
        // `node` subprocess calls resolve to the same runtime; also
        // weave in the user's configured extra search dirs so any
        // grandchild subprocess (`git`, etc.) finds tools the user
        // has explicitly told flowstate about — same rationale as
        // the Claude SDK bridge spawn.
        let new_path = zenui_provider_api::path_with_extras(&[node.bin_dir.as_path()]);

        let mut cmd = Command::new(&node.node_bin);
        cmd.arg(&bridge.script)
            .current_dir(&bridge.dir)
            .env("PATH", new_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // Cross-provider orchestration: when the runtime's loopback
        // HTTP is up, pass its coordinates through the bridge's env
        // so the TS side can mount the flowstate MCP server inside
        // `SessionConfig.mcpServers` for every session it creates.
        // No auth token — the loopback bind is the only boundary.
        if let Some(ipc) = self.orchestration.as_ref().and_then(|h| h.get()) {
            cmd.env("FLOWSTATE_HTTP_BASE", &ipc.base_url);
            cmd.env("FLOWSTATE_EXECUTABLE_PATH", ipc.executable_path.as_os_str());
            // Plumb flowstate's pid so the TS bridge can forward it
            // into the MCP subprocess env. See the `FLOWSTATE_PID`
            // note in `crates/core/provider-api/src/mcp_config.rs`.
            cmd.env("FLOWSTATE_PID", std::process::id().to_string());
        }
        // Put the Node bridge in its own process group / Job Object
        // so the `copilot` CLI subprocess it internally spawns (via
        // `new CopilotClient({ useStdio: true })`) dies with the
        // bridge when flowstate exits or the idle watchdog reaps
        // the cache entry. Without this the CLI grandchild would
        // reparent to PID 1 (Unix) / orphan (Windows) and survive.
        // See `zenui_provider_api::ProcessGroup`.
        let mut process_group = zenui_provider_api::ProcessGroup::before_spawn(&mut cmd);
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("Failed to spawn bridge: {e}"))?;
        process_group.attach(&child);

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "Bridge stdin unavailable".to_string())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "Bridge stdout unavailable".to_string())?;

        // Buffer the last N stderr lines so a fatal-on-startup bridge
        // (e.g. `[bridge] Fatal error: ...` from `main().catch`) can
        // attach context to the "Bridge process closed stdout" error
        // we'd otherwise raise blind. Also stream every line through
        // tracing so they show up in the user's log file in real time.
        // No `target:` override — earlier code used a dashed target
        // (`provider-github-copilot`) which the default `EnvFilter`
        // (which keys on the underscored crate name) silently dropped,
        // so fatal bridge errors were invisible. Logging under the
        // module path makes them obey the same filter as everything
        // else in this crate.
        let stderr_buf: Arc<Mutex<std::collections::VecDeque<String>>> =
            Arc::new(Mutex::new(std::collections::VecDeque::with_capacity(32)));
        if let Some(stderr) = child.stderr.take() {
            let buf = Arc::clone(&stderr_buf);
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    info!("bridge stderr: {trimmed}");
                    let mut guard = buf.lock().await;
                    if guard.len() == 32 {
                        guard.pop_front();
                    }
                    guard.push_back(trimmed.to_string());
                }
            });
        }

        let mut process = CopilotBridgeProcess {
            child,
            process_group,
            stdin: Arc::new(Mutex::new(stdin)),
            stdout: BufReader::new(stdout).lines(),
            bridge_session_id: String::new(),
        };

        debug!("Waiting for bridge ready signal...");
        match tokio::time::timeout(std::time::Duration::from_secs(10), process.read_response())
            .await
        {
            Ok(Ok(BridgeResponse::Ready)) => {
                info!("Bridge is ready");
            }
            Ok(Ok(other)) => {
                return Err(format!("Expected ready signal, got: {:?}", other));
            }
            Ok(Err(e)) => {
                // Bridge died (or otherwise hosed stdout) before
                // emitting `ready`. Reap the child to get an exit code
                // and drain the buffered stderr; both make the failure
                // diagnosable instead of just "stdout closed".
                let exit_status = tokio::time::timeout(
                    std::time::Duration::from_secs(2),
                    process.child.wait(),
                )
                .await
                .ok()
                .and_then(|r| r.ok())
                .map(|s| s.to_string())
                .unwrap_or_else(|| "still running".to_string());
                let tail = {
                    let guard = stderr_buf.lock().await;
                    guard
                        .iter()
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(" | ")
                };
                if tail.is_empty() {
                    return Err(format!(
                        "Failed to read ready signal: {e} (bridge exit: {exit_status}). \
                        No stderr captured — try `npm i -g @github/copilot` and `copilot` → `/login` if not yet installed/authenticated."
                    ));
                }
                return Err(format!(
                    "Failed to read ready signal: {e} (bridge exit: {exit_status}; \
                    last stderr: {tail})"
                ));
            }
            Err(_) => {
                return Err("Timeout waiting for bridge ready signal".to_string());
            }
        }

        // Ship the user MCP catalog. The bridge stashes it and merges
        // entries into every future session's `SessionConfig.mcpServers`
        // alongside the flowstate orchestration entry it builds from
        // the env vars planted earlier. Skipped when no registry is
        // wired or when the snapshot is empty — older bridges with
        // no handler simply ignore the message.
        if let Some(registry) = &self.user_mcp {
            let snapshot = registry.load();
            if !snapshot.is_empty() {
                let entries: Vec<CopilotUserMcpEntry> = snapshot
                    .servers
                    .into_iter()
                    .map(|(name, cfg)| CopilotUserMcpEntry {
                        name,
                        transport: cfg.transport,
                        command: cfg.command,
                        args: cfg.args,
                        env: cfg.env,
                        url: cfg.url,
                    })
                    .collect();
                let req = BridgeRequest::SetUserMcpServers { servers: entries };
                if let Err(err) = write_request(&process.stdin, &req).await {
                    warn!(%err, "failed to ship user MCP catalog to Copilot bridge");
                }
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
        images: Vec<CopilotBridgeImage>,
        events: &TurnEventSink,
    ) -> Result<String, String> {
        write_request(
            &process.stdin,
            &BridgeRequest::SendPrompt {
                prompt,
                permission_mode: permission_mode_to_str(permission_mode).to_string(),
                reasoning_effort: reasoning_effort.map(|e| e.as_str().to_string()),
                images,
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
        let (q_tx, mut q_rx) = tokio::sync::mpsc::unbounded_channel::<(String, UserInputOutcome)>();
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

        let deadline =
            tokio::time::Instant::now() + std::time::Duration::from_millis(BRIDGE_TIMEOUT_MS);

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
                                    // GitHub Copilot CLI has no background-tool
                                    // concept; every tool runs in the foreground.
                                    is_background: false,
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
                            // permission-mode change nor a deny-reason
                            // feedback message, so drop those parts of the
                            // tuple. Adapters that do want them (Claude SDK)
                            // keep all three halves.
                            let (decision, _mode_override, _deny_reason) = events_clone
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
                            events.send(ProviderTurnEvent::TurnUsage { usage: u }).await;
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
        if let Some(existing) = self.sessions.get(&session.summary.session_id).await {
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
                    // Only pass the flowstate session id when
                    // orchestration is actually wired — prevents older
                    // bridges from seeing a field they'd reject and
                    // avoids spurious MCP-server registration on builds
                    // where the loopback transport didn't start.
                    flowstate_session_id: self
                        .orchestration
                        .as_ref()
                        .and_then(|h| h.get())
                        .map(|_| session.summary.session_id.clone()),
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

        // Double-check under the lock via `ProcessCache::insert` (which
        // overwrites). To preserve "first writer wins" semantics on
        // concurrent misses, re-check before inserting.
        if let Some(existing) = self.sessions.get(&session.summary.session_id).await {
            // Someone else already populated the slot while we were
            // spawning; drop our freshly-spawned bridge and return theirs.
            let mut dropped = bridge;
            let _ = dropped.child.start_kill();
            return Ok(existing);
        }
        Ok(self
            .sessions
            .insert(session.summary.session_id.clone(), bridge)
            .await)
    }

    /// Remove a session's bridge from the cache and kill its process.
    async fn invalidate_session(&self, session_id: &str) {
        if let Some(cached) = self.sessions.remove(session_id).await {
            let mut process = cached.inner().lock().await;
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

        // The Copilot SDK bridge spawns `@github/copilot-sdk` which
        // in turn drives the standalone `copilot` CLI as a subprocess
        // (see bridge/src/index.ts:362 — `useStdio: true, cliPath`).
        // So health depends on locating that binary. Reuse the
        // shared cross-platform resolver — it walks `$PATH` (with
        // `PATHEXT` on Windows) and falls back through npm globals,
        // `%APPDATA%\npm`, `~/.local/bin`, Homebrew dirs, etc. The
        // sibling `-cli` crate already uses the same helper.
        match zenui_provider_api::find_cli_binary("copilot") {
            Some(path) => ProviderStatus {
                kind,
                label: label.to_string(),
                installed: true,
                authenticated: true,
                version: None,
                status: ProviderStatusLevel::Ready,
                message: Some(format!("Copilot SDK ready (found at {})", path.display())),
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
                message: Some("Copilot CLI not found on PATH".to_string()),
                models: copilot_models(),
                enabled: true,
                features: zenui_provider_api::ProviderFeatures::default(),
            },
        }
    }

    /// Upgrade the locally-installed `@github/copilot` package via
    /// npm. Unlike Claude / Codex / opencode — all of which ship a
    /// CLI-native `update` subcommand — the Copilot CLI has no in-
    /// place self-update flow, so the official upgrade path is
    /// `npm install -g @github/copilot@latest` (see the trait doc on
    /// `ProviderAdapter::upgrade` and the install hint at lib.rs:153).
    ///
    /// We resolve `npm` via the same cross-platform helper we use for
    /// `copilot` itself (walks `$PATH`, `%APPDATA%\npm`, Volta, nvm,
    /// Homebrew, etc.) so a Tauri-launched daemon — which doesn't
    /// inherit the user's shell rc — still finds it.
    ///
    /// Idempotent: re-running when already at latest is a successful
    /// npm no-op.
    async fn upgrade(&self) -> Result<String, String> {
        let npm_path = zenui_provider_api::find_cli_binary("npm").ok_or_else(|| {
            "npm is not on PATH; install Node.js (which ships npm) and retry, \
             or run `npm install -g @github/copilot@latest` manually."
                .to_string()
        })?;
        let mut cmd = tokio::process::Command::new(&npm_path);
        zenui_provider_api::hide_console_window_tokio(&mut cmd);
        cmd.env("PATH", zenui_provider_api::path_with_extras(&[]));
        let output = cmd
            .args(["install", "-g", "@github/copilot@latest"])
            .output()
            .await
            .map_err(|err| format!("failed to invoke npm install: {err}"))?;
        if output.status.success() {
            Ok("GitHub Copilot CLI upgraded.".to_string())
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
            Err(if !stderr.is_empty() {
                format!("npm install -g @github/copilot@latest failed: {stderr}")
            } else if !stdout.is_empty() {
                format!("npm install -g @github/copilot@latest failed: {stdout}")
            } else {
                format!(
                    "npm install -g @github/copilot@latest exited with status {:?}",
                    output.status.code()
                )
            })
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
        _thinking_mode: Option<zenui_provider_api::ThinkingMode>,
        events: TurnEventSink,
    ) -> Result<ProviderTurnOutput, String> {
        info!(
            "Executing turn with GitHub Copilot (mode={:?}, effort={:?}, images={})",
            permission_mode,
            reasoning_effort,
            input.images.len(),
        );

        let cached = self.ensure_session_process(session).await?;
        // Held for the entire turn. Drops after `process` is released,
        // decrementing in_flight and stamping last_activity = now so the
        // 30-minute idle timer starts ticking.
        let _activity = cached.activity_guard();
        let result = {
            let mut process = cached.inner().lock().await;
            // Capture the bridge session id BEFORE the streaming call so
            // we can return it as native_thread_id even on first turn.
            // ensure_session_process populates it during CreateSession.
            let bridge_session_id = process.bridge_session_id.clone();
            // Map the runtime's `UserInput::images` (the same shape
            // we hand to the Claude SDK adapter) into the
            // bridge-side `CopilotBridgeImage` wire type. The bridge
            // wraps each entry in a `BlobAttachment` and includes
            // them in `session.sendAndWait`. Empty stays empty —
            // serde skips the field entirely when no images are
            // attached, preserving the pre-0.3.0 single-prompt path.
            let bridge_images: Vec<CopilotBridgeImage> = input
                .images
                .iter()
                .map(|img| CopilotBridgeImage {
                    media_type: img.media_type.clone(),
                    data_base64: img.data_base64.clone(),
                })
                .collect();
            let output = self
                .bridge_request_streaming(
                    &mut process,
                    input.text.clone(),
                    permission_mode,
                    reasoning_effort,
                    bridge_images,
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
        let response =
            tokio::time::timeout(std::time::Duration::from_secs(30), bridge.read_response())
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
        let cached = self.sessions.get(&session.summary.session_id).await;
        let Some(cached) = cached else {
            return Ok(format!(
                "GitHub Copilot interrupt requested for session '{}' (no active bridge).",
                session.summary.session_id
            ));
        };
        let stdin = {
            let guard = cached.inner().lock().await;
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
                enabled: matches!(m.status.as_deref(), Some("connected") | Some("pending")),
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

    /// Daemon-shutdown hook: kill every cached Copilot SDK bridge
    /// child. Mirrors `invalidate_session` but sweeps the whole cache
    /// in one pass so `graceful_shutdown` reaps the bridges without
    /// relying on Drop timing.
    async fn shutdown(&self) {
        for (session_id, cached) in self.sessions.drain_all().await {
            let mut process = cached.inner().lock().await;
            if let Err(e) = process.child.start_kill() {
                debug!(
                    %session_id,
                    "github-copilot shutdown: start_kill failed (child likely already exited): {e}"
                );
            }
        }
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
        (
            Vec<BridgeSkill>,
            Vec<BridgeCopilotAgent>,
            Vec<BridgeCopilotMcp>,
        ),
        String,
    > {
        let cached = self.ensure_session_process(session).await?;
        let _guard = cached.activity_guard();
        let mut process = cached.inner().lock().await;
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
