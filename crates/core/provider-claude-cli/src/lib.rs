use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;
use tracing::{debug, info, warn};
use zenui_provider_api::{
    CommandCatalog, CommandKind, McpServerInfo, PermissionDecision, PermissionMode,
    ProbeCliOptions, ProviderAdapter, ProviderAgent, ProviderCommand, ProviderKind, ProviderModel,
    ProviderSessionState, ProviderStatus, ProviderTurnEvent, ProviderTurnOutput, RateLimitInfo,
    RateLimitStatus, ReasoningEffort, SessionDetail, SkillSource, TokenUsage, TurnEventSink,
    UserInput, UserInputQuestion, claude_bucket_label, claude_file_change_from_tool_call,
    parse_options_from_value, probe_cli, session_cwd, skills_disk,
};

const TURN_TIMEOUT_SECS: u64 = 600;

// ── subprocess handle ────────────────────────────────────────────────────────

struct ClaudeCliProcess {
    child: Child,
    stdin: Arc<Mutex<ChildStdin>>,
    stdout: Lines<BufReader<ChildStdout>>,
}

// ── stream-json events emitted by the CLI on stdout ─────────────────────────

/// Represents the `event` payload inside a `stream_event` message.
/// Only the fields we care about are captured; the rest are ignored.
#[derive(Debug, Deserialize)]
struct RawStreamEvent {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    delta: Option<Value>,
}

/// Anthropic's `result.usage` shape, as emitted by the CLI's
/// stream-json output. Mirrored from SDKResultMessage.usage.
#[derive(Debug, Deserialize)]
struct CliUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: Option<u64>,
    #[serde(default)]
    cache_read_input_tokens: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CliModelUsage {
    #[serde(default)]
    context_window: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CliRateLimitInfo {
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    rate_limit_type: Option<String>,
    #[serde(default)]
    utilization: Option<f64>,
    #[serde(default)]
    resets_at: Option<i64>,
    #[serde(default)]
    is_using_overage: Option<bool>,
}

/// Shape of an `mcp_servers[]` entry inside the init payload — name +
/// authentication status. `status` is a best-effort string ("connected",
/// "needs-auth", "failed", …) we expose as the `enabled` flag.
#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct CliMcpServer {
    name: String,
    status: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum CliEvent {
    /// Initialisation and other system-level messages from the CLI.
    ///
    /// When `subtype == "init"`, the payload also carries
    /// `slash_commands`, `agents`, `skills`, and `mcp_servers`. All are
    /// optional and default to empty so `subtype != "init"` messages
    /// (status updates, compacted events, …) continue to parse.
    System {
        #[serde(default)]
        subtype: Option<String>,
        #[serde(default)]
        session_id: Option<String>,
        /// Every slash command the CLI exposes — a superset of
        /// `skills`. Entries not in `skills` are provider-native
        /// built-ins (e.g. `compact`, `context`, `cost`).
        #[serde(default)]
        slash_commands: Vec<String>,
        /// Sub-agents the CLI was started with. Names only; the init
        /// payload doesn't include descriptions.
        #[serde(default)]
        agents: Vec<String>,
        /// Subset of `slash_commands` that are user-authored SKILL.md
        /// entries. Used to tag them as UserSkill in the catalog.
        #[serde(default)]
        skills: Vec<String>,
        #[serde(default)]
        mcp_servers: Vec<CliMcpServer>,
    },
    /// Incremental stream events carrying individual content-block deltas
    /// (text tokens, thinking tokens). These arrive *before* the final
    /// `assistant` message and should be used for live streaming.
    StreamEvent {
        event: RawStreamEvent,
        #[serde(default)]
        #[allow(dead_code)]
        parent_tool_use_id: Option<String>,
        #[serde(default)]
        session_id: Option<String>,
    },
    /// The complete assembled assistant message. Text blocks were already
    /// emitted via `stream_event`, so we only process `tool_use` blocks here.
    Assistant {
        message: Value,
        #[serde(default)]
        session_id: Option<String>,
    },
    /// User-role messages (tool results etc.)
    User {
        message: Value,
        #[serde(default)]
        session_id: Option<String>,
    },
    /// Final turn result. Subtype is `"success"` on success; error subtypes are
    /// `"error_during_execution"`, `"error_max_turns"`, `"error_max_budget_usd"`,
    /// `"error_max_structured_output_retries"`, or the legacy `"error"`.
    Result {
        subtype: String,
        #[serde(default)]
        session_id: Option<String>,
        #[serde(default)]
        result: Option<String>,
        #[serde(default)]
        errors: Option<Vec<String>>,
        #[serde(default)]
        is_error: Option<bool>,
        /// Anthropic's per-turn token breakdown. Same shape the SDK
        /// bridge sees in SDKResultMessage.usage.
        #[serde(default)]
        usage: Option<CliUsage>,
        /// Keyed by model id. We pick the first (only) entry for
        /// contextWindow.
        #[serde(default, rename = "modelUsage")]
        model_usage: Option<HashMap<String, CliModelUsage>>,
        #[serde(default)]
        total_cost_usd: Option<f64>,
        #[serde(default)]
        duration_ms: Option<u64>,
    },
    /// Old-style per-tool permission prompt (some CLI versions / bridge mode).
    PermissionRequest {
        request_id: String,
        tool_name: String,
        tool_input: Value,
        #[serde(default)]
        #[allow(dead_code)]
        is_mcp_tool: Option<bool>,
    },
    /// New-style control channel request (Claude CLI ≥ 1.x).
    /// `request.subtype == "can_use_tool"` is the tool-permission variant.
    ControlRequest { request_id: String, request: Value },
    /// Rate-limit / plan-usage snapshot.
    RateLimitEvent {
        #[serde(default)]
        rate_limit_info: Option<CliRateLimitInfo>,
    },
    /// In-progress status for long-running tools — safe to ignore.
    ToolProgress {},
    /// Authentication status updates — safe to ignore.
    AuthStatus {},
    /// Catch-all for any future event types we don't know about yet.
    #[serde(other)]
    Unknown,
}

// ── adapter ──────────────────────────────────────────────────────────────────

/// Snapshot of the init payload for a single session. Captured the
/// first time the stream-json loop sees a `system/init` line and read
/// back from `session_command_catalog` so the popup shows the CLI's
/// full set of built-in commands and sub-agents.
#[derive(Debug, Clone, Default)]
struct InitSnapshot {
    slash_commands: Vec<String>,
    agents: Vec<String>,
    skills: Vec<String>,
    mcp_servers: Vec<(String, Option<String>)>,
}

#[derive(Clone)]
pub struct ClaudeCliAdapter {
    working_directory: PathBuf,
    /// One active process per session (for interrupt support).
    active_processes: Arc<Mutex<HashMap<String, Arc<Mutex<ClaudeCliProcess>>>>>,
    /// Per-session `system/init` payload cache. Populated inside
    /// `run_turn`'s stream-json loop, read by `session_command_catalog`.
    /// Sessions without a cached snapshot fall back to disk-only results.
    init_cache: Arc<Mutex<HashMap<String, InitSnapshot>>>,
}

impl ClaudeCliAdapter {
    pub fn new(working_directory: PathBuf) -> Self {
        Self {
            working_directory,
            active_processes: Arc::new(Mutex::new(HashMap::new())),
            init_cache: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Locate the `claude` binary. Delegates to the cross-platform
    /// resolver in `zenui-provider-api` which walks PATH (with PATHEXT
    /// on Windows) and falls back to a curated list of Linux/macOS/
    /// Windows install locations. Returns the bare name `"claude"` as
    /// a last resort so `Command::new` still gets a chance to do its
    /// own PATH lookup — the spawn will produce a meaningful "not
    /// found" error if even that fails.
    fn find_claude_binary() -> String {
        zenui_provider_api::find_cli_binary("claude")
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| "claude".to_string())
    }

    fn permission_mode_flag(mode: PermissionMode) -> &'static str {
        match mode {
            PermissionMode::Default => "default",
            PermissionMode::AcceptEdits => "acceptEdits",
            PermissionMode::Plan => "plan",
            PermissionMode::Bypass => "bypassPermissions",
            // Claude CLI hasn't opted in to
            // `supports_auto_permission_mode`, so the UI won't let
            // a user pick this; the arm exists for exhaustiveness.
            // Neutral fallback preserves historical behaviour.
            PermissionMode::Auto => "default",
        }
    }

    async fn spawn_process(
        &self,
        session: &SessionDetail,
        permission_mode: PermissionMode,
    ) -> Result<ClaudeCliProcess, String> {
        let binary = Self::find_claude_binary();
        let mut args = vec![
            "--output-format".to_string(),
            "stream-json".to_string(),
            "--input-format".to_string(),
            "stream-json".to_string(),
            "--verbose".to_string(),
            "--permission-prompt-tool".to_string(),
            "stdio".to_string(),
            "--permission-mode".to_string(),
            Self::permission_mode_flag(permission_mode).to_string(),
        ];

        if let Some(model) = session.summary.model.as_deref() {
            args.push("--model".to_string());
            args.push(model.to_string());
        }

        // Resume an existing Claude CLI session when we have a persisted id.
        if let Some(native_id) = session
            .provider_state
            .as_ref()
            .and_then(|s| s.native_thread_id.as_deref())
        {
            args.push("--resume".to_string());
            args.push(native_id.to_string());
        }

        info!("Spawning claude CLI: {} {:?}", binary, args);

        let cwd = session_cwd(session, &self.working_directory);
        let mut child = Command::new(&binary)
            .args(&args)
            .current_dir(&cwd)
            .env("CLAUDE_CODE_ENTRYPOINT", "zenui")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_ASKPASS", "")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("Failed to spawn claude CLI ('{binary}'): {e}"))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "claude CLI stdin unavailable".to_string())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "claude CLI stdout unavailable".to_string())?;

        // Drain stderr to logs.
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let trimmed = line.trim();
                    if !trimmed.is_empty() {
                        debug!(target: "provider-claude-cli", "{}", trimmed);
                    }
                }
            });
        }

        Ok(ClaudeCliProcess {
            child,
            stdin: Arc::new(Mutex::new(stdin)),
            stdout: BufReader::new(stdout).lines(),
        })
    }

    async fn run_turn(
        &self,
        session_id: String,
        process: Arc<Mutex<ClaudeCliProcess>>,
        input: String,
        events: TurnEventSink,
    ) -> Result<ProviderTurnOutput, String> {
        let mut proc = process.lock().await;

        // Send the user message.
        let user_msg = serde_json::json!({
            "type": "user",
            "message": { "role": "user", "content": input }
        });
        write_line(&proc.stdin, &user_msg).await?;

        let mut accumulated_output = String::new();
        let mut cli_session_id: Option<String> = None;
        // Track whether any stream_event text arrived. When false (raw CLI without
        // partial-messages support) we fall back to emitting text from the final
        // `assistant` message so the UI always receives a delta.
        let mut has_stream_events = false;

        let deadline =
            tokio::time::Instant::now() + std::time::Duration::from_secs(TURN_TIMEOUT_SECS);

        let result = loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break Err("Claude CLI turn timed out".to_string());
            }

            let line = match tokio::time::timeout(remaining, proc.stdout.next_line()).await {
                Ok(Ok(Some(line))) => line,
                Ok(Ok(None)) => break Err("Claude CLI process exited before result".to_string()),
                Ok(Err(e)) => break Err(format!("Failed to read from claude CLI: {e}")),
                Err(_) => break Err("Claude CLI turn timed out".to_string()),
            };

            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            let event: CliEvent = match serde_json::from_str(trimmed) {
                Ok(e) => e,
                Err(e) => {
                    debug!("claude CLI: unparseable line ({}): {}", e, trimmed);
                    continue;
                }
            };

            match event {
                CliEvent::System {
                    subtype,
                    session_id: sid,
                    slash_commands,
                    agents,
                    skills,
                    mcp_servers,
                } => {
                    if let Some(sub) = &subtype {
                        debug!("claude CLI system: subtype={sub}");
                    }
                    if let Some(id) = sid {
                        cli_session_id = Some(id);
                    }
                    // The init payload arrives once per subprocess,
                    // keyed under `subtype = "init"`. Cache it under
                    // our session id so `session_command_catalog` can
                    // merge built-ins with the disk scan without
                    // spawning a dedicated probe.
                    if subtype.as_deref() == Some("init")
                        && (!slash_commands.is_empty()
                            || !agents.is_empty()
                            || !mcp_servers.is_empty())
                    {
                        let snapshot = InitSnapshot {
                            slash_commands,
                            agents,
                            skills,
                            mcp_servers: mcp_servers
                                .into_iter()
                                .map(|m| (m.name, m.status))
                                .collect(),
                        };
                        let mut guard = self.init_cache.lock().await;
                        guard.insert(session_id.clone(), snapshot);
                    }
                }

                // Token-by-token streaming — text and reasoning deltas.
                // Only emitted when the CLI runs with partial-message support.
                CliEvent::StreamEvent {
                    event: raw,
                    session_id: sid,
                    ..
                } => {
                    if let Some(id) = sid {
                        cli_session_id = Some(id);
                    }
                    if raw.kind == "content_block_delta" {
                        if let Some(delta) = raw.delta {
                            let dtype = delta.get("type").and_then(Value::as_str);
                            match dtype {
                                Some("text_delta") => {
                                    if let Some(text) = delta.get("text").and_then(Value::as_str) {
                                        if !text.is_empty() {
                                            has_stream_events = true;
                                            accumulated_output.push_str(text);
                                            events
                                                .send(ProviderTurnEvent::AssistantTextDelta {
                                                    delta: text.to_string(),
                                                })
                                                .await;
                                        }
                                    }
                                }
                                Some("thinking_delta") => {
                                    if let Some(thinking) =
                                        delta.get("thinking").and_then(Value::as_str)
                                    {
                                        if !thinking.is_empty() {
                                            has_stream_events = true;
                                            events
                                                .send(ProviderTurnEvent::ReasoningDelta {
                                                    delta: thinking.to_string(),
                                                })
                                                .await;
                                        }
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                }

                // Complete assembled assistant message.
                // • When stream_event deltas arrived: text blocks were already
                //   streamed — skip them to avoid duplication, process tool_use only.
                // • When no stream_events (raw CLI without partial-messages):
                //   emit text blocks as a single delta so the UI always gets content.
                CliEvent::Assistant {
                    message,
                    session_id: sid,
                } => {
                    if let Some(id) = sid {
                        cli_session_id = Some(id);
                    }
                    if let Some(content) = message.get("content").and_then(Value::as_array) {
                        for block in content {
                            let btype = block.get("type").and_then(Value::as_str);

                            // Fallback streaming: emit text when no stream_events came.
                            if btype == Some("text") && !has_stream_events {
                                if let Some(text) = block.get("text").and_then(Value::as_str) {
                                    if !text.is_empty() {
                                        accumulated_output.push_str(text);
                                        events
                                            .send(ProviderTurnEvent::AssistantTextDelta {
                                                delta: text.to_string(),
                                            })
                                            .await;
                                    }
                                }
                                continue;
                            }

                            if btype != Some("tool_use") {
                                continue;
                            }
                            let call_id = block
                                .get("id")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string();
                            let name = block
                                .get("name")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string();
                            let args = block
                                .get("input")
                                .cloned()
                                .unwrap_or(Value::Object(Default::default()));

                            events
                                .send(ProviderTurnEvent::ToolCallStarted {
                                    call_id: call_id.clone(),
                                    name: name.clone(),
                                    args: args.clone(),
                                    parent_call_id: None,
                                })
                                .await;

                            // Emit a structured file-change event for
                            // edit-shaped tools (Write / Edit / MultiEdit
                            // / NotebookEdit). The mapping lives in
                            // provider-api so all Claude adapters stay
                            // in lockstep.
                            if let Some(file_change) =
                                claude_file_change_from_tool_call(&call_id, &name, &args)
                            {
                                events.send(file_change).await;
                            }
                        }
                    }
                }

                CliEvent::User {
                    message,
                    session_id: sid,
                } => {
                    if let Some(id) = sid {
                        cli_session_id = Some(id);
                    }
                    if let Some(content) = message.get("content").and_then(Value::as_array) {
                        for block in content {
                            if block.get("type").and_then(Value::as_str) == Some("tool_result") {
                                let call_id = block
                                    .get("tool_use_id")
                                    .and_then(Value::as_str)
                                    .unwrap_or("")
                                    .to_string();
                                let output = extract_tool_result_text(block);
                                let error = if block
                                    .get("is_error")
                                    .and_then(Value::as_bool)
                                    .unwrap_or(false)
                                {
                                    Some(output.clone())
                                } else {
                                    None
                                };
                                events
                                    .send(ProviderTurnEvent::ToolCallCompleted {
                                        call_id,
                                        output,
                                        error,
                                    })
                                    .await;
                            }
                        }
                    }
                }

                CliEvent::PermissionRequest {
                    request_id,
                    tool_name,
                    tool_input,
                    ..
                } => {
                    info!("permission_request: tool_name={tool_name} request_id={request_id}");
                    // AskUserQuestion is Claude's built-in clarifying-question tool.
                    // Route it to the structured question dialog instead of the
                    // yes/no permission dialog, then embed the answers back in the
                    // permission response as `updated_input` (matches the SDK contract).
                    if tool_name == "AskUserQuestion" {
                        warn!(
                            "AskUserQuestion permission_request received, tool_input: {}",
                            tool_input
                        );
                        let questions = parse_ask_user_questions(&tool_input);
                        warn!(
                            "AskUserQuestion: parsed {} questions, showing dialog",
                            questions.len()
                        );
                        let response = match events.ask_user(questions.clone()).await {
                            Some(answers) => {
                                let updated = build_updated_input(&questions, &answers);
                                serde_json::json!({
                                    "type": "permission_response",
                                    "request_id": request_id,
                                    "granted": true,
                                    "updated_input": updated,
                                })
                            }
                            None => {
                                // User dismissed the dialog.
                                serde_json::json!({
                                    "type": "permission_response",
                                    "request_id": request_id,
                                    "granted": false,
                                })
                            }
                        };
                        if let Err(e) = write_line(&proc.stdin, &response).await {
                            warn!("Failed to write AskUserQuestion response: {e}");
                        }
                    } else {
                        // The legacy Claude CLI permission channel doesn't
                        // carry a mode switch; drop the mode_override half of
                        // the tuple.
                        let (decision, _mode_override) = events
                            .request_permission(tool_name, tool_input, PermissionDecision::Allow)
                            .await;
                        let granted = matches!(
                            decision,
                            PermissionDecision::Allow | PermissionDecision::AllowAlways
                        );
                        let response = serde_json::json!({
                            "type": "permission_response",
                            "request_id": request_id,
                            "granted": granted,
                        });
                        if let Err(e) = write_line(&proc.stdin, &response).await {
                            warn!("Failed to write permission response: {e}");
                        }
                    }
                }

                // New-style control channel: `can_use_tool` replaces `permission_request`
                // in Claude CLI ≥ 1.x when running in stream-json mode.
                CliEvent::ControlRequest {
                    request_id,
                    request,
                } => {
                    let subtype = request.get("subtype").and_then(Value::as_str).unwrap_or("");

                    match subtype {
                        "can_use_tool" => {
                            let tool_name = request
                                .get("tool_name")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string();
                            let tool_input = request
                                .get("input")
                                .cloned()
                                .unwrap_or(Value::Object(Default::default()));

                            info!(
                                "control_request can_use_tool: {tool_name} request_id={request_id}"
                            );

                            if tool_name == "AskUserQuestion" {
                                warn!("AskUserQuestion control_request received, showing dialog");
                                let questions = parse_ask_user_questions(&tool_input);
                                warn!("AskUserQuestion: parsed {} questions", questions.len());

                                let response = match events.ask_user(questions.clone()).await {
                                    Some(answers) => {
                                        let updated = build_updated_input(&questions, &answers);
                                        control_success(&request_id, updated)
                                    }
                                    None => {
                                        control_error(&request_id, "User dismissed the question")
                                    }
                                };
                                if let Err(e) = write_line(&proc.stdin, &response).await {
                                    warn!("Failed to write AskUserQuestion control_response: {e}");
                                }
                            } else {
                                // control_request path doesn't thread a mode
                                // switch either; drop the mode_override half.
                                let (decision, _mode_override) = events
                                    .request_permission(
                                        tool_name,
                                        tool_input.clone(),
                                        PermissionDecision::Allow,
                                    )
                                    .await;
                                let response = if matches!(
                                    decision,
                                    PermissionDecision::Allow | PermissionDecision::AllowAlways
                                ) {
                                    // Echo the original tool_input back as
                                    // updatedInput. The Claude CLI replaces
                                    // the tool's args with whatever we send
                                    // here, so passing {} would call e.g.
                                    // Bash with command=undefined and crash
                                    // inside the tool with a TypeError.
                                    control_success(
                                        &request_id,
                                        serde_json::json!({
                                            "behavior": "allow",
                                            "updatedInput": tool_input,
                                        }),
                                    )
                                } else {
                                    control_error(&request_id, "User denied")
                                };
                                if let Err(e) = write_line(&proc.stdin, &response).await {
                                    warn!("Failed to write control_response: {e}");
                                }
                            }
                        }
                        _ => {
                            // Unknown subtype — auto-acknowledge so the CLI can continue.
                            debug!("control_request unhandled subtype={subtype}, auto-ack");
                            let ack = serde_json::json!({
                                "type": "control_response",
                                "response": {
                                    "subtype": "success",
                                    "request_id": request_id,
                                }
                            });
                            if let Err(e) = write_line(&proc.stdin, &ack).await {
                                warn!("Failed to write control_response ack: {e}");
                            }
                        }
                    }
                }

                CliEvent::Result {
                    subtype,
                    session_id: sid,
                    result,
                    errors,
                    is_error,
                    usage: result_usage,
                    model_usage,
                    total_cost_usd,
                    duration_ms,
                } => {
                    if let Some(id) = sid {
                        cli_session_id = Some(id);
                    }
                    // Forward token usage before handling the subtype
                    // branch so the runtime-core drain sees TurnUsage
                    // before turn_completed. Skips when the CLI
                    // version didn't populate the usage field.
                    if let Some(u) = result_usage {
                        let (model, ctx_window) = model_usage
                            .as_ref()
                            .and_then(|m| m.iter().next())
                            .map(|(k, v)| (Some(k.clone()), v.context_window))
                            .unwrap_or((None, None));
                        events
                            .send(ProviderTurnEvent::TurnUsage {
                                usage: TokenUsage {
                                    input_tokens: u.input_tokens,
                                    output_tokens: u.output_tokens,
                                    cache_write_tokens: u.cache_creation_input_tokens,
                                    cache_read_tokens: u.cache_read_input_tokens,
                                    context_window: ctx_window,
                                    // claude-cli's stream-json `result` only
                                    // exposes the per-turn aggregate; no
                                    // per-API-call snapshot is available.
                                    // The live context indicator falls back
                                    // to the aggregate sum on this provider.
                                    live_context_tokens: None,
                                    total_cost_usd,
                                    duration_ms,
                                    model,
                                },
                            })
                            .await;
                    }
                    match subtype.as_str() {
                        "success" => {
                            // Prefer the result field from the CLI; fall back to
                            // text accumulated from stream_event deltas.
                            let output = result
                                .filter(|s| !s.is_empty())
                                .unwrap_or_else(|| accumulated_output.clone());
                            break Ok(output);
                        }
                        "interrupted" => {
                            break Ok(accumulated_output.clone());
                        }
                        other => {
                            // All error subtypes: error_during_execution,
                            // error_max_turns, error_max_budget_usd,
                            // error_max_structured_output_retries, legacy "error".
                            let msg = errors
                                .as_deref()
                                .and_then(|e| e.first())
                                .cloned()
                                .or(result)
                                .unwrap_or_else(|| {
                                    format!("Claude CLI turn failed (subtype: {other})")
                                });
                            if is_error.unwrap_or(true) {
                                break Err(msg);
                            } else {
                                break Ok(msg);
                            }
                        }
                    }
                }

                CliEvent::RateLimitEvent { rate_limit_info } => {
                    if let Some(info) = rate_limit_info {
                        let bucket = match info.rate_limit_type {
                            Some(b) => b,
                            None => continue,
                        };
                        let utilization = info.utilization.unwrap_or(0.0);
                        let status = match info.status.as_deref() {
                            Some("allowed_warning") => RateLimitStatus::AllowedWarning,
                            Some("rejected") => RateLimitStatus::Rejected,
                            _ => RateLimitStatus::Allowed,
                        };
                        events
                            .send(ProviderTurnEvent::RateLimitUpdated {
                                info: RateLimitInfo {
                                    label: claude_bucket_label(&bucket),
                                    bucket,
                                    status,
                                    utilization,
                                    resets_at: info.resets_at,
                                    is_using_overage: info.is_using_overage.unwrap_or(false),
                                },
                            })
                            .await;
                    }
                }
                // Safe to ignore.
                CliEvent::ToolProgress {} | CliEvent::AuthStatus {} => {}
                CliEvent::Unknown => {
                    // Log the raw line so we can identify event types we're missing.
                    warn!(
                        "claude-cli: unknown event type in line (ignored): {}",
                        trimmed
                    );
                }
            }
        };

        // Process is done (or errored) — remove from active map.
        drop(proc);
        self.active_processes.lock().await.remove(&session_id);

        let output = result?;
        let provider_state = cli_session_id.map(|id| ProviderSessionState {
            native_thread_id: Some(id),
            metadata: None,
        });

        Ok(ProviderTurnOutput {
            output,
            provider_state,
        })
    }
}

#[async_trait]
impl ProviderAdapter for ClaudeCliAdapter {
    fn kind(&self) -> ProviderKind {
        ProviderKind::ClaudeCli
    }

    fn default_enabled(&self) -> bool {
        false
    }

    async fn health(&self) -> ProviderStatus {
        let binary = Self::find_claude_binary();
        probe_cli(ProbeCliOptions {
            kind: ProviderKind::ClaudeCli,
            binary: &binary,
            version_args: &["--version"],
            auth_args: &["auth", "status"],
            models: claude_cli_models(),
            // Claude CLI is a black-box subprocess — none of the
            // cross-provider enrichment features (status, tool
            // progress, recap, etc.) map to its wire format.
            features: zenui_provider_api::ProviderFeatures::default(),
            install_hint: Some("Install with: npm install -g @anthropic-ai/claude-code"),
            auth_hint: Some("Run: claude auth login"),
            // Older claude CLIs may not ship an `auth status`
            // subcommand; if the launch itself fails we still let the
            // user try a turn rather than blocking on a probing
            // capability that doesn't exist.
            auth_err_is_ok: true,
        })
        .await
    }

    async fn fetch_models(&self) -> Result<Vec<ProviderModel>, String> {
        Ok(claude_cli_models())
    }

    async fn execute_turn(
        &self,
        session: &SessionDetail,
        input: &UserInput,
        permission_mode: PermissionMode,
        _reasoning_effort: Option<ReasoningEffort>,
        events: TurnEventSink,
    ) -> Result<ProviderTurnOutput, String> {
        if !input.images.is_empty() {
            tracing::warn!(
                provider = ?ProviderKind::ClaudeCli,
                count = input.images.len(),
                "claude CLI adapter dropping image attachments; not implemented"
            );
        }
        let process = self.spawn_process(session, permission_mode).await?;
        let process = Arc::new(Mutex::new(process));

        let session_id = session.summary.session_id.clone();
        self.active_processes
            .lock()
            .await
            .insert(session_id.clone(), process.clone());

        self.run_turn(session_id, process, input.text.clone(), events)
            .await
    }

    async fn interrupt_turn(&self, session: &SessionDetail) -> Result<String, String> {
        let session_id = &session.summary.session_id;
        let process = self.active_processes.lock().await.get(session_id).cloned();

        if let Some(process) = process {
            let proc = process.lock().await;
            let interrupt = serde_json::json!({ "type": "interrupt" });
            if let Err(e) = write_line(&proc.stdin, &interrupt).await {
                warn!("Failed to send interrupt to claude CLI: {e}");
            }
        }

        Ok("Interrupt sent to Claude CLI.".to_string())
    }

    async fn end_session(&self, session: &SessionDetail) -> Result<(), String> {
        let session_id = &session.summary.session_id;
        let process = self.active_processes.lock().await.remove(session_id);

        if let Some(process) = process {
            let mut proc = process.lock().await;
            let _ = proc.child.start_kill();
        }

        // Drop the init snapshot alongside the process — session is
        // gone, cache entry has no further consumers.
        self.init_cache.lock().await.remove(session_id);

        Ok(())
    }

    /// Build the catalog by merging (a) this session's most recent
    /// `system/init` snapshot, captured inside `run_turn` the first
    /// time the CLI subprocess speaks, with (b) the disk scan shared
    /// by every adapter's default impl.
    ///
    /// On cache miss (no turn has happened yet for this session) the
    /// result is disk-only — the popup still shows user SKILL.md
    /// entries, and the built-ins fill in after the user's first
    /// message completes the round-trip (runtime-core re-triggers
    /// spawn_catalog_refresh on TurnCompleted).
    async fn session_command_catalog(
        &self,
        session: &SessionDetail,
    ) -> Result<CommandCatalog, String> {
        let (home_dirs, project_dirs) = self.skill_scan_roots();
        let cwd = session.cwd.as_deref().map(Path::new);
        let roots = skills_disk::scan_roots_for(home_dirs, project_dirs, cwd);
        let mut commands = skills_disk::scan(&roots, self.kind());

        let snapshot = {
            let guard = self.init_cache.lock().await;
            guard.get(&session.summary.session_id).cloned()
        };
        let mut agents_out: Vec<ProviderAgent> = Vec::new();
        let mut mcp_out: Vec<McpServerInfo> = Vec::new();

        if let Some(snap) = snapshot {
            // Fast name-lookup of disk-side skills so we don't
            // double-list a user skill as a "built-in" just because
            // Claude lists it in `slash_commands`.
            let disk_names: std::collections::HashSet<String> =
                commands.iter().map(|c| c.name.clone()).collect();
            let skills_set: std::collections::HashSet<&str> =
                snap.skills.iter().map(String::as_str).collect();

            for name in &snap.slash_commands {
                if disk_names.contains(name) {
                    continue;
                }
                // The CLI flags skill-style entries via its `skills`
                // array; treat everything else as a provider-native
                // built-in. The init payload has no descriptions —
                // show a generic label and let the command itself
                // explain on invocation.
                let is_skill = skills_set.contains(name.as_str());
                let kind = if is_skill {
                    CommandKind::UserSkill {
                        // The CLI doesn't tell us where the skill
                        // lives on disk; default to Global since
                        // that's where unscoped CLI skills usually
                        // live (`~/.claude/`). Project-rooted ones
                        // are picked up by the disk scan above and
                        // take precedence via `disk_names`.
                        source: SkillSource::DiskGlobal,
                    }
                } else {
                    CommandKind::Builtin
                };
                commands.push(ProviderCommand {
                    id: format!("claude_cli:{}:{}", tag_for_kind(kind), name),
                    name: name.clone(),
                    description: match kind {
                        CommandKind::Builtin => "Claude CLI built-in command.".to_string(),
                        CommandKind::UserSkill { .. } => "User skill.".to_string(),
                    },
                    kind,
                    user_invocable: true,
                    arg_hint: None,
                });
            }

            for name in &snap.agents {
                agents_out.push(ProviderAgent {
                    id: format!("claude_cli:agent:{name}"),
                    name: name.clone(),
                    description: "Claude CLI sub-agent.".to_string(),
                });
            }

            for (name, status) in &snap.mcp_servers {
                let enabled = matches!(status.as_deref(), Some("connected") | Some("ready"));
                mcp_out.push(McpServerInfo {
                    id: format!("claude_cli:mcp:{name}"),
                    name: name.clone(),
                    enabled,
                });
            }
        }

        commands.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(CommandCatalog {
            commands,
            agents: agents_out,
            mcp_servers: mcp_out,
        })
    }
}

/// Map a `CommandKind` variant to a short tag used in synthesised ids.
/// Kept alongside the adapter since the init payload only gives us
/// names — ids need a stable disambiguator.
fn tag_for_kind(kind: CommandKind) -> &'static str {
    match kind {
        CommandKind::Builtin => "builtin",
        CommandKind::UserSkill { .. } => "user_skill",
    }
}

// ── helpers ──────────────────────────────────────────────────────────────────

async fn write_line(stdin: &Arc<Mutex<ChildStdin>>, value: &Value) -> Result<(), String> {
    let mut guard = stdin.lock().await;
    zenui_provider_api::write_json_line(&mut *guard, value, "claude CLI").await
}

/// Extract plain text from a tool_result content block.
/// The content field may be a string or an array of text blocks.
fn extract_tool_result_text(block: &Value) -> String {
    match block.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(arr)) => arr
            .iter()
            .filter_map(|b| {
                if b.get("type").and_then(Value::as_str) == Some("text") {
                    b.get("text").and_then(Value::as_str).map(str::to_string)
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Parse the `tool_input` of an `AskUserQuestion` permission request into
/// the canonical `UserInputQuestion` list expected by `TurnEventSink::ask_user`.
///
/// Handles multiple shapes the CLI may send:
/// 1. `{ "questions": [{ "question": "...", "options": [...], "multiSelect": false }] }`
/// 2. `{ "question": "...", "options": [...] }` — flat single-question variant
/// 3. Fallback — construct a freeform question from any text fields found
fn parse_ask_user_questions(tool_input: &Value) -> Vec<UserInputQuestion> {
    // Shape 1: `questions` array.
    if let Some(arr) = tool_input.get("questions").and_then(Value::as_array) {
        if !arr.is_empty() {
            return arr
                .iter()
                .enumerate()
                .map(|(qi, q)| parse_single_question(q, qi))
                .collect();
        }
    }

    // Shape 2: flat single question.
    let text = tool_input
        .get("question")
        .or_else(|| tool_input.get("message"))
        .or_else(|| tool_input.get("text"))
        .and_then(Value::as_str)
        .map(str::to_string);

    if let Some(text) = text {
        let options = parse_options_from_value(tool_input.get("options"), "q0");
        let multi_select = tool_input
            .get("multiSelect")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let allow_freeform = options.is_empty() || options.iter().any(|o| o.label == "Other");
        return vec![UserInputQuestion {
            id: "q0".to_string(),
            text,
            header: tool_input
                .get("header")
                .and_then(Value::as_str)
                .map(str::to_string),
            options,
            multi_select,
            allow_freeform,
            is_secret: false,
        }];
    }

    // Shape 3: fallback — show the raw JSON as a freeform question so the dialog
    // always appears and the user can type a free-form response.
    let raw = serde_json::to_string_pretty(tool_input).unwrap_or_default();
    vec![UserInputQuestion {
        id: "q0".to_string(),
        text: raw,
        header: Some("Claude is asking a question".to_string()),
        options: vec![],
        multi_select: false,
        allow_freeform: true,
        is_secret: false,
    }]
}

fn parse_single_question(q: &Value, qi: usize) -> UserInputQuestion {
    let id = format!("q{qi}");
    let text = q
        .get("question")
        .or_else(|| q.get("text"))
        .or_else(|| q.get("message"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let header = q.get("header").and_then(Value::as_str).map(str::to_string);
    let multi_select = q
        .get("multiSelect")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let options = parse_options_from_value(q.get("options"), &id);
    let allow_freeform = options.is_empty() || options.iter().any(|o| o.label == "Other");
    UserInputQuestion {
        id,
        text,
        header,
        options,
        multi_select,
        allow_freeform,
        is_secret: false,
    }
}

/// Build the `updatedInput` payload for the `control_response` for `AskUserQuestion`.
///
/// Claude CLI expects:
/// ```json
/// { "questions": [...original question objects...],
///   "answers": { "<question text>": "<selected label or freeform text>" } }
/// ```
/// This mirrors the SDK bridge's `answerQuestion` implementation.
fn build_updated_input(
    questions: &[UserInputQuestion],
    answers: &[zenui_provider_api::UserInputAnswer],
) -> Value {
    // Map from question text → answer string, keyed the way Claude expects.
    let mut answer_map = serde_json::Map::new();

    for a in answers {
        let question = questions.iter().find(|q| q.id == a.question_id);

        let answer_text = if !a.answer.is_empty() {
            a.answer.clone()
        } else if let Some(q) = question {
            // Resolve selected option IDs back to their label text.
            let labels: Vec<&str> = q
                .options
                .iter()
                .filter(|o| a.option_ids.contains(&o.id))
                .map(|o| o.label.as_str())
                .collect();
            labels.join(", ")
        } else {
            String::new()
        };

        let question_text = question.map(|q| q.text.as_str()).unwrap_or("");
        answer_map.insert(question_text.to_string(), Value::String(answer_text));
    }

    // Re-serialize original question objects so Claude has full context.
    let raw_questions: Vec<Value> = questions
        .iter()
        .map(|q| {
            let opts: Vec<Value> = q
                .options
                .iter()
                .map(|o| {
                    let mut m = serde_json::Map::new();
                    m.insert("label".to_string(), Value::String(o.label.clone()));
                    if let Some(d) = &o.description {
                        m.insert("description".to_string(), Value::String(d.clone()));
                    }
                    Value::Object(m)
                })
                .collect();

            let mut m = serde_json::Map::new();
            m.insert("question".to_string(), Value::String(q.text.clone()));
            if let Some(h) = &q.header {
                m.insert("header".to_string(), Value::String(h.clone()));
            }
            m.insert("options".to_string(), Value::Array(opts));
            m.insert("multiSelect".to_string(), Value::Bool(q.multi_select));
            Value::Object(m)
        })
        .collect();

    serde_json::json!({
        "behavior": "allow",
        "updatedInput": {
            "questions": raw_questions,
            "answers": Value::Object(answer_map),
        }
    })
}

/// Build a successful `control_response` wrapping the given `PermissionResult`-shaped value.
fn control_success(request_id: &str, permission_result: Value) -> Value {
    serde_json::json!({
        "type": "control_response",
        "response": {
            "subtype": "success",
            "request_id": request_id,
            "response": permission_result,
        }
    })
}

/// Build an error `control_response` (deny / cancel).
fn control_error(request_id: &str, error: &str) -> Value {
    serde_json::json!({
        "type": "control_response",
        "response": {
            "subtype": "error",
            "request_id": request_id,
            "error": error,
        }
    })
}

fn claude_cli_models() -> Vec<ProviderModel> {
    // Matches the capability table in provider-claude-sdk — the CLI
    // reaches the same Anthropic model family.
    vec![
        ProviderModel {
            value: "claude-sonnet-4-6".to_string(),
            label: "Claude Sonnet 4.6".to_string(),
            context_window: Some(200_000),
            max_output_tokens: Some(64_000),
            ..ProviderModel::default()
        },
        ProviderModel {
            value: "claude-opus-4-6".to_string(),
            label: "Claude Opus 4.6".to_string(),
            context_window: Some(200_000),
            max_output_tokens: Some(32_000),
            ..ProviderModel::default()
        },
        ProviderModel {
            value: "claude-sonnet-4-5".to_string(),
            label: "Claude Sonnet 4.5".to_string(),
            context_window: Some(200_000),
            max_output_tokens: Some(64_000),
            ..ProviderModel::default()
        },
        ProviderModel {
            value: "claude-opus-4-5".to_string(),
            label: "Claude Opus 4.5".to_string(),
            context_window: Some(200_000),
            max_output_tokens: Some(32_000),
            ..ProviderModel::default()
        },
        ProviderModel {
            value: "claude-haiku-4-5".to_string(),
            label: "Claude Haiku 4.5".to_string(),
            context_window: Some(200_000),
            max_output_tokens: Some(64_000),
            ..ProviderModel::default()
        },
    ]
}
