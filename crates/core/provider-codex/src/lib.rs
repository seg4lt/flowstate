use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, BufReader, Lines};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::task::JoinHandle;
use tracing::{debug, warn};
use zenui_provider_api::{
    CachedProcess, McpServerConfig, OrchestrationIpcHandle, OrchestrationIpcInfo, PermissionMode,
    ProbeCliOptions, ProcessCache, ProviderAdapter, ProviderKind, ProviderModel,
    ProviderSessionState, ProviderStatus, ProviderTurnEvent, ProviderTurnOutput, ReasoningEffort,
    SessionDetail, TurnEventSink, UserInput, UserInputAnswer, UserInputOption, UserInputQuestion,
    UserMcpRegistry, probe_cli, session_cwd,
};

/// Idle TTL baked into the adapter. A Codex `app-server` process is
/// killed after this many seconds of inactivity (no turn in flight).
/// Hosts can override via [`CodexAdapter::new_with_orchestration_and_idle_ttl`].
const IDLE_TIMEOUT_SECS: u64 = 30 * 60;

/// How often the idle watchdog wakes to scan for stale entries.
const WATCHDOG_INTERVAL_SECS: u64 = 30;

/// Convenience alias — the cached Codex bridge handle returned by
/// `ensure_session_process`.
type CachedCodex = CachedProcess<CodexSessionProcess>;

const REQUEST_TIMEOUT_MS: u64 = 20_000;

const RECOVERABLE_THREAD_RESUME_ERRORS: &[&str] = &[
    "not found",
    "missing thread",
    "no such thread",
    "unknown thread",
    "does not exist",
    "no rollout found",
];

#[derive(Clone)]
pub struct CodexAdapter {
    binary_path: String,
    working_directory: PathBuf,
    /// Shared handle over the runtime's loopback HTTP transport. When
    /// populated, `create_session_process` adds a
    /// `-c mcp_servers.flowstate=…` flag to each Codex spawn so the
    /// agent can call cross-provider orchestration tools. Codex's
    /// `-c` flag accepts TOML fragments that override entries from
    /// `~/.codex/config.toml`; using it session-scoped keeps the
    /// user's global config untouched.
    orchestration: Option<OrchestrationIpcHandle>,
    /// User-defined global MCPs from `~/.flowstate/mcp.json`. Each
    /// stdio entry becomes an additional `-c mcp_servers.<name>=…`
    /// override on the spawn cmdline. http/sse entries are skipped
    /// with a warn (Codex's `-c` TOML override doesn't support
    /// remote transports today). `None` means no user MCPs.
    user_mcp: Option<UserMcpRegistry>,
    sessions: Arc<ProcessCache<CodexSessionProcess>>,
}

#[derive(Debug)]
struct CodexSessionProcess {
    child: Child,
    /// Cross-platform process-group / Job-Object owning the `codex
    /// app-server` subtree. The `Drop` below reaps any per-session
    /// agent workers / MCP subprocesses Codex itself forked. See
    /// `zenui_provider_api::ProcessGroup`.
    process_group: zenui_provider_api::ProcessGroup,
    stdin: ChildStdin,
    stdout: Lines<BufReader<ChildStdout>>,
    stderr_task: JoinHandle<()>,
    next_request_id: u64,
    provider_thread_id: String,
    active_mode: PermissionMode,
}

impl Drop for CodexSessionProcess {
    fn drop(&mut self) {
        self.process_group.kill_best_effort();
        let _ = self.child.start_kill();
        // `stderr_task` is an `AbortHandle`-ish `JoinHandle`; aborting
        // it prevents the drain loop from dangling after the child
        // exits. Non-fatal if the task has already completed.
        self.stderr_task.abort();
    }
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
    /// Construct without cross-provider orchestration wiring.
    pub fn new(working_directory: PathBuf) -> Self {
        Self::new_with_orchestration(working_directory, None, None)
    }

    /// Construct with an optional [`OrchestrationIpcHandle`]. When
    /// populated, every Codex app-server spawn picks up a
    /// `-c mcp_servers.flowstate=…` TOML override registering the
    /// flowstate MCP server for that session. Uses [`IDLE_TIMEOUT_SECS`]
    /// for idle-kill; prefer
    /// [`Self::new_with_orchestration_and_idle_ttl`] when the host has
    /// a user-config store to read the TTL from.
    pub fn new_with_orchestration(
        working_directory: PathBuf,
        orchestration: Option<OrchestrationIpcHandle>,
        user_mcp: Option<UserMcpRegistry>,
    ) -> Self {
        Self::new_with_orchestration_and_idle_ttl(
            working_directory,
            orchestration,
            user_mcp,
            Some(IDLE_TIMEOUT_SECS),
        )
    }

    /// Construct with an optional orchestration handle, user MCP
    /// registry, and an explicit idle-kill timeout. Pass `None` to
    /// disable idle-kill (useful in tests). Pass `Some(secs)` to
    /// override the compiled-in [`IDLE_TIMEOUT_SECS`] default.
    pub fn new_with_orchestration_and_idle_ttl(
        working_directory: PathBuf,
        orchestration: Option<OrchestrationIpcHandle>,
        user_mcp: Option<UserMcpRegistry>,
        idle_timeout_secs: Option<u64>,
    ) -> Self {
        Self {
            binary_path: Self::find_codex_binary(),
            working_directory,
            orchestration,
            user_mcp,
            sessions: Arc::new(ProcessCache::new(
                idle_timeout_secs.unwrap_or(IDLE_TIMEOUT_SECS),
                WATCHDOG_INTERVAL_SECS,
                "provider-codex",
            )),
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

    /// Spawn the idle-kill watchdog exactly once. Called at the top of
    /// `ensure_session_process` so the first session creation arms it.
    fn ensure_watchdog(&self) {
        self.sessions.ensure_watchdog(|cached| async move {
            let mut process = cached.inner().lock().await;
            // Kill the process group first so any MCP subprocesses
            // Codex forked die alongside the app-server.
            process.process_group.kill_best_effort();
            process.stderr_task.abort();
            let _ = process.child.start_kill();
        });
    }

    async fn ensure_session_process(
        &self,
        session: &SessionDetail,
        permission_mode: PermissionMode,
    ) -> Result<CachedCodex, String> {
        self.ensure_watchdog();
        if let Some(existing) = self.sessions.get(&session.summary.session_id).await {
            return Ok(existing);
        }
        let process = self
            .create_session_process(session, permission_mode)
            .await?;
        // Double-check: another task may have inserted while we were
        // spawning. Prefer the winner; Drop on our `process` kills it.
        if let Some(existing) = self.sessions.get(&session.summary.session_id).await {
            return Ok(existing);
        }
        Ok(self
            .sessions
            .insert(session.summary.session_id.clone(), process)
            .await)
    }

    async fn create_session_process(
        &self,
        session: &SessionDetail,
        permission_mode: PermissionMode,
    ) -> Result<CodexSessionProcess, String> {
        let cwd = session_cwd(session, &self.working_directory);
        let mut cmd = Command::new(&self.binary_path);
        cmd.arg("app-server");
        // Cross-provider orchestration: when the Tauri app has
        // mounted the loopback HTTP transport, register the flowstate
        // MCP server with this Codex session via `-c
        // mcp_servers.flowstate=…`. Codex merges this TOML override
        // with `~/.codex/config.toml`, so user-authored MCP servers
        // keep working. Session-scoped (this codex process only).
        if let Some(ipc) = self.orchestration.as_ref().and_then(|h| h.get()) {
            let toml_value = render_flowstate_toml_override(&ipc, &session.summary.session_id);
            cmd.arg("-c")
                .arg(format!("mcp_servers.flowstate={toml_value}"));

            // User-defined MCPs from `~/.flowstate/mcp.json`. One
            // additional `-c mcp_servers.<name>=…` per stdio entry.
            // http/sse entries are skipped with a warn — Codex's
            // `-c` TOML override doesn't accept a `url`-typed remote
            // transport in the version we target. Users who need
            // remote MCPs in Codex specifically must add them to
            // `~/.codex/config.toml` directly.
            if let Some(registry) = &self.user_mcp {
                let snapshot = registry.load();
                for (name, cfg) in &snapshot.servers {
                    match cfg.transport.as_str() {
                        "stdio" => match render_stdio_toml_override(cfg) {
                            Some(toml) => {
                                cmd.arg("-c").arg(format!("mcp_servers.{name}={toml}"));
                            }
                            None => warn!(
                                target: "provider-codex",
                                %name,
                                "skipping user MCP: stdio entry missing command"
                            ),
                        },
                        other => warn!(
                            target: "provider-codex",
                            %name,
                            transport = %other,
                            "skipping user MCP: codex -c overrides only support stdio"
                        ),
                    }
                }
            }
        }
        cmd.current_dir(&cwd)
            // Augment PATH with the user's configured extra search
            // dirs so codex's MCP-server subprocesses (and anything
            // codex itself forks: git, editors, etc.) inherit them.
            .env("PATH", zenui_provider_api::path_with_extras(&[]))
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        // `codex app-server` forks per-session agent workers and MCP
        // subprocesses; place them all in this child's process group
        // / Job Object so `kill_best_effort` at teardown reaps the
        // subtree atomically.
        let mut process_group = zenui_provider_api::ProcessGroup::before_spawn(&mut cmd);
        let mut child = cmd
            .spawn()
            .map_err(|error| format!("failed to launch Codex app-server: {error}"))?;
        process_group.attach(&child);

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
            process_group,
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

    // ---------------------------------------------------------
    // helpers
    // ---------------------------------------------------------

    async fn invalidate_session(&self, session_id: &str) {
        if let Some(cached) = self.sessions.remove(session_id).await {
            let mut process = cached.inner().lock().await;
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
        // No update-available probe: keeps health() fast and
        // immune to subcommands that may block on stdin. The
        // Upgrade button in Settings runs `codex update`
        // unconditionally when the user explicitly clicks it.
        probe_cli(ProbeCliOptions {
            kind: ProviderKind::Codex,
            binary: &self.binary_path,
            version_args: &["--version"],
            auth_args: &["login", "status"],
            models: codex_models(),
            features: zenui_provider_api::features_for_kind(ProviderKind::Codex),
            install_hint: None,
            auth_hint: None,
            auth_err_is_ok: false,
        })
        .await
    }

    async fn upgrade(&self) -> Result<String, String> {
        // Use the CLI's own `codex update` self-update command —
        // matches the `codex update --check` probe shape and means
        // we don't depend on `npm` being on PATH (codex can also be
        // installed via the standalone installer / homebrew). The
        // CLI knows how it was installed and refreshes in-place.
        let mut cmd = tokio::process::Command::new(&self.binary_path);
        zenui_provider_api::hide_console_window_tokio(&mut cmd);
        cmd.env("PATH", zenui_provider_api::path_with_extras(&[]));
        let output = cmd
            .arg("update")
            .output()
            .await
            .map_err(|err| format!("failed to invoke codex update: {err}"))?;
        if output.status.success() {
            Ok("Codex upgraded.".to_string())
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
            Err(if !stderr.is_empty() {
                format!("codex update failed: {stderr}")
            } else if !stdout.is_empty() {
                format!("codex update failed: {stdout}")
            } else {
                format!(
                    "codex update exited with status {:?}",
                    output.status.code()
                )
            })
        }
    }

    async fn fetch_models(&self) -> Result<Vec<ProviderModel>, String> {
        // Spawn an ephemeral codex app-server, run the JSON-RPC handshake, send
        // model/list, parse, and kill the process. The response shape isn't
        // documented anywhere we can audit, so we're defensive: any parsing
        // failure falls back to the hardcoded list.
        let mut cmd = Command::new(&self.binary_path);
        cmd.arg("app-server")
            .current_dir(&self.working_directory)
            .env("PATH", zenui_provider_api::path_with_extras(&[]))
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        let mut process_group = zenui_provider_api::ProcessGroup::before_spawn(&mut cmd);
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("failed to launch codex app-server for model list: {e}"))?;
        process_group.attach(&child);
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
            process_group,
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
        _thinking_mode: Option<zenui_provider_api::ThinkingMode>,
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
        let cached = self
            .ensure_session_process(session, permission_mode)
            .await?;
        {
            let current_mode = cached.inner().lock().await.active_mode;
            if current_mode != permission_mode {
                drop(cached);
                self.invalidate_session(&session.summary.session_id).await;
            }
        }
        let cached = self
            .ensure_session_process(session, permission_mode)
            .await?;

        // Hold the activity guard for the entire turn so the idle
        // watchdog cannot kill this bridge while a turn is in flight.
        // Drops (and stamps last_activity) when `execute_turn` returns.
        let _activity = cached.activity_guard();

        let result = {
            let mut process = cached.inner().lock().await;
            let provider_thread_id = process.provider_thread_id.clone();
            let turn_params = build_turn_start_params(
                &input.text,
                &provider_thread_id,
                reasoning_effort,
                permission_mode,
                session.summary.model.as_deref(),
            );
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
        let cached = self.sessions.get(&session.summary.session_id).await;

        let Some(cached) = cached else {
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

        let mut process = cached.inner().lock().await;
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

    /// Set or update the codex thread's persisted goal.
    ///
    /// Sends `thread/goal/set` with the user's `objective`, `tokenBudget`,
    /// and (optionally) `status` to the per-session `codex app-server`
    /// bridge. Codex returns the resulting `ThreadGoal`; we decode it and
    /// hand it back to the runtime, which publishes
    /// `RuntimeEvent::ThreadGoalUpdated` from the response so the UI
    /// updates immediately rather than waiting for the
    /// `thread/goal/updated` notification round-trip.
    ///
    /// The session bridge is started lazily — the first call here will
    /// spawn `codex app-server` and run the `thread/start` handshake under
    /// `PermissionMode::Default`, mirroring the lazy-spawn behaviour of
    /// `execute_turn`. The next `execute_turn` rebuilds the bridge under
    /// the user-selected permission mode (existing teardown logic in
    /// `execute_turn` already handles mode mismatches).
    async fn set_goal(
        &self,
        session: &SessionDetail,
        objective: String,
        token_budget: Option<i64>,
        status: Option<zenui_provider_api::ThreadGoalStatus>,
    ) -> Result<zenui_provider_api::ThreadGoal, String> {
        let cached = self
            .ensure_session_process(session, PermissionMode::Default)
            .await?;
        let _activity = cached.activity_guard();
        let mut process = cached.inner().lock().await;
        let provider_thread_id = process.provider_thread_id.clone();

        let mut params = json!({
            "threadId": provider_thread_id,
            "objective": objective,
        });
        if let Some(obj) = params.as_object_mut() {
            // tokenBudget is double-Option in the codex protocol:
            // `Some(Some(x))` to set, `Some(None)` to clear, omit to leave
            // alone. Our caller's `Option<i64>` semantically maps to "set
            // or unset"; we send `null` to clear and skip the key on
            // None-from-runtime callers (they'd never reach here, but
            // belt-and-suspenders).
            if let Some(budget) = token_budget {
                obj.insert("tokenBudget".to_string(), json!(budget));
            }
            if let Some(s) = status {
                obj.insert(
                    "status".to_string(),
                    json!(codex_thread_goal_status_str(s)),
                );
            }
        }

        let response = process.send_request("thread/goal/set", params).await?;
        let goal_value = response
            .get("goal")
            .ok_or_else(|| "Codex thread/goal/set response missing `goal`.".to_string())?;
        parse_codex_thread_goal(goal_value)
            .ok_or_else(|| "Codex returned an unparseable ThreadGoal.".to_string())
    }

    async fn clear_goal(&self, session: &SessionDetail) -> Result<(), String> {
        // If no bridge exists yet there's nothing to clear — codex never
        // saw a goal for this session. Treat as success (idempotent).
        let Some(cached) = self.sessions.get(&session.summary.session_id).await else {
            return Ok(());
        };
        let _activity = cached.activity_guard();
        let mut process = cached.inner().lock().await;
        let provider_thread_id = process.provider_thread_id.clone();
        process
            .send_request(
                "thread/goal/clear",
                json!({ "threadId": provider_thread_id }),
            )
            .await?;
        Ok(())
    }

    /// Daemon-shutdown hook: kill every per-session `codex` CLI child
    /// and abort its stderr pump. Mirrors `invalidate_session` but
    /// sweeps the whole map in one pass — the existing Drop on
    /// `CodexSessionProcess` does the same thing, but explicit here
    /// keeps us independent of Arc-refcount timing.
    async fn shutdown(&self) {
        for (session_id, cached) in self.sessions.drain_all().await {
            let mut proc = cached.inner().lock().await;
            proc.stderr_task.abort();
            if let Err(e) = proc.child.start_kill() {
                debug!(
                    %session_id,
                    "codex shutdown: start_kill failed (child likely already exited): {e}"
                );
            }
        }
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
        zenui_provider_api::write_json_line(&mut self.stdin, &value, "Codex app-server").await
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

            let value: Value = serde_json::from_str(trimmed)
                .map_err(|error| format!("received invalid JSON from Codex app-server: {error}"))?;

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
            let parsed: Vec<ProviderModel> = arr.iter().filter_map(extract_model_entry).collect();
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
                allow_freeform: q.get("isOther").and_then(Value::as_bool).unwrap_or(true),
                is_secret: q.get("isSecret").and_then(Value::as_bool).unwrap_or(false),
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

/// Build the `turn/start` JSON-RPC params payload.
///
/// Codex 0.130.0's `TurnStartParams` is `#[serde(rename_all = "camelCase")]`
/// (see `codex-rs/app-server-protocol/src/protocol/v2/turn.rs`), so the
/// top-level reasoning hint is `effort` (NOT `reasoning_effort`). Older
/// flowstate revs sent `reasoning_effort`, which serde silently dropped —
/// the user's selected effort was ignored on every turn.
///
/// Inside `collaborationMode.settings`, however, the nested `Settings`
/// struct (`codex-rs/protocol/src/config_types.rs`) has no `rename_all`,
/// so `reasoning_effort` is still snake_case there. Don't "fix" it.
///
/// Plan mode is gated behind `experimentalApi: true` (already negotiated
/// in `initialize_params()`); leaving `developer_instructions` absent
/// makes codex fill in the plan-preset prompt that drives the
/// `request_user_input` tool.
fn build_turn_start_params(
    input_text: &str,
    provider_thread_id: &str,
    reasoning_effort: Option<ReasoningEffort>,
    permission_mode: PermissionMode,
    session_model: Option<&str>,
) -> Value {
    let effort_str = reasoning_effort.unwrap_or(ReasoningEffort::Medium).as_str();
    let mut turn_params = json!({
        "input": [{
            "text": input_text,
            "text_elements": [],
            "type": "text",
        }],
        "threadId": provider_thread_id,
        "effort": effort_str,
    });
    if permission_mode == PermissionMode::Plan
        && let Some(obj) = turn_params.as_object_mut()
    {
        let model = session_model.unwrap_or("").to_string();
        obj.insert(
            "collaborationMode".to_string(),
            json!({
                "mode": "plan",
                "settings": {
                    "model": model,
                    // Nested Settings uses snake_case (no serde rename).
                    "reasoning_effort": effort_str,
                },
            }),
        );
    }
    turn_params
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
    value
        .get("thread")
        .and_then(|thread| thread.get("id"))
        .and_then(Value::as_str)
        .or_else(|| value.get("threadId").and_then(Value::as_str))
        .map(ToOwned::to_owned)
}

fn extract_turn_id(value: &Value) -> Option<String> {
    value
        .get("turn")
        .and_then(|turn| turn.get("id"))
        .and_then(Value::as_str)
        .or_else(|| value.get("turnId").and_then(Value::as_str))
        .map(ToOwned::to_owned)
}

fn notification_turn_id(value: &Value) -> Option<String> {
    value
        .get("turn")
        .and_then(|turn| turn.get("id"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

fn notification_turn_status(value: &Value) -> Option<String> {
    value
        .get("turn")
        .and_then(|turn| turn.get("status"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

fn notification_turn_error(value: &Value) -> Option<String> {
    value
        .get("turn")
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

/// Render the flowstate MCP server entry as a TOML inline table for
/// Codex's `-c key=value` flag (the value is parsed as TOML). Codex
/// merges the result with `mcp_servers.flowstate` from
/// `~/.codex/config.toml`, so pass-through precedence is "session
/// override wins."
///
/// We emit a single inline table string rather than multiple `-c`
/// flags because Codex tokenises each `-c` independently — using
/// one `-c mcp_servers.flowstate={…}` keeps every field scoped to
/// the same override key and avoids partial-apply bugs if Codex
/// ever rejects one specific field.
///
/// Escapes `\` and `"` inside any field value — the other TOML
/// escape characters (`\b`, `\t`, `\n`, `\f`, `\r`, `\uXXXX`) don't
/// occur in our inputs (URLs, UUIDs, hex tokens, filesystem paths).
fn render_flowstate_toml_override(info: &OrchestrationIpcInfo, session_id: &str) -> String {
    fn escape(s: &str) -> String {
        let mut out = String::with_capacity(s.len() + 2);
        out.push('"');
        for ch in s.chars() {
            match ch {
                '\\' => out.push_str("\\\\"),
                '"' => out.push_str("\\\""),
                c => out.push(c),
            }
        }
        out.push('"');
        out
    }
    let command = escape(&info.executable_path.to_string_lossy());
    let base = escape(&info.base_url);
    let sid = escape(session_id);
    // `pid` is a simple integer — no escaping needed; TOML accepts
    // both `"42"` (string) and `42` (integer) for a string-typed env
    // value, but we render as a TOML string for uniformity with the
    // other env entries.
    let pid = escape(&std::process::id().to_string());
    format!(
        "{{ command = {command}, args = [\"mcp-server\", \"--http-base\", {base}, \
         \"--session-id\", {sid}], env = {{ FLOWSTATE_SESSION_ID = {sid}, \
         FLOWSTATE_HTTP_BASE = {base}, FLOWSTATE_PID = {pid} }} }}"
    )
}

/// Render a user-defined stdio [`McpServerConfig`] as a TOML inline
/// table for Codex's `-c mcp_servers.<name>={…}` flag. Returns
/// `None` if the entry is malformed (missing `command`); the caller
/// then logs and skips. Reuses the same backslash/quote escape rules
/// as [`render_flowstate_toml_override`] — these are the only TOML
/// escapes our user-supplied strings need (no control chars in
/// realistic command paths / args / env values).
fn render_stdio_toml_override(cfg: &McpServerConfig) -> Option<String> {
    fn escape(s: &str) -> String {
        let mut out = String::with_capacity(s.len() + 2);
        out.push('"');
        for ch in s.chars() {
            match ch {
                '\\' => out.push_str("\\\\"),
                '"' => out.push_str("\\\""),
                c => out.push(c),
            }
        }
        out.push('"');
        out
    }
    let command = cfg.command.as_deref()?;
    if command.is_empty() {
        return None;
    }
    let mut out = String::from("{ command = ");
    out.push_str(&escape(command));
    out.push_str(", args = [");
    for (i, a) in cfg.args.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        out.push_str(&escape(a));
    }
    out.push(']');
    if let Some(env) = &cfg.env {
        if !env.is_empty() {
            out.push_str(", env = {");
            for (i, (k, v)) in env.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                // Codex accepts bare-identifier keys for ASCII names;
                // env var names are conventionally `[A-Z_][A-Z0-9_]*`
                // so quoting is unnecessary in practice, but a key
                // with unusual characters would still be safer
                // quoted. We quote unconditionally.
                out.push_str(&escape(k));
                out.push_str(" = ");
                out.push_str(&escape(v));
            }
            out.push('}');
        }
    }
    out.push_str(" }");
    Some(out)
}

#[cfg(test)]
mod codex_mcp_tests {
    use super::*;
    use std::path::PathBuf;

    fn sample_info() -> OrchestrationIpcInfo {
        OrchestrationIpcInfo {
            base_url: "http://127.0.0.1:54321".to_string(),
            executable_path: PathBuf::from("/Applications/flowstate.app/Contents/MacOS/flowstate"),
        }
    }

    #[test]
    fn toml_override_contains_expected_keys() {
        let out = render_flowstate_toml_override(&sample_info(), "sess-1");
        assert!(out.contains("command = \""));
        assert!(out.contains("args = [\"mcp-server\""));
        assert!(out.contains("--http-base"));
        assert!(out.contains("--session-id"));
        assert!(out.contains("env = {"));
        assert!(out.contains("FLOWSTATE_SESSION_ID"));
        assert!(out.contains("FLOWSTATE_HTTP_BASE"));
        assert!(out.contains("FLOWSTATE_PID"));
        assert!(out.contains("sess-1"));
        // No auth token on the loopback — the bind is the boundary.
        assert!(!out.contains("FLOWSTATE_AUTH_TOKEN"));
    }

    #[test]
    fn stdio_user_override_renders_command_and_args() {
        let cfg = McpServerConfig {
            transport: "stdio".to_string(),
            command: Some("/usr/local/bin/srv".to_string()),
            args: vec!["--port".to_string(), "9000".to_string()],
            env: None,
            url: None,
        };
        let out = render_stdio_toml_override(&cfg).expect("stdio override should render");
        assert!(out.contains("command = \"/usr/local/bin/srv\""));
        assert!(out.contains("args = [\"--port\", \"9000\"]"));
        // env block is omitted entirely when there are no entries.
        assert!(!out.contains("env = "));
    }

    #[test]
    fn stdio_user_override_renders_env_when_present() {
        let mut env = std::collections::BTreeMap::new();
        env.insert("FOO".to_string(), "bar".to_string());
        let cfg = McpServerConfig {
            transport: "stdio".to_string(),
            command: Some("srv".to_string()),
            args: vec![],
            env: Some(env),
            url: None,
        };
        let out = render_stdio_toml_override(&cfg).unwrap();
        assert!(out.contains("env = {"));
        assert!(out.contains("\"FOO\" = \"bar\""));
    }

    #[test]
    fn stdio_user_override_returns_none_without_command() {
        let cfg = McpServerConfig {
            transport: "stdio".to_string(),
            command: None,
            args: vec![],
            env: None,
            url: None,
        };
        assert!(render_stdio_toml_override(&cfg).is_none());
    }

    #[test]
    fn stdio_user_override_escapes_special_chars() {
        let cfg = McpServerConfig {
            transport: "stdio".to_string(),
            command: Some("/path with \"quote\" and \\slash".to_string()),
            args: vec!["--arg=\"weird\"".to_string()],
            env: None,
            url: None,
        };
        let out = render_stdio_toml_override(&cfg).unwrap();
        assert!(out.contains("\\\""));
        assert!(out.contains("\\\\"));
    }

    #[test]
    fn toml_override_escapes_quotes_and_backslashes() {
        let info = OrchestrationIpcInfo {
            // Backslash in the URL (e.g. a Windows-style file:// URL
            // someone typed by hand) and a quote in the exe path
            // (pathological but catches the shape of the escaping).
            base_url: "http://evil.example/\\\"attack".to_string(),
            executable_path: PathBuf::from("/weird\"path\\flowstate"),
        };
        let out = render_flowstate_toml_override(&info, "sess");
        // Quotes inside values must be backslash-escaped; backslashes too.
        assert!(out.contains("\\\""));
        assert!(out.contains("\\\\"));
    }
}

/// Regression tests pinned to the codex 0.130.0 app-server JSON-RPC protocol
/// (verified against `codex app-server generate-json-schema`). Each one
/// guards a specific shape change that previously broke the adapter
/// silently — see /Users/babal/.claude/plans/splendid-watching-zephyr.md
/// for the full incident.
#[cfg(test)]
mod codex_protocol_compat_tests {
    use super::*;

    #[test]
    fn turn_start_uses_effort_field_not_reasoning_effort() {
        // codex 0.130.0's TurnStartParams is `#[serde(rename_all = "camelCase")]`
        // so the top-level field is `effort`, not `reasoning_effort`. Sending
        // the snake_case name silently dropped the user's choice.
        let params = build_turn_start_params(
            "hello",
            "thread-1",
            Some(ReasoningEffort::High),
            PermissionMode::Default,
            Some("gpt-5"),
        );
        assert_eq!(
            params.get("effort").and_then(Value::as_str),
            Some("high"),
            "expected top-level `effort` field"
        );
        assert!(
            params.get("reasoning_effort").is_none(),
            "snake_case `reasoning_effort` was renamed away in codex 0.130.0"
        );
        assert_eq!(
            params.get("threadId").and_then(Value::as_str),
            Some("thread-1")
        );
        assert!(params.get("collaborationMode").is_none());
    }

    #[test]
    fn turn_start_plan_mode_keeps_collaboration_mode_with_snake_case_settings() {
        // Inside collaborationMode.settings the nested `Settings` struct in
        // codex (`codex-rs/protocol/src/config_types.rs`) has no
        // rename_all, so `reasoning_effort` is still snake_case there. Don't
        // "fix" it.
        let params = build_turn_start_params(
            "plan it",
            "thread-2",
            Some(ReasoningEffort::Low),
            PermissionMode::Plan,
            Some("gpt-5"),
        );
        let cm = params
            .get("collaborationMode")
            .expect("plan mode must include collaborationMode");
        assert_eq!(cm.get("mode").and_then(Value::as_str), Some("plan"));
        let settings = cm.get("settings").expect("settings");
        assert_eq!(
            settings.get("reasoning_effort").and_then(Value::as_str),
            Some("low"),
            "nested Settings.reasoning_effort stays snake_case"
        );
        assert_eq!(
            settings.get("model").and_then(Value::as_str),
            Some("gpt-5")
        );
        // developer_instructions must be ABSENT so codex auto-fills the
        // plan preset prompt that drives request_user_input.
        assert!(settings.get("developer_instructions").is_none());
    }

    #[test]
    fn file_change_item_emits_one_event_per_change() {
        // codex 0.130.0+ groups multi-file edits into a single `fileChange`
        // item with a `changes: [{path, kind: {type}, diff}]` array.
        let item = json!({
            "id": "fc1",
            "type": "fileChange",
            "status": "applied",
            "changes": [
                {
                    "path": "src/a.rs",
                    "kind": { "type": "update" },
                    "diff": "@@ -1 +1 @@\n-foo\n+bar\n"
                },
                {
                    "path": "src/b.rs",
                    "kind": { "type": "add" },
                    "diff": "@@ -0,0 +1 @@\n+new\n"
                },
                {
                    "path": "src/c.rs",
                    "kind": { "type": "delete" },
                    "diff": "@@ -1 +0,0 @@\n-gone\n"
                }
            ]
        });
        let events = extract_file_changes(&item);
        assert_eq!(events.len(), 3, "one event per change entry");

        let paths: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                ProviderTurnEvent::FileChange { path, .. } => Some(path.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(paths, ["src/a.rs", "src/b.rs", "src/c.rs"]);

        let ops: Vec<zenui_provider_api::FileOperation> = events
            .iter()
            .filter_map(|e| match e {
                ProviderTurnEvent::FileChange { operation, .. } => Some(*operation),
                _ => None,
            })
            .collect();
        assert_eq!(
            ops,
            vec![
                zenui_provider_api::FileOperation::Edit,
                zenui_provider_api::FileOperation::Write,
                zenui_provider_api::FileOperation::Delete,
            ]
        );

        // Per-change call_id must be `<itemId>#<idx>` so the UI dedupes
        // multi-edit items into separate rows.
        let ids: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                ProviderTurnEvent::FileChange { call_id, .. } => Some(call_id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(ids, ["fc1#0", "fc1#1", "fc1#2"]);

        // The unified diff lands in `after` for the UI's diff renderer.
        if let ProviderTurnEvent::FileChange { after, before, .. } = &events[0] {
            assert!(after.as_deref().unwrap_or("").contains("+bar"));
            assert!(before.is_none());
        } else {
            panic!("expected FileChange variant");
        }
    }

    #[test]
    fn file_change_legacy_shape_still_parses() {
        // Pinned-to-old-codex fallback path: single-change `{path, kind,
        // before, after}` directly on the item. Drop after one release.
        let item = json!({
            "id": "fc-legacy",
            "type": "fileChange",
            "path": "old/file.rs",
            "kind": "write",
            "before": "old",
            "after": "new"
        });
        let events = extract_file_changes(&item);
        assert_eq!(events.len(), 1);
        if let ProviderTurnEvent::FileChange {
            call_id,
            path,
            operation,
            before,
            after,
        } = &events[0]
        {
            assert_eq!(call_id, "fc-legacy");
            assert_eq!(path, "old/file.rs");
            assert_eq!(*operation, zenui_provider_api::FileOperation::Write);
            assert_eq!(before.as_deref(), Some("old"));
            assert_eq!(after.as_deref(), Some("new"));
        } else {
            panic!("expected FileChange variant");
        }
    }

    #[test]
    fn parse_codex_thread_goal_round_trip() {
        // Mirrors codex's `ThreadGoalUpdatedNotification.goal` payload
        // (verified against codex 0.130.0's generated JSON Schema).
        let value = json!({
            "threadId": "th_abc",
            "objective": "ship the codex 0.130 fix",
            "status": "active",
            "tokenBudget": 50_000,
            "tokensUsed": 1234,
            "timeUsedSeconds": 90,
            "createdAt": 1_715_000_000_000_i64,
            "updatedAt": 1_715_000_001_000_i64
        });
        let goal = parse_codex_thread_goal(&value).expect("parses");
        assert_eq!(goal.thread_id, "th_abc");
        assert_eq!(goal.objective, "ship the codex 0.130 fix");
        assert_eq!(
            goal.status,
            zenui_provider_api::ThreadGoalStatus::Active
        );
        assert_eq!(goal.token_budget, Some(50_000));
        assert_eq!(goal.tokens_used, 1234);
        assert_eq!(goal.time_used_seconds, 90);
        assert_eq!(goal.created_at, 1_715_000_000_000);
        assert_eq!(goal.updated_at, 1_715_000_001_000);
    }

    #[test]
    fn parse_codex_thread_goal_status_covers_all_variants() {
        // Codex's wire vocabulary is `active|paused|budgetLimited|complete`
        // (camelCase for the multi-word one — confirmed in
        // ThreadGoalUpdatedNotification.json). A typo here silently maps
        // statuses to None which collapses goal events into a no-op
        // `parse_codex_thread_goal` failure — pin every variant.
        assert_eq!(
            parse_codex_thread_goal_status("active"),
            Some(zenui_provider_api::ThreadGoalStatus::Active)
        );
        assert_eq!(
            parse_codex_thread_goal_status("paused"),
            Some(zenui_provider_api::ThreadGoalStatus::Paused)
        );
        assert_eq!(
            parse_codex_thread_goal_status("budgetLimited"),
            Some(zenui_provider_api::ThreadGoalStatus::BudgetLimited)
        );
        assert_eq!(
            parse_codex_thread_goal_status("complete"),
            Some(zenui_provider_api::ThreadGoalStatus::Complete)
        );
        assert_eq!(parse_codex_thread_goal_status("unknown"), None);
        // And the inverse direction matches verbatim.
        assert_eq!(
            codex_thread_goal_status_str(zenui_provider_api::ThreadGoalStatus::BudgetLimited),
            "budgetLimited"
        );
    }

    #[test]
    fn parse_codex_thread_goal_drops_malformed_payloads() {
        // Missing required `objective` → drop instead of building a half
        // goal. The notification handler relies on this returning None
        // so it can suppress the forward.
        let value = json!({
            "threadId": "th_x",
            "status": "active"
        });
        assert!(parse_codex_thread_goal(&value).is_none());

        // Unknown status string → drop.
        let value = json!({
            "threadId": "th_x",
            "objective": "do a thing",
            "status": "frozen"
        });
        assert!(parse_codex_thread_goal(&value).is_none());
    }

    #[test]
    fn collab_agent_tool_call_is_tool_like() {
        // codex 0.130.0 renamed `collabToolCall` → `collabAgentToolCall`.
        // Both must be accepted; display name must come from `tool` (the
        // CollabAgentToolCallThreadItem field) with `toolName` / `name` as
        // fallbacks for older shapes.
        assert!(is_tool_like_item_type("collabAgentToolCall"));
        assert!(is_tool_like_item_type("collabToolCall")); // legacy alias

        let item = json!({
            "id": "c1",
            "type": "collabAgentToolCall",
            "tool": "spawn_subagent",
            "senderThreadId": "t1",
            "receiverThreadIds": ["t2"],
            "status": { "type": "completed" }
        });
        assert_eq!(
            tool_item_display_name(&item, "collabAgentToolCall"),
            "spawn_subagent"
        );
    }
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
            let Some(item) = params.get("item") else {
                return;
            };
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
            let Some(item) = params.get("item") else {
                return;
            };
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
                for fc in extract_file_changes(item) {
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
                            entry.get("step").and_then(Value::as_str).map(|s| {
                                zenui_provider_api::PlanStep {
                                    title: s.to_string(),
                                    detail: entry
                                        .get("status")
                                        .and_then(Value::as_str)
                                        .map(str::to_string),
                                }
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

        // codex `/goal`: persisted thread-level objective with budget
        // tracking. Codex emits `thread/goal/updated` whenever the user
        // sets/pauses/resumes a goal OR the agent calls its `set_goal`
        // model tool, and `thread/goal/cleared` when the goal is
        // dropped. We forward both verbatim so the UI replaces or
        // removes its per-session goal entry.
        //
        // The `goal` payload shape mirrors codex's `ThreadGoal` (see
        // `codex-rs/app-server-protocol/src/protocol/v2/thread.rs:553`
        // at rust-v0.130.0). `parse_codex_thread_goal` returns `None`
        // on malformed payloads — we drop instead of forwarding a
        // half-built event so the UI never renders a partial goal.
        "thread/goal/updated" => {
            if let Some(goal) = params.get("goal").and_then(parse_codex_thread_goal) {
                events
                    .send(ProviderTurnEvent::ThreadGoalUpdated { goal })
                    .await;
            }
        }
        "thread/goal/cleared" => {
            events.send(ProviderTurnEvent::ThreadGoalCleared).await;
        }

        _ => {}
    }
}

/// Decode codex's `ThreadGoal` JSON shape into the cross-provider
/// [`zenui_provider_api::ThreadGoal`]. Returns `None` if any required
/// field is missing — we drop malformed payloads instead of rendering
/// half-built goals.
fn parse_codex_thread_goal(value: &Value) -> Option<zenui_provider_api::ThreadGoal> {
    let obj = value.as_object()?;
    let thread_id = obj.get("threadId").and_then(Value::as_str)?.to_string();
    let objective = obj.get("objective").and_then(Value::as_str)?.to_string();
    let status = obj
        .get("status")
        .and_then(Value::as_str)
        .and_then(parse_codex_thread_goal_status)?;
    let token_budget = obj.get("tokenBudget").and_then(Value::as_i64);
    let tokens_used = obj.get("tokensUsed").and_then(Value::as_i64).unwrap_or(0);
    let time_used_seconds = obj
        .get("timeUsedSeconds")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let created_at = obj.get("createdAt").and_then(Value::as_i64).unwrap_or(0);
    let updated_at = obj.get("updatedAt").and_then(Value::as_i64).unwrap_or(0);
    Some(zenui_provider_api::ThreadGoal {
        thread_id,
        objective,
        status,
        token_budget,
        tokens_used,
        time_used_seconds,
        created_at,
        updated_at,
    })
}

fn parse_codex_thread_goal_status(s: &str) -> Option<zenui_provider_api::ThreadGoalStatus> {
    match s {
        "active" => Some(zenui_provider_api::ThreadGoalStatus::Active),
        "paused" => Some(zenui_provider_api::ThreadGoalStatus::Paused),
        "budgetLimited" => Some(zenui_provider_api::ThreadGoalStatus::BudgetLimited),
        "complete" => Some(zenui_provider_api::ThreadGoalStatus::Complete),
        _ => None,
    }
}

/// Render a [`zenui_provider_api::ThreadGoalStatus`] as the codex
/// `ThreadGoalStatus` wire string. Inverse of
/// [`parse_codex_thread_goal_status`].
fn codex_thread_goal_status_str(status: zenui_provider_api::ThreadGoalStatus) -> &'static str {
    match status {
        zenui_provider_api::ThreadGoalStatus::Active => "active",
        zenui_provider_api::ThreadGoalStatus::Paused => "paused",
        zenui_provider_api::ThreadGoalStatus::BudgetLimited => "budgetLimited",
        zenui_provider_api::ThreadGoalStatus::Complete => "complete",
    }
}

/// Returns true for Codex item types that map to a tool-call-style entry
/// in the UI's work log (commands, file changes, MCP tool invocations, etc).
///
/// `collabAgentToolCall` is the codex 0.130.0+ name for what older CLIs
/// emitted as `collabToolCall`; both are accepted so a user pinned to an
/// older codex isn't broken.
fn is_tool_like_item_type(item_type: &str) -> bool {
    matches!(
        item_type,
        "commandExecution"
            | "fileChange"
            | "mcpToolCall"
            | "dynamicToolCall"
            | "collabToolCall"
            | "collabAgentToolCall"
            | "webSearch"
    )
}

/// Pick the best display name for a tool-call item (e.g., `Bash`, `Write`,
/// or the raw item type if nothing more specific is available).
fn tool_item_display_name(item: &Value, item_type: &str) -> String {
    match item_type {
        "commandExecution" => "Bash".to_string(),
        "fileChange" => "File change".to_string(),
        "mcpToolCall" | "dynamicToolCall" | "collabToolCall" | "collabAgentToolCall" => item
            .get("toolName")
            .or_else(|| item.get("tool"))
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
            let command = item.get("command").and_then(Value::as_str).or_else(|| {
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

/// Translate a Codex `fileChange` item into one or more zenui `FileChange`
/// events.
///
/// Codex 0.130.0+ groups all file edits from a single apply-patch into one
/// `fileChange` item with a `changes: [{ path, kind: { type }, diff }]`
/// array (see `FileChangeThreadItem` in the v2 schema; introduced by codex
/// PR #20540). We emit one `ProviderTurnEvent::FileChange` per entry so the
/// UI's existing per-file renderer keeps working.
///
/// Older codex CLIs (< 0.128) put `path`, `operation`, `before`/`after`
/// directly on the item. We retain a fallback parse for that legacy shape
/// so a user pinned to an older codex isn't broken — drop after one
/// release.
///
/// For each change we fan out:
///  - `kind.type` is one of `add` / `delete` / `update`. Unknown values
///    fall through to `Edit` (the conservative default).
///  - The `after` slot carries the unified `diff` payload (the new
///    protocol no longer ships separate before/after text). The UI's diff
///    renderer accepts a unified diff, so this is the right place for it.
///  - `call_id` becomes `<itemId>#<idx>` so multi-edit items dedupe in
///    the UI rather than collapsing onto one row.
fn extract_file_changes(item: &Value) -> Vec<ProviderTurnEvent> {
    let call_id = item
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    if let Some(changes) = item.get("changes").and_then(Value::as_array) {
        let mut out = Vec::with_capacity(changes.len());
        for (idx, change) in changes.iter().enumerate() {
            let Some(path) = change.get("path").and_then(Value::as_str) else {
                continue;
            };
            // `kind` is an enum-tagged object: `{ "type": "add" | "delete" | "update" }`.
            // Tolerate the legacy bare-string spelling too.
            let kind_str = change
                .get("kind")
                .and_then(|k| k.get("type").and_then(Value::as_str).or_else(|| k.as_str()))
                .unwrap_or("");
            let operation = match kind_str {
                "add" => zenui_provider_api::FileOperation::Write,
                "delete" => zenui_provider_api::FileOperation::Delete,
                _ => zenui_provider_api::FileOperation::Edit,
            };
            let after = change
                .get("diff")
                .and_then(Value::as_str)
                .map(str::to_string);
            let scoped_id = if call_id.is_empty() {
                format!("fileChange#{idx}")
            } else {
                format!("{call_id}#{idx}")
            };
            out.push(ProviderTurnEvent::FileChange {
                call_id: scoped_id,
                path: path.to_string(),
                operation,
                before: None,
                after,
            });
        }
        return out;
    }

    // Legacy single-change shape (codex < 0.128). Drop after one release.
    let Some(path) = item.get("path").and_then(Value::as_str).map(str::to_string) else {
        return Vec::new();
    };
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
    vec![ProviderTurnEvent::FileChange {
        call_id,
        path,
        operation,
        before,
        after,
    }]
}
