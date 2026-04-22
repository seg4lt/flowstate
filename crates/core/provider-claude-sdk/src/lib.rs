mod bridge_runtime;
mod process;
mod rpc;
mod stream;
mod wire;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{ChildStdin, Command};
use tokio::sync::{Mutex, mpsc, oneshot};
use tracing::{debug, info, warn};
use zenui_provider_api::{
    CommandCatalog, CommandKind, McpServerInfo, PermissionDecision, PermissionMode,
    ProviderAdapter, ProviderAgent, ProviderCommand, ProviderKind, ProviderModel,
    ProviderSessionState, ProviderStatus, ProviderStatusLevel, ProviderTurnEvent,
    ProviderTurnOutput, ReasoningEffort, SessionDetail, ThinkingMode, TurnEventSink, UserInput,
    session_cwd, skills_disk,
};

use crate::process::{
    BRIDGE_IDLE_TIMEOUT_SECS, BRIDGE_WATCHDOG_INTERVAL_SECS, CachedBridge, ClaudeBridgeProcess,
    write_request,
};
use crate::rpc::{BridgeRpcKind, BridgeRpcResponse};
use crate::stream::forward_stream;
use crate::wire::{
    BridgeAgent, BridgeCommand, BridgeImageAttachment, BridgeMcpServer, BridgeRequest,
    BridgeResponse, QuestionOutcome, parse_claude_questions, parse_compact_trigger, parse_decision,
    permission_decision_to_str, permission_mode_to_str,
};

pub struct ClaudeSdkAdapter {
    working_directory: PathBuf,
    /// Monotonic counter for mid-turn RPC request IDs. A process-local
    /// counter is sufficient because request_id correlation happens
    /// entirely within this adapter instance (the bridge echoes it
    /// back on the matching response). No cross-process uniqueness is
    /// required.
    rpc_counter: Arc<AtomicU64>,
    sessions: Arc<zenui_provider_api::ProcessCache<ClaudeBridgeProcess>>,
    /// Direct, lock-free-from-outside handles to each session's bridge
    /// stdin. `run_turn` holds the cached process Mutex guard for the
    /// duration of the turn (because it owns `&mut process.stdout`), so
    /// any control message that needs to write to the bridge mid-turn
    /// (interrupt, set_permission_mode, …) would deadlock if it had to
    /// re-lock the same Mutex. Storing a clone of the inner stdin Arc
    /// here lets control paths bypass the process lock entirely; the
    /// inner stdin Mutex still serializes writes against the writer
    /// task inside `run_turn`, so the bridge never sees torn JSON lines.
    session_stdins: Arc<Mutex<HashMap<String, Arc<Mutex<ChildStdin>>>>>,
    /// Parallel handle to each session's pending-RPC map, for the same
    /// reason as `session_stdins`: mid-turn RPC issuers need to insert
    /// a oneshot sender into the map while `run_turn` is holding the
    /// cached process Mutex. The `Arc` points at the same inner map
    /// as the owning `ClaudeBridgeProcess.pending_rpcs`, so the drain
    /// loop and out-of-band RPC callers mutate the same storage.
    session_pending_rpcs: Arc<Mutex<HashMap<String, PendingRpcsMap>>>,
}

type PendingRpcsMap = Arc<Mutex<HashMap<String, oneshot::Sender<BridgeRpcResponse>>>>;

impl ClaudeSdkAdapter {
    pub fn new(working_directory: PathBuf) -> Self {
        Self {
            working_directory,
            rpc_counter: Arc::new(AtomicU64::new(0)),
            sessions: Arc::new(zenui_provider_api::ProcessCache::new(
                BRIDGE_IDLE_TIMEOUT_SECS,
                BRIDGE_WATCHDOG_INTERVAL_SECS,
                "provider-claude-sdk",
            )),
            session_stdins: Arc::new(Mutex::new(HashMap::new())),
            session_pending_rpcs: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Generate a unique request id for a mid-turn RPC. Format is
    /// `rpc-<counter>` — unambiguous within this adapter process,
    /// which is the only scope where correlation matters.
    fn next_rpc_id(&self) -> String {
        let n = self.rpc_counter.fetch_add(1, Ordering::Relaxed);
        format!("rpc-{n}")
    }

    /// Spawn the idle-kill watchdog exactly once via the shared
    /// `ProcessCache` helper. The helper handles the core cache/kill
    /// path; we additionally wipe the sibling `session_stdins` and
    /// `session_pending_rpcs` maps from a pre-kill hook so stale
    /// handles don't accumulate there. That cleanup is approximate
    /// — we don't know the session_id from the shared helper's kill
    /// closure, so we compare the stdin Arc identity in the sibling
    /// maps against the one the watchdog is about to kill. This is
    /// O(sessions) per tick, but `sessions` is bounded by active
    /// users and the watchdog runs at 30s.
    fn ensure_watchdog(&self) {
        let session_stdins = self.session_stdins.clone();
        let session_pending_rpcs = self.session_pending_rpcs.clone();
        self.sessions.ensure_watchdog(move |cached| {
            let session_stdins = session_stdins.clone();
            let session_pending_rpcs = session_pending_rpcs.clone();
            async move {
                let (stdin_arc, pending_arc) = {
                    let p = cached.inner().lock().await;
                    (p.stdin.clone(), p.pending_rpcs.clone())
                };
                {
                    let mut stdins = session_stdins.lock().await;
                    stdins.retain(|_, v| !Arc::ptr_eq(v, &stdin_arc));
                }
                {
                    let mut pending = session_pending_rpcs.lock().await;
                    pending.retain(|_, v| !Arc::ptr_eq(v, &pending_arc));
                }
                let mut process = cached.inner().lock().await;
                // Kill the whole process group first so any `claude`
                // CLI child the SDK forked for tool use dies with
                // the bridge. Drop order on the `ClaudeBridgeProcess`
                // handles the same case on scope exit, but the idle
                // watchdog gets here before the struct is dropped.
                if let Some(pgid) = process.pgid {
                    zenui_provider_api::kill_process_group_best_effort(pgid);
                }
                let _ = process.child.start_kill();
            }
        });
    }

    /// Lookup the per-session stdin handle without ever touching the
    /// outer `sessions` Mutex that `run_turn` holds. Returns `None` if
    /// the session has no live bridge.
    async fn session_stdin(&self, session_id: &str) -> Option<Arc<Mutex<ChildStdin>>> {
        self.session_stdins.lock().await.get(session_id).cloned()
    }

    /// Lookup the per-session pending-RPC map without locking the outer
    /// `sessions` Mutex. Returns `None` if no live bridge exists for
    /// the session — RPC callers treat that as "feature unavailable".
    async fn session_pending_rpcs(&self, session_id: &str) -> Option<PendingRpcsMap> {
        self.session_pending_rpcs
            .lock()
            .await
            .get(session_id)
            .cloned()
    }

    /// Issue a mid-turn RPC to the bridge and await its response via a
    /// oneshot routed through `run_turn`'s drain loop.
    ///
    /// Caller supplies the `BridgeRequest` (already carrying its own
    /// `request_id`) and the expected `kind`. Returns:
    /// - `Ok(Some(payload))` on success
    /// - `Ok(None)` if the session has no live bridge (no active turn)
    /// - `Err(_)` on serialization failure, write error, timeout,
    ///   bridge-reported error, or response-kind mismatch
    ///
    /// Cleanup is thorough: the pending-RPC entry is removed on every
    /// exit path (success, timeout, or write error) so a leaked sender
    /// can't linger across turn boundaries.
    async fn issue_rpc(
        &self,
        session_id: &str,
        request_id: &str,
        request: &BridgeRequest,
        expected_kind: BridgeRpcKind,
        timeout: Duration,
    ) -> Result<Option<Value>, String> {
        // Both halves (stdin + pending map) must exist. Either missing
        // means no live bridge, which the caller translates to the
        // feature-unavailable case.
        let Some(pending_map) = self.session_pending_rpcs(session_id).await else {
            return Ok(None);
        };
        let Some(stdin) = self.session_stdin(session_id).await else {
            return Ok(None);
        };

        let (tx, rx) = oneshot::channel::<BridgeRpcResponse>();
        {
            let mut pending = pending_map.lock().await;
            pending.insert(request_id.to_string(), tx);
        }

        // RAII guard — on any early return / error, remove the pending
        // entry so no stale senders accumulate. The successful path
        // also clears it (via the drain loop's `remove` call before
        // send), so the guard's fallback `remove` is a no-op there.
        struct PendingCleanup<'a> {
            map: &'a PendingRpcsMap,
            request_id: &'a str,
        }
        impl<'a> Drop for PendingCleanup<'a> {
            fn drop(&mut self) {
                // Best-effort clean-up — we can't `.await` in drop, so
                // try_lock; worst case the entry stays and gets
                // garbage-collected on the next invalidate_session.
                if let Ok(mut map) = self.map.try_lock() {
                    map.remove(self.request_id);
                }
            }
        }
        let _cleanup = PendingCleanup {
            map: &pending_map,
            request_id,
        };

        // Ship the request. Write failures propagate as Err — the
        // drain loop never sees the request, so nothing will arrive
        // to resolve our oneshot.
        write_request(&stdin, request).await?;

        // Await the response under a wall-clock timeout. The drain
        // loop sends through the oneshot; a dropped sender (bridge
        // died mid-turn, pending map cleared by invalidate_session)
        // surfaces as RecvError, which we translate to an Err.
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(response)) => {
                if response.kind != expected_kind {
                    return Err(format!(
                        "bridge rpc_response kind mismatch: expected {expected_kind:?}, got {:?}",
                        response.kind
                    ));
                }
                match response.payload {
                    Ok(value) => Ok(Some(value)),
                    Err(err) => Err(err),
                }
            }
            Ok(Err(_)) => Err("bridge closed before rpc response".to_string()),
            Err(_) => Err(format!(
                "bridge rpc_response timed out after {}s",
                timeout.as_secs()
            )),
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
            let sep = if cfg!(windows) { ";" } else { ":" };
            format!("{}{sep}{}", node.bin_dir.display(), existing_path)
        };

        let mut cmd = Command::new(&node.node_bin);
        cmd.arg(&bridge.script)
            .current_dir(&bridge.dir)
            .env("PATH", new_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // Put the Node bridge (and anything it forks) in its own
        // process group so Drop can `killpg` the whole subtree —
        // the SDK can spawn the `claude` CLI internally for tool
        // use, and tokio's `kill_on_drop` only terminates the
        // direct child. See `zenui_provider_api::process_group`.
        zenui_provider_api::enter_own_process_group(&mut cmd);
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("Failed to spawn bridge: {e}"))?;
        let pgid: Option<i32> = child.id().and_then(|p| i32::try_from(p).ok());

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
                        // Log under this crate's module path (rather
                        // than a custom `target:`) so the normal
                        // `zenui=info` env filter catches bridge
                        // stderr lines. Custom targets fall back to
                        // the default warn level and would be dropped.
                        info!("[bridge-stderr] {}", line);
                    }
                }
            });
        }

        let mut process = ClaudeBridgeProcess {
            child,
            pgid,
            stdin: Arc::new(Mutex::new(stdin)),
            stdout: BufReader::new(stdout).lines(),
            bridge_session_id: String::new(),
            // Overwritten by `ensure_session_process` with an Arc that's
            // also cloned into `session_pending_rpcs` so mid-turn RPC
            // callers can insert without re-locking the bridge.
            pending_rpcs: Arc::new(Mutex::new(HashMap::new())),
        };

        debug!("Waiting for bridge ready signal...");
        match tokio::time::timeout(std::time::Duration::from_secs(15), process.read_response())
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

        // Ship the cross-provider orchestration tool catalog to the
        // bridge exactly once, right after ready. The bridge registers
        // each entry with its in-process Claude SDK MCP server, so
        // schemas stay single-sourced in `capabilities.rs` instead of
        // being redeclared as Zod in `bridge/src/index.ts`. Any new
        // variant on `ProviderKind` / `PermissionMode` / etc. or any
        // new orchestration tool shows up to the model without editing
        // the bridge.
        let catalog_request = BridgeRequest::LoadToolCatalog {
            tools: zenui_provider_api::capability_tools_wire(),
        };
        if let Err(err) = write_request(&process.stdin, &catalog_request).await {
            // Non-fatal: older bridges that don't understand the
            // message will log and ignore. A current bridge that
            // expected the catalog and didn't get it will surface a
            // tool-registration error on the first `spawn*` call from
            // the model, which is loud enough to debug.
            warn!(%err, "failed to ship tool catalog to Claude SDK bridge");
        }

        Ok(process)
    }

    async fn ensure_session_process(
        &self,
        session: &SessionDetail,
    ) -> Result<CachedBridge, String> {
        self.ensure_watchdog();
        if let Some(existing) = self.sessions.get(&session.summary.session_id).await {
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

        let response =
            tokio::time::timeout(std::time::Duration::from_secs(30), bridge.read_response())
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

        // Clone the bridge's stdin + pending-rpcs Arcs BEFORE inserting
        // into the cache. Control paths (interrupt, set_permission_mode)
        // and mid-turn RPC issuers read from these parallel maps instead
        // of locking the bridge, so they don't deadlock against run_turn
        // which holds the process lock for the whole turn.
        let stdin_clone = bridge.stdin.clone();
        // Fresh pending-rpcs map installed inside the bridge so the
        // shared `ProcessCache<T>` only needs to track one `T` per slot.
        let pending_rpcs_clone: PendingRpcsMap = Arc::new(Mutex::new(HashMap::new()));
        let mut bridge = bridge;
        bridge.pending_rpcs = pending_rpcs_clone.clone();
        {
            let mut stdins = self.session_stdins.lock().await;
            stdins
                .entry(session.summary.session_id.clone())
                .or_insert(stdin_clone);
        }
        {
            let mut pending = self.session_pending_rpcs.lock().await;
            pending
                .entry(session.summary.session_id.clone())
                .or_insert(pending_rpcs_clone);
        }
        // Double-check for a concurrent insertion before committing.
        if let Some(existing) = self.sessions.get(&session.summary.session_id).await {
            let mut dropped = bridge;
            let _ = dropped.child.start_kill();
            return Ok(existing);
        }
        Ok(self
            .sessions
            .insert(session.summary.session_id.clone(), bridge)
            .await)
    }

    async fn invalidate_session(&self, session_id: &str) {
        // Drop the parallel stdin handle first so any in-flight control
        // request that already cloned it sees its writes fail cleanly
        // when the child process is killed below.
        self.session_stdins.lock().await.remove(session_id);
        // Drop the pending-RPC map too. Any in-flight awaiters will
        // see their oneshot senders dropped; they'll time out on the
        // configured deadline.
        self.session_pending_rpcs.lock().await.remove(session_id);
        if let Some(cached) = self.sessions.remove(session_id).await {
            let mut process = cached.inner().lock().await;
            let _ = process.child.start_kill();
        }
    }

    async fn run_turn(
        &self,
        cached: CachedBridge,
        input: &UserInput,
        permission_mode: PermissionMode,
        reasoning_effort: Option<ReasoningEffort>,
        thinking_mode: Option<ThinkingMode>,
        events: TurnEventSink,
    ) -> Result<(String, Option<String>), String> {
        // Held for the entire turn. Drops after `process` is released,
        // decrementing in_flight and stamping last_activity = now so the
        // 2-minute idle timer starts ticking.
        let _activity = cached.activity_guard();
        let mut process = cached.inner().lock().await;

        let mode_str = permission_mode_to_str(permission_mode);
        let bridge_images: Vec<BridgeImageAttachment> = input
            .images
            .iter()
            .map(|img| BridgeImageAttachment {
                media_type: img.media_type.clone(),
                data_base64: img.data_base64.clone(),
            })
            .collect();
        let request = BridgeRequest::SendPrompt {
            prompt: input.text.clone(),
            permission_mode: mode_str.to_string(),
            reasoning_effort: reasoning_effort.map(|e| e.as_str().to_string()),
            thinking_mode: thinking_mode.map(|m| m.as_str().to_string()),
            images: bridge_images,
        };
        write_request(&process.stdin, &request).await?;

        let stdin = process.stdin.clone();
        let (perm_tx, mut perm_rx) = mpsc::unbounded_channel::<(
            String,
            PermissionDecision,
            Option<PermissionMode>,
            Option<String>,
        )>();
        let (q_tx, mut q_rx) = mpsc::unbounded_channel::<(String, QuestionOutcome)>();
        // Single-item channel the writer task uses to abort the main
        // read loop when it fails to forward a permission / question
        // answer to the bridge. Without this the main loop would keep
        // blocking on read_response while the SDK's canUseTool Promise
        // sits forever on an answer that will never arrive — which is
        // exactly the "card stuck on pending" bug we are fixing.
        let (shutdown_tx, mut shutdown_rx) = mpsc::unbounded_channel::<String>();

        // Background task: forwards permission/question answers from the sink helpers back
        // into the bridge stdin while the turn is in flight.
        let stdin_for_writer = stdin.clone();
        let shutdown_tx_for_writer = shutdown_tx.clone();
        let writer_task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    Some((request_id, decision, mode_override, deny_reason)) = perm_rx.recv() => {
                        let req = BridgeRequest::AnswerPermission {
                            request_id: request_id.clone(),
                            decision: permission_decision_to_str(decision).to_string(),
                            permission_mode: mode_override
                                .map(|m| permission_mode_to_str(m).to_string()),
                            reason: deny_reason,
                        };
                        if let Err(e) = write_request(&stdin_for_writer, &req).await {
                            let msg = format!(
                                "failed to forward permission answer for {request_id} to bridge: {e}"
                            );
                            warn!("{msg}");
                            let _ = shutdown_tx_for_writer.send(msg);
                            break;
                        }
                        info!(
                            bridge_request_id = %request_id,
                            "claude-sdk writer forwarded permission answer to bridge stdin"
                        );
                    }
                    Some((request_id, outcome)) = q_rx.recv() => {
                        let req = match outcome {
                            QuestionOutcome::Answered(answers) => {
                                BridgeRequest::AnswerQuestion { request_id: request_id.clone(), answers }
                            }
                            QuestionOutcome::Cancelled => {
                                BridgeRequest::CancelQuestion { request_id: request_id.clone() }
                            }
                        };
                        if let Err(e) = write_request(&stdin_for_writer, &req).await {
                            let msg = format!(
                                "failed to forward question outcome for {request_id} to bridge: {e}"
                            );
                            warn!("{msg}");
                            let _ = shutdown_tx_for_writer.send(msg);
                            break;
                        }
                    }
                    else => break,
                }
            }
        });

        // Reason supplied by the writer task if it shuts down early
        // because it couldn't forward an answer to the bridge. We
        // capture it here and kill the bridge child *after* the select
        // arm drops so there's no mutable-borrow overlap with the
        // concurrent `process.read_response()` future.
        let mut writer_shutdown_reason: Option<String> = None;

        // No artificial per-turn deadline here on purpose. The model
        // can take as long as it needs to answer; long Bash commands,
        // big edits, slow networks, or just a very chewy prompt are
        // all legitimate reasons for the bridge to stay quiet for
        // many minutes. The user retains the manual escape hatch via
        // the Stop button / Esc key (interrupt_turn), and a real
        // bridge crash still surfaces as `Bridge read error` because
        // `read_response` returns Err on stdout EOF. The only way the
        // bridge silently hangs forever is if the SDK itself
        // deadlocks, which is a bug to fix at the source rather than
        // paper over with a kill-the-bridge timeout.
        let result = loop {
            // Race the bridge stdout against the writer's shutdown
            // signal. `biased` ensures we check the shutdown arm first
            // so a write failure never loses to an incoming bridge
            // line — we always break out with the original reason.
            let line = tokio::select! {
                biased;
                Some(reason) = shutdown_rx.recv() => {
                    writer_shutdown_reason = Some(reason);
                    break Err(String::new());
                }
                read = process.read_response() => {
                    match read {
                        Ok(resp) => resp,
                        Err(e) => break Err(format!("Bridge read error: {e}")),
                    }
                }
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
                    trigger,
                    pre_tokens,
                    post_tokens,
                    duration_ms,
                    summary,
                    mode,
                    memories,
                    phase,
                    attempt,
                    max_retries,
                    retry_delay_ms,
                    error_status,
                    suggestion,
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
                    usage,
                    rate_limit_info,
                    model,
                    elapsed_time_seconds: _elapsed_time_seconds,
                    occurred_at,
                } => {
                    // Log every non-delta stream event so "stuck"
                    // bugs are diagnosable from the log alone: if the
                    // bridge stops emitting after a permission answer,
                    // this is where the silence becomes visible.
                    if !matches!(event.as_str(), "text_delta" | "reasoning_delta") {
                        info!(event = %event, "bridge stream event");
                    }
                    match event.as_str() {
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
                            //
                            // The optional PermissionMode override rides atomically
                            // with the decision through the oneshot in provider-api,
                            // so there is no side channel to read here — the
                            // plan-exit "Approve & Auto-edit" flow Just Works.
                            let events_clone = events.clone();
                            let perm_tx = perm_tx.clone();
                            let req_id_for_writer = request_id;
                            tokio::spawn(async move {
                                let (decision, mode_override, deny_reason) = events_clone
                                    .request_permission(tool_name, input, suggested)
                                    .await;
                                tracing::info!(
                                    bridge_request_id = %req_id_for_writer,
                                    ?decision,
                                    has_mode_override = mode_override.is_some(),
                                    has_deny_reason = deny_reason.is_some(),
                                    "claude-sdk adapter: forwarding permission answer to writer"
                                );
                                let _ = perm_tx.send((
                                    req_id_for_writer,
                                    decision,
                                    mode_override,
                                    deny_reason,
                                ));
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
                        "turn_usage" => {
                            if let Some(u) = usage.and_then(|v| {
                                serde_json::from_value::<zenui_provider_api::TokenUsage>(v).ok()
                            }) {
                                events.send(ProviderTurnEvent::TurnUsage { usage: u }).await;
                            }
                        }
                        "rate_limit_update" => {
                            if let Some(mut info) = rate_limit_info.and_then(|v| {
                                serde_json::from_value::<zenui_provider_api::RateLimitInfo>(v).ok()
                            }) {
                                // Canonicalize the label against the shared
                                // Rust table so the two Claude adapters always
                                // agree on phrasing, even if the bridge's
                                // fallback copy ever drifts (see
                                // `claude_bucket_label`).
                                info.label = zenui_provider_api::claude_bucket_label(&info.bucket);
                                events
                                    .send(ProviderTurnEvent::RateLimitUpdated { info })
                                    .await;
                            }
                        }
                        "model_resolved" => {
                            // The bridge surfaces the SDK's resolved model
                            // from `system.init`. Forward to runtime-core so
                            // `session.summary.model` can be upgraded from an
                            // alias (e.g. `sonnet`) to the pinned id the SDK
                            // actually ran with — the model-selector dropdown
                            // matches on the pinned value and otherwise fails
                            // to highlight the active entry.
                            if let Some(m) = model {
                                if !m.is_empty() {
                                    events
                                        .send(ProviderTurnEvent::ModelResolved { model: m })
                                        .await;
                                }
                            }
                        }
                        "compact_boundary" => {
                            // SDK is compressing older turns. Metrics
                            // arrive here; the paired summary text lands
                            // separately via the PostCompact hook
                            // (`compact_summary` event). Runtime-core
                            // merges the pair into one ContentBlock.
                            let trig = parse_compact_trigger(trigger.as_deref());
                            events
                                .send(ProviderTurnEvent::CompactBoundary {
                                    trigger: trig,
                                    pre_tokens,
                                    post_tokens,
                                    duration_ms,
                                })
                                .await;
                        }
                        "compact_summary" => {
                            let trig = parse_compact_trigger(trigger.as_deref());
                            let text = summary.unwrap_or_default();
                            events
                                .send(ProviderTurnEvent::CompactSummary {
                                    trigger: trig,
                                    summary: text,
                                })
                                .await;
                        }
                        "memory_recall" => {
                            use zenui_provider_api::{
                                MemoryRecallItem, MemoryRecallMode, MemoryRecallScope,
                            };
                            let parsed_mode = match mode.as_deref() {
                                Some("synthesize") => MemoryRecallMode::Synthesize,
                                _ => MemoryRecallMode::Select,
                            };
                            let items: Vec<MemoryRecallItem> = memories
                                .as_ref()
                                .and_then(Value::as_array)
                                .map(|arr| {
                                    arr.iter()
                                        .map(|v| {
                                            let path = v
                                                .get("path")
                                                .and_then(Value::as_str)
                                                .unwrap_or_default()
                                                .to_string();
                                            let scope = match v.get("scope").and_then(Value::as_str)
                                            {
                                                Some("team") => MemoryRecallScope::Team,
                                                _ => MemoryRecallScope::Personal,
                                            };
                                            let content = v
                                                .get("content")
                                                .and_then(Value::as_str)
                                                .map(str::to_string);
                                            MemoryRecallItem {
                                                path,
                                                scope,
                                                content,
                                            }
                                        })
                                        .collect()
                                })
                                .unwrap_or_default();
                            events
                                .send(ProviderTurnEvent::MemoryRecall {
                                    mode: parsed_mode,
                                    memories: items,
                                })
                                .await;
                        }
                        "turn_status" => {
                            use zenui_provider_api::TurnPhase;
                            let parsed_phase = match phase.as_deref() {
                                Some("requesting") => TurnPhase::Requesting,
                                Some("streaming") => TurnPhase::Streaming,
                                Some("compacting") => TurnPhase::Compacting,
                                Some("awaiting_input") => TurnPhase::AwaitingInput,
                                _ => TurnPhase::Idle,
                            };
                            events
                                .send(ProviderTurnEvent::StatusChanged {
                                    phase: parsed_phase,
                                })
                                .await;
                        }
                        "api_retry" => {
                            events
                                .send(ProviderTurnEvent::TurnRetrying {
                                    attempt: attempt.unwrap_or(1),
                                    max_retries: max_retries.unwrap_or(0),
                                    retry_delay_ms: retry_delay_ms.unwrap_or(0),
                                    error_status,
                                    error: error.unwrap_or_default(),
                                })
                                .await;
                        }
                        "prompt_suggestion" => {
                            if let Some(text) = suggestion {
                                if !text.is_empty() {
                                    events
                                        .send(ProviderTurnEvent::PromptSuggestion {
                                            suggestion: text,
                                        })
                                        .await;
                                }
                            }
                        }
                        "tool_progress" => {
                            // Per-tool heartbeat from the SDK. Drives the
                            // stalled-tool pip on the frontend (a per-tool
                            // affordance that's strictly more useful than
                            // the existing 45s session-wide stuck banner).
                            // We require call_id since it's the join key
                            // against the live ToolCall; missing call_id
                            // means we can't attach the heartbeat to a
                            // tool, so drop the event.
                            if let Some(cid) = call_id {
                                // The bridge always stamps `occurred_at`
                                // before emitting (see the
                                // `tool_progress` case in
                                // bridge/src/index.ts), so the
                                // `unwrap_or_default()` is just a JSON-
                                // safety net — runtime-core treats an
                                // empty string the same as no heartbeat
                                // (the staleness check is `!is_empty()`
                                // first, then a parse).
                                events
                                    .send(ProviderTurnEvent::ToolProgress {
                                        call_id: cid,
                                        tool_name: tool_name.unwrap_or_default(),
                                        parent_call_id,
                                        occurred_at: occurred_at.unwrap_or_default(),
                                    })
                                    .await;
                            }
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
                                model,
                            )
                            .await;
                        }
                    }
                }
                BridgeResponse::RpcResponse {
                    request_id,
                    kind,
                    payload,
                    error,
                } => {
                    // Route mid-turn RPC response back to the
                    // waiting caller. Pull the sender out of
                    // the pending map (one-shot — a second
                    // response for the same id would have
                    // nothing to resolve), then dispatch.
                    // `send` fails only if the receiver was
                    // dropped (caller timed out / was cancelled);
                    // that's fine, the pending-map cleanup on
                    // the caller side already removed the entry
                    // and we're just discarding a late reply.
                    let sender_opt = {
                        let mut pending = process.pending_rpcs.lock().await;
                        pending.remove(&request_id)
                    };
                    let payload = match (payload, error) {
                        (_, Some(err)) => Err(err),
                        (Some(value), None) => Ok(value),
                        (None, None) => {
                            Err("bridge rpc_response had neither payload nor error".to_string())
                        }
                    };
                    if let Some(sender) = sender_opt {
                        let _ = sender.send(BridgeRpcResponse { kind, payload });
                    } else {
                        debug!("bridge rpc_response for unknown request_id: {request_id}");
                    }
                }
                BridgeResponse::RuntimeCallRequest {
                    request_id,
                    tool_name,
                    args,
                } => {
                    // The agent called a flowstate_* capability tool.
                    // Parse into a typed RuntimeCall, dispatch through
                    // the sink (which routes to runtime-core), then
                    // write the encoded result back to the bridge so
                    // its pending MCP tool promise resolves. All the
                    // round-trip plumbing lives in `TurnEventSink::runtime_call`
                    // and the bridge RPC — orchestration dispatch is
                    // provider-agnostic by design.
                    let sink = events.clone();
                    let stdin = process.stdin.clone();
                    tokio::spawn(async move {
                        let result = match zenui_provider_api::parse_runtime_call(&tool_name, &args)
                        {
                            Ok(call) => sink.runtime_call(call).await,
                            Err(message) => {
                                Err(zenui_provider_api::RuntimeCallError::Internal { message })
                            }
                        };
                        let (payload, error) = match result {
                            Ok(r) => (Some(zenui_provider_api::encode_runtime_result(&r)), None),
                            Err(e) => (None, Some(zenui_provider_api::encode_runtime_error(&e))),
                        };
                        let resp = BridgeRequest::RuntimeCallResponse {
                            request_id,
                            payload,
                            error,
                        };
                        if let Err(err) = write_request(&stdin, &resp).await {
                            warn!(%err, "failed to write runtime_call_response to bridge");
                        }
                    });
                }
                other => {
                    debug!("Unexpected mid-stream bridge message: {:?}", other);
                }
            }
        };

        // Drain any permission/question oneshots still sitting in the
        // sink's pending maps — e.g. tool calls whose canUseTool
        // Promise was resolved by drainPendingOnAbort on the bridge
        // side during an interrupt, but whose Rust-side spawned task
        // is still awaiting an answer the user will never click.
        // Dropping the Senders wakes those tasks with Err and lets
        // them return so they don't leak. Must happen before we drop
        // the mpsc senders so the tasks can still forward their
        // synthetic Deny to the writer (which will then either write
        // it or the writer will exit naturally).
        events.drain_pending().await;

        // Drain the writer task. Dropping the senders closes the channels and lets it exit.
        drop(perm_tx);
        drop(q_tx);
        let _ = writer_task.await;

        // If the writer tripped its shutdown signal, the turn loop
        // broke with a placeholder Err. Kill the bridge child so its
        // stdout closes (future reads would otherwise hang on a dead
        // pipe), and return a real Err so runtime-core transitions
        // the turn to Failed and publishes a RuntimeEvent::Error.
        if let Some(reason) = writer_shutdown_reason {
            let _ = process.child.start_kill();
            return Err(format!("Claude SDK bridge write path failed: {reason}"));
        }

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
        let features = zenui_provider_api::features_for_kind(ProviderKind::Claude);

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
                models: Vec::new(),
                enabled: true,
                features: features.clone(),
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
                models: Vec::new(),
                enabled: true,
                features: features.clone(),
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
            // Left empty on purpose: the runtime triggers `fetch_models()`
            // shortly after health() and populates the cache from the
            // installed claude binary's SDK init response. Returning a
            // hardcoded fallback here would just mask staleness when
            // Anthropic ships new model slugs.
            models: Vec::new(),
            enabled: true,
            features,
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
        input: &UserInput,
        permission_mode: PermissionMode,
        reasoning_effort: Option<ReasoningEffort>,
        thinking_mode: Option<ThinkingMode>,
        events: TurnEventSink,
    ) -> Result<ProviderTurnOutput, String> {
        let cached = self.ensure_session_process(session).await?;
        let result = self
            .run_turn(
                cached,
                input,
                permission_mode,
                reasoning_effort,
                thinking_mode,
                events,
            )
            .await;

        match result {
            Ok((output, session_id)) => {
                // Prefer the freshly-captured session id from this turn so a
                // resume after restart works. Fall back to whatever was already
                // persisted if the bridge didn't return one (e.g. init failed
                // to carry a session_id in this SDK version).
                //
                // CAREFUL: when we mint a new ProviderSessionState here we
                // intentionally preserve any existing `metadata` blob so
                // future per-session settings aren't wiped by a successful
                // turn.
                let provider_state = session_id
                    .map(|id| ProviderSessionState {
                        native_thread_id: Some(id),
                        metadata: session
                            .provider_state
                            .as_ref()
                            .and_then(|s| s.metadata.clone()),
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

    async fn update_permission_mode(
        &self,
        session: &SessionDetail,
        mode: PermissionMode,
    ) -> Result<(), String> {
        // Forward a set_permission_mode request to the live bridge. The
        // bridge calls `query.setPermissionMode(...)` on its held SDK
        // Query handle, applying the new mode to the rest of the
        // in-flight turn. No-op (Ok) if no bridge exists yet — the
        // runtime will pick up the new mode from the next send_turn.
        //
        // We grab the stdin handle directly from `session_stdins` rather
        // than locking `sessions`, because `run_turn` holds the outer
        // process Mutex for the entire duration of the turn (it owns
        // `&mut process.stdout`). Going through that lock would block
        // us until the turn finished — which is exactly when the user
        // is asking us to switch the mode.
        let Some(stdin) = self.session_stdin(&session.summary.session_id).await else {
            return Ok(());
        };
        write_request(
            &stdin,
            &BridgeRequest::SetPermissionMode {
                permission_mode: permission_mode_to_str(mode).to_string(),
            },
        )
        .await?;
        Ok(())
    }

    async fn update_session_model(
        &self,
        session: &SessionDetail,
        model: String,
    ) -> Result<(), String> {
        // Forward a set_model request to the live bridge so the next
        // query() call uses the new model. Same stdin-grab pattern as
        // update_permission_mode — avoids blocking on the outer process
        // Mutex held during run_turn. No-op if no bridge exists yet;
        // the next ensure_session_process will create one with the model
        // already persisted in session.summary.model.
        let Some(stdin) = self.session_stdin(&session.summary.session_id).await else {
            return Ok(());
        };
        write_request(&stdin, &BridgeRequest::SetModel { model }).await?;
        Ok(())
    }

    async fn interrupt_turn(&self, session: &SessionDetail) -> Result<String, String> {
        // Send an `interrupt` message to the live bridge. The bridge
        // calls `abortController.abort()` on the in-flight SDK query,
        // which returns `'[interrupted]'` and flips `inFlight = false`
        // so the bridge is ready to accept the next send_prompt.
        //
        // We intentionally do NOT invalidate the session — the bridge's
        // in-memory `resumeSessionId` must survive so the next turn
        // resumes the same Claude conversation.
        //
        // Uses `session_stdins` rather than the outer `sessions` lock
        // because `run_turn` holds that outer Mutex for the duration of
        // the turn; trying to re-lock it here would deadlock until the
        // turn naturally finished, which defeats the entire point of
        // interrupt.
        let Some(stdin) = self.session_stdin(&session.summary.session_id).await else {
            return Ok(format!(
                "Claude SDK interrupt requested for session `{}` (no active bridge).",
                session.summary.session_id
            ));
        };
        write_request(&stdin, &BridgeRequest::Interrupt).await?;
        Ok(format!(
            "Claude SDK turn interrupted for session `{}`.",
            session.summary.session_id
        ))
    }

    async fn end_session(&self, session: &SessionDetail) -> Result<(), String> {
        self.invalidate_session(&session.summary.session_id).await;
        Ok(())
    }

    async fn invalidate_process(&self, session: &SessionDetail) -> Result<(), String> {
        // Reaps the bridge subprocess — `native_thread_id` stays in
        // persistence, so the next turn respawns with a fresh Claude
        // Code CLI that resumes the same conversation. Same mechanics
        // as `end_session` above; kept as a separate method because
        // runtime-core only wants the subprocess reap, not full
        // session teardown (catalog refresh, etc.).
        self.invalidate_session(&session.summary.session_id).await;
        Ok(())
    }

    async fn get_context_usage(
        &self,
        session: &SessionDetail,
    ) -> Result<Option<zenui_provider_api::ContextBreakdown>, String> {
        // Only works during an active turn — the SDK's
        // `query.getContextUsage()` is a method on a live Query
        // object. No live bridge means no active query means
        // nothing to query. Return Ok(None) in that case; the
        // frontend feature-gate already hides the popover trigger
        // when `isRunning` is false, but defending against a
        // misrouted click here costs nothing.
        let request_id = self.next_rpc_id();
        let request = BridgeRequest::GetContextUsage {
            request_id: request_id.clone(),
        };
        let raw = match self
            .issue_rpc(
                &session.summary.session_id,
                &request_id,
                &request,
                BridgeRpcKind::ContextUsage,
                Duration::from_secs(15),
            )
            .await?
        {
            Some(value) => value,
            None => return Ok(None),
        };

        // Translate the SDK's raw response into the cross-provider
        // `ContextBreakdown` shape. The SDK ships richer data
        // (grid rows, MCP tool detail, memory file detail); we
        // only pick up what our UI surfaces today (totals +
        // category list). Extra fields we ignore flow through
        // untouched for providers that want them later.
        let total_tokens = raw.get("totalTokens").and_then(Value::as_u64).unwrap_or(0);
        let max_tokens = raw.get("maxTokens").and_then(Value::as_u64).unwrap_or(0);
        let categories: Vec<zenui_provider_api::ContextCategory> = raw
            .get("categories")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .map(|c| zenui_provider_api::ContextCategory {
                        name: c
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        tokens: c.get("tokens").and_then(Value::as_u64).unwrap_or(0),
                        color: c.get("color").and_then(Value::as_str).map(str::to_string),
                    })
                    .collect()
            })
            .unwrap_or_default();

        Ok(Some(zenui_provider_api::ContextBreakdown {
            total_tokens,
            max_tokens,
            categories,
        }))
    }

    async fn fetch_models(&self) -> Result<Vec<ProviderModel>, String> {
        // Spawn an ephemeral bridge process, ask it for the model list, kill it.
        // The bridge calls query() with a noop prompt and reads supportedModels()
        // off the init response — no actual SDK call is made.
        let mut bridge = self.spawn_bridge().await?;
        write_request(&bridge.stdin, &BridgeRequest::ListModels).await?;
        let response =
            tokio::time::timeout(std::time::Duration::from_secs(30), bridge.read_response())
                .await
                .map_err(|_| "Timeout fetching Claude models".to_string())?
                .map_err(|e| format!("Bridge read error: {e}"))?;
        let _ = bridge.child.start_kill();

        match response {
            BridgeResponse::Models { models } => {
                if models.is_empty() {
                    Err("Claude bridge returned no models".to_string())
                } else {
                    // Forward the installed claude binary's `supportedModels()`
                    // response verbatim. `context_window` / `max_output_tokens`
                    // are `None` today because `ModelInfo` in the Claude Agent
                    // SDK doesn't carry them — if a future SDK release adds
                    // those fields, the bridge will forward them and this path
                    // will start populating them for free. No hardcoded fallback.
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

    /// Ask the Claude Agent SDK what slash commands, sub-agents, and
    /// MCP servers are available for this session, and merge the
    /// result with the shared on-disk skill scan.
    ///
    /// The bridge path is the same throwaway `query({ prompt: 'noop' })`
    /// trick as `fetch_models`: no API call is made, init runs
    /// locally, we read the cached capability lists, abort. On any
    /// failure we fall through to a disk-only catalog so the popup
    /// still shows user SKILL.md entries.
    async fn session_command_catalog(
        &self,
        session: &SessionDetail,
    ) -> Result<CommandCatalog, String> {
        let (home_dirs, project_dirs) = self.skill_scan_roots();
        let cwd_path = session.cwd.as_deref().map(Path::new);
        let roots = skills_disk::scan_roots_for(home_dirs, project_dirs, cwd_path);
        let mut commands = skills_disk::scan(&roots, self.kind());

        // Ask the bridge for the SDK's live capability snapshot. If
        // anything goes wrong (bridge spawn, timeout, malformed
        // response) fall back to disk-only — the popup is a UX
        // affordance, not something a failure should propagate.
        let capabilities = self
            .fetch_capabilities(session.cwd.clone(), session.summary.model.clone())
            .await;
        let (sdk_commands, sdk_agents, sdk_mcp) = match capabilities {
            Ok(c) => c,
            Err(err) => {
                warn!("session_command_catalog: falling back to disk-only ({err})");
                (Vec::new(), Vec::new(), Vec::new())
            }
        };

        let disk_names: std::collections::HashSet<String> =
            commands.iter().map(|c| c.name.clone()).collect();
        for sdk in sdk_commands {
            if disk_names.contains(&sdk.name) {
                // The on-disk SKILL.md carries richer metadata
                // (source, real description) — let it win the slot.
                continue;
            }
            commands.push(ProviderCommand {
                id: format!("claude:builtin:{}", sdk.name),
                name: sdk.name,
                description: sdk.description,
                kind: CommandKind::Builtin,
                user_invocable: true,
                arg_hint: sdk.argument_hint,
            });
        }
        commands.sort_by(|a, b| a.name.cmp(&b.name));

        let agents = sdk_agents
            .into_iter()
            .map(|a| ProviderAgent {
                id: format!("claude:agent:{}", a.name),
                name: a.name,
                description: a.description,
            })
            .collect();

        let mcp_servers = sdk_mcp
            .into_iter()
            .map(|m| McpServerInfo {
                enabled: matches!(m.status.as_deref(), Some("connected") | Some("pending")),
                id: format!("claude:mcp:{}", m.name),
                name: m.name,
            })
            .collect();

        Ok(CommandCatalog {
            commands,
            agents,
            mcp_servers,
        })
    }
}

impl ClaudeSdkAdapter {
    /// Spawn an ephemeral bridge, send `list_capabilities`, tear down
    /// the bridge, and surface the parsed lists. Separated from the
    /// trait method so errors are centralised and the trait body stays
    /// focused on the mapping step.
    async fn fetch_capabilities(
        &self,
        cwd: Option<String>,
        model: Option<String>,
    ) -> Result<(Vec<BridgeCommand>, Vec<BridgeAgent>, Vec<BridgeMcpServer>), String> {
        let mut bridge = self.spawn_bridge().await?;
        write_request(
            &bridge.stdin,
            &BridgeRequest::ListCapabilities { cwd, model },
        )
        .await?;
        let response =
            tokio::time::timeout(std::time::Duration::from_secs(30), bridge.read_response())
                .await
                .map_err(|_| "Timeout listing Claude capabilities".to_string())?
                .map_err(|e| format!("Bridge read error: {e}"))?;
        let _ = bridge.child.start_kill();

        match response {
            BridgeResponse::Capabilities {
                commands,
                agents,
                mcp_servers,
            } => Ok((commands, agents, mcp_servers)),
            BridgeResponse::Error { error } => {
                Err(format!("Claude list_capabilities error: {error}"))
            }
            other => Err(format!(
                "Unexpected bridge response for list_capabilities: {:?}",
                other
            )),
        }
    }
}
