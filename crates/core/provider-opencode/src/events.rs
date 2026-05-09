//! SSE reader + per-session event routing.
//!
//! One opencode server streams events for every session over a
//! single `GET /event` SSE connection. The [`EventRouter`] owns that
//! connection and fans events out to the `TurnEventSink` registered
//! for each in-flight session. Sessions that aren't currently
//! awaiting a turn get their events dropped — the opencode server
//! persists them server-side, so anything we miss here is replayable
//! by asking for the session detail again.
//!
//! Event mapping is deliberately permissive. Opencode's SSE schema
//! has evolved across versions and our sink only cares about a
//! handful of conceptual milestones (text delta, tool start, tool
//! complete, permission request, turn end). We pattern-match on the
//! `type` field at runtime and forward the salient bits; unknown
//! event types are logged and ignored, which is the same
//! forward-compatibility stance the other provider adapters take.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Weak};
use std::time::Duration;

use futures_util::StreamExt;
use serde_json::Value;
use tokio::sync::{Mutex, oneshot};
use tokio::time::timeout;
use tracing::{debug, warn};
use zenui_provider_api::{
    PermissionDecision, ProviderTurnEvent, TurnEventSink, UserInputOption, UserInputQuestion,
};

use crate::http::{OpenCodeClient, PermissionReply};
use crate::server::OpenCodeServer;

/// Handle returned by [`EventRouter::subscribe`]. The adapter keeps
/// this alive for the length of a turn and either:
///   - awaits [`Subscription::wait_for_completion`] (happy path), or
///   - calls [`Subscription::cancel`] (prompt dispatch failed before
///     we ever needed the stream).
///
/// Dropping the subscription does *not* automatically unsubscribe —
/// the adapter owns the lifecycle so that a panic mid-turn still
/// leaves the router's registrations clean (we unsubscribe
/// explicitly in every code path, including the error branch).
pub struct Subscription {
    native_session_id: String,
    router: Arc<EventRouter>,
    completion: oneshot::Receiver<Result<String, String>>,
}

impl Subscription {
    /// Wait for a turn-completion event on the registered session,
    /// bounded by `max`. On timeout we synthesise a diagnostic so
    /// the caller can surface it as a turn-level error instead of
    /// hanging the UI.
    pub async fn wait_for_completion(self, max: Duration) -> Result<String, String> {
        let Subscription {
            native_session_id,
            router,
            completion,
        } = self;

        let outcome = match timeout(max, completion).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err("opencode SSE channel closed before the turn completed \
                 (server may have restarted)"
                .to_string()),
            Err(_) => Err(format!(
                "opencode turn exceeded {}s with no completion event",
                max.as_secs()
            )),
        };

        // Always unsubscribe; the router must not hold onto a stale
        // sink reference once the turn has resolved one way or the
        // other.
        router.unsubscribe(&native_session_id).await;
        outcome
    }

    /// Explicitly abandon the subscription without awaiting. Used on
    /// the error branch where the prompt POST itself failed before
    /// we ever expected an SSE completion.
    pub async fn cancel(self) {
        self.router.unsubscribe(&self.native_session_id).await;
    }
}

/// Per-session runtime state owned by the router.
struct SessionState {
    sink: TurnEventSink,
    /// Accumulator for assistant text across deltas. Returned in the
    /// `wait_for_completion` payload so the adapter's
    /// `ProviderTurnOutput.output` stays populated even if the
    /// runtime's reconciliation fallback (concat of
    /// `AssistantTextDelta`s) ever drifts.
    accumulated_output: String,
    /// Tool-call ids for which we've already emitted
    /// `ToolCallStarted`. Opencode fires `message.part.updated` many
    /// times for a single tool as it progresses (args arriving,
    /// status flipping pending → running); we only want to surface
    /// the first one as a "started" event to avoid duplicate cards
    /// in the UI. Entries are removed when the tool transitions to
    /// `completed` or `error`.
    open_tool_calls: HashSet<String>,
    /// Per-session working directory passed at `subscribe` time.
    /// Forwarded into `respond_to_permission` / `respond_to_question`
    /// so opencode's per-request directory resolver scopes the
    /// reply to the same project the prompt ran under. Without this,
    /// the reply hits `process.cwd()` of the opencode server — which
    /// happens to work today (the reply doesn't run tools), but
    /// matching the SDK's behaviour is the safe default.
    directory: String,
    /// Oneshot fired by the SSE reader on turn completion (success
    /// or error) — the subscriber awaits this inside
    /// `wait_for_completion`. Wrapped in `Option` so the reader can
    /// `take()` it exactly once; later events for the same session
    /// fall through to the logged-and-ignored path.
    completion: Option<oneshot::Sender<Result<String, String>>>,
}

pub struct EventRouter {
    /// Keyed by opencode's native session id — *not* flowstate's
    /// internal session id. The SSE events carry the opencode id so
    /// we index on that directly.
    ///
    /// Shared across per-flowstate-session `opencode serve` children
    /// because opencode mints globally-unique native ids (UUIDs), so
    /// a single HashMap can demux events from every running server
    /// without key collisions.
    sessions: Mutex<HashMap<String, SessionState>>,
    /// Working directories we've already spawned an SSE reader for,
    /// against the current opencode server. Opencode 1.14.41 scopes
    /// the `/event` stream by the request's `x-opencode-directory`
    /// header (or `?directory=` query) — without it, the SSE
    /// subscriber receives only events whose session directory
    /// matches `process.cwd()` of the opencode server. So a single
    /// SSE reader can't cover sessions across multiple project
    /// directories; we have to spawn one reader per unique cwd. This
    /// set records which cwds already have a reader so a second
    /// `subscribe(...)` for the same dir doesn't double-spawn.
    ///
    /// Reset by [`reset_directory_readers`] when the server respawns
    /// (idle-kill → fresh server → fresh readers required).
    directory_readers: Mutex<HashSet<String>>,
}

impl EventRouter {
    pub fn new() -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
            directory_readers: Mutex::new(HashSet::new()),
        }
    }

    /// Forget every directory-scoped SSE reader we've spawned. Called
    /// when the underlying `OpenCodeServer` respawns — the previous
    /// readers are about to exit because their `Weak<OpenCodeServer>`
    /// upgrade fails — so the next `subscribe(...)` call must mint
    /// fresh readers against the new server.
    pub async fn reset_directory_readers(&self) {
        self.directory_readers.lock().await.clear();
    }

    /// Register a sink for a given opencode session id and receive a
    /// [`Subscription`] that resolves when the turn finishes.
    ///
    /// `directory` is the per-session working directory the adapter
    /// resolved for the turn. Stashed inside `SessionState` so that
    /// the permission and question reply handlers can attach it to
    /// their HTTP requests (matching the SDK's behaviour of scoping
    /// every per-session call by directory).
    pub async fn subscribe(
        self: &Arc<Self>,
        native_session_id: String,
        sink: TurnEventSink,
        directory: String,
    ) -> Subscription {
        let (tx, rx) = oneshot::channel();
        let state = SessionState {
            sink,
            accumulated_output: String::new(),
            open_tool_calls: HashSet::new(),
            directory,
            completion: Some(tx),
        };
        self.sessions
            .lock()
            .await
            .insert(native_session_id.clone(), state);
        Subscription {
            native_session_id,
            router: self.clone(),
            completion: rx,
        }
    }

    /// Remove a session's routing state. Safe to call multiple times;
    /// absent keys are ignored.
    pub async fn unsubscribe(&self, native_session_id: &str) {
        let mut guard = self.sessions.lock().await;
        guard.remove(native_session_id);
    }

    /// Fail every in-flight subscriber with the supplied reason,
    /// clearing the session map. Called by the idle-kill path in
    /// the adapter right before it tears down the `OpenCodeServer`
    /// so subscribers surface a clean error instead of hanging on
    /// their completion oneshot forever.
    ///
    /// In practice `inflight == 0` (the idle-kill precondition)
    /// implies no `execute_turn` is mid-flight, so this is mostly
    /// a defensive sweep — it primarily catches stale entries from
    /// e.g. a `cancel()` path that didn't fully drain, or a
    /// permission-ask task that spawned during teardown. The cost
    /// of the sweep is O(entries) and bounded by the number of
    /// open flowstate sessions.
    pub async fn fail_all_in_flight(&self, reason: &str) {
        let mut guard = self.sessions.lock().await;
        if guard.is_empty() {
            return;
        }
        // Drain so the map is empty regardless of whether individual
        // oneshots had already been `take()`n by `session.idle` /
        // `session.error` paths earlier.
        let drained: Vec<_> = guard.drain().collect();
        drop(guard);
        for (session_id, mut state) in drained {
            if let Some(sender) = state.completion.take() {
                // `let _ =` — receiver may already be gone (caller
                // timed out, dropped the subscription). Not an error.
                let _ = sender.send(Err(reason.to_string()));
            }
            // Log at debug; a watcher-driven mass-fail is expected
            // during idle-kill and not a user-facing incident.
            debug!(
                %session_id,
                reason,
                "opencode: failing in-flight session due to server shutdown"
            );
        }
    }

    /// Spawn an SSE reader for `directory` against the current
    /// opencode server, if one isn't already running for that dir.
    ///
    /// Why per-directory: opencode 1.14.41 scopes its `/event` stream
    /// by the request's `x-opencode-directory` header. A reader
    /// without that header (or with the wrong value) only receives
    /// events from sessions whose directory matches `process.cwd()`
    /// of the opencode server — so when our adapter hosts sessions
    /// across multiple project directories, one shared reader misses
    /// every event from every project except the one matching the
    /// server's CWD. The fix is one reader per directory; opencode
    /// supports many concurrent SSE clients on a single server.
    ///
    /// Called from `subscribe(...)` so the reader is guaranteed to be
    /// up by the time the adapter fires a prompt for that session.
    /// Idempotent per directory — duplicate calls for the same dir
    /// are no-ops (the existing reader keeps running).
    pub async fn ensure_directory_reader(
        self: &Arc<Self>,
        server: Arc<OpenCodeServer>,
        client: Arc<OpenCodeClient>,
        directory: String,
    ) {
        {
            let mut guard = self.directory_readers.lock().await;
            if !guard.insert(directory.clone()) {
                // Already running for this dir.
                return;
            }
        }
        let router = self.clone();
        let weak_server = Arc::downgrade(&server);
        // Drop our Arc before spawning — the reader holds a `Weak` so
        // its existence doesn't keep the server alive past
        // `end_session`.
        drop(server);
        tokio::spawn(async move {
            read_forever(router, client, weak_server, directory).await;
        });
    }
}

/// SSE reader bound to one `opencode serve` child AND scoped to one
/// project directory. Reconnects on transient failures with a short
/// backoff so a momentary byte-stream blip doesn't orphan an
/// in-flight turn. Exits cleanly the moment the
/// `Weak<OpenCodeServer>` fails to upgrade — i.e. when
/// `shutdown_session_server` drops the last strong ref — so we don't
/// leak a retry loop hammering a dead port after `end_session`.
///
/// The `directory` parameter is sent on every connect as the
/// `x-opencode-directory` header. Without it, opencode falls back to
/// `process.cwd()` and only delivers events from sessions whose
/// directory matches the server's own CWD — see
/// [`crate::http::OPENCODE_DIRECTORY_HEADER`] (live-probed against
/// 1.14.41) for the resolver behaviour.
async fn read_forever(
    router: Arc<EventRouter>,
    client: Arc<OpenCodeClient>,
    server: Weak<OpenCodeServer>,
    directory: String,
) {
    let (user, pass) = client.credentials();
    let user = user.to_string();
    let pass = pass.to_string();
    let url = format!("{}/event", client.base_url());
    let directory_header_value = crate::http::urlencode_path_for_header(&directory);

    // The SSE client wants no read timeout — events arrive at their
    // own pace. We still time out the initial connect so a wedged
    // server doesn't hang the reader forever.
    let http = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .build()
        .expect("reqwest SSE client build should not fail");

    loop {
        // Per-session servers: the moment the caller drops the last
        // strong ref (session delete, adapter tear-down), exit. Kept
        // above the actual connect so a server that dies mid-retry
        // loop doesn't generate a storm of connect-refused logs.
        if server.upgrade().is_none() {
            debug!(%url, %directory, "opencode server dropped; SSE reader exiting");
            return;
        }

        let response = match http
            .get(&url)
            .basic_auth(&user, Some(&pass))
            .header("accept", "text/event-stream")
            .header(crate::http::OPENCODE_DIRECTORY_HEADER, &directory_header_value)
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => resp,
            Ok(resp) => {
                warn!(status = %resp.status(), "opencode /event returned non-success; retrying");
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
            Err(err) => {
                warn!(%err, "opencode /event connect failed; retrying");
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
        };

        let mut stream = response.bytes_stream();
        let mut buffer = String::new();
        while let Some(chunk) = stream.next().await {
            let bytes = match chunk {
                Ok(b) => b,
                Err(err) => {
                    warn!(%err, "opencode SSE byte stream error; reconnecting");
                    break;
                }
            };
            buffer.push_str(&String::from_utf8_lossy(&bytes));

            // SSE frames are separated by blank lines. Drain every
            // complete frame out of the buffer; the trailing
            // (incomplete) frame stays for the next chunk.
            while let Some(idx) = buffer.find("\n\n") {
                let frame = buffer[..idx].to_string();
                buffer.drain(..=idx + 1);
                if let Some(data) = extract_sse_data(&frame) {
                    if let Err(err) = dispatch_frame(&router, &client, &data).await {
                        debug!(%err, "opencode SSE frame dispatch problem");
                    }
                }
            }
        }
    }
}

/// Pull the `data:` payload out of an SSE frame. Ignores comments,
/// `event:` lines (opencode multiplexes event types inside the JSON
/// payload itself), and empty frames.
fn extract_sse_data(frame: &str) -> Option<String> {
    let mut data_lines = Vec::new();
    for line in frame.lines() {
        if let Some(rest) = line.strip_prefix("data:") {
            data_lines.push(rest.trim_start().to_string());
        }
    }
    if data_lines.is_empty() {
        None
    } else {
        Some(data_lines.join("\n"))
    }
}

/// Parse an SSE data payload as JSON and forward the conceptual
/// event to the matching session's sink. Unknown shapes are logged
/// at `debug!` — opencode adds events over time and the adapter
/// should degrade gracefully instead of error-looping on a new
/// variant.
async fn dispatch_frame(
    router: &Arc<EventRouter>,
    client: &Arc<OpenCodeClient>,
    data: &str,
) -> Result<(), String> {
    let payload: Value =
        serde_json::from_str(data).map_err(|e| format!("invalid SSE JSON: {e}: {data}"))?;

    let event_type = payload
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let session_id = find_session_id(&payload);

    // Events that aren't session-scoped (startup banners, app-wide
    // state) are informational-only from the adapter's perspective.
    // Drop them — they'll come through the daemon log via stderr
    // forwarding if the operator needs them.
    let Some(session_id) = session_id else {
        debug!(
            event_type,
            "opencode SSE event without session id; skipping"
        );
        return Ok(());
    };

    // Permission + question handling are special cases: they need
    // the sink outside the router lock (so we can block on the user
    // for an arbitrarily long time) and they need the HTTP client to
    // post the reply. Branch early, clone what we need, drop the lock.
    if event_type == "permission.asked" || event_type == "question.asked" {
        let props = payload
            .get("properties")
            .cloned()
            .unwrap_or(Value::Object(Default::default()));
        // Snapshot both the sink and the per-session directory in a
        // single lock acquire so the spawned reply task carries the
        // exact directory that was associated with the running turn.
        let session_snapshot = {
            let sessions = router.sessions.lock().await;
            sessions
                .get(&session_id)
                .map(|s| (s.sink.clone(), s.directory.clone()))
        };
        let Some((sink, directory)) = session_snapshot else {
            debug!(
                %session_id,
                event_type,
                "prompt event for unsubscribed session; dropping \
                 (opencode will time out on its own)"
            );
            return Ok(());
        };
        if event_type == "permission.asked" {
            tokio::spawn(handle_permission_asked(
                client.clone(),
                sink,
                session_id,
                props,
                directory,
            ));
        } else {
            tokio::spawn(handle_question_asked(
                client.clone(),
                sink,
                props,
                directory,
            ));
        }
        return Ok(());
    }

    // `*.replied` events are purely informational — opencode echoes
    // the reply back on the event stream after our POST completes.
    // Nothing for us to do.
    if event_type == "permission.replied" || event_type == "question.replied" {
        return Ok(());
    }

    let mut sessions = router.sessions.lock().await;
    let Some(state) = sessions.get_mut(&session_id) else {
        debug!(%session_id, event_type, "opencode SSE event for unsubscribed session; dropping");
        return Ok(());
    };

    // Opencode's event types are dotted and nested under a
    // `properties` object. The schema reference used while writing
    // this branch:
    //   - message.part.delta   → { properties: { partID, sessionID, delta } }
    //                            `delta` is an incremental string,
    //                            *not* a snapshot of the full reply.
    //   - message.part.updated → { properties: { part: { id, sessionID,
    //                                                    type, text?, tool?, state? } } }
    //                            fires for both text and tool parts;
    //                            for tools it's the primary lifecycle
    //                            signal (state.status ∈ pending | running
    //                            | completed | error).
    //   - message.updated      → { properties: { info: { id, sessionID, role } } }
    //                            role transitions; no text payload.
    //   - session.status       → { properties: { sessionID, status: { type, message? } } }
    //                            type ∈ busy | idle | retry. idle =
    //                            turn done.
    //   - session.error        → { properties: { sessionID, error: {...} } }
    let props = payload
        .get("properties")
        .cloned()
        .unwrap_or(Value::Object(Default::default()));

    match event_type.as_str() {
        "message.part.delta" => {
            let delta = props
                .get("delta")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if !delta.is_empty() {
                state.accumulated_output.push_str(delta);
                state
                    .sink
                    .send(ProviderTurnEvent::AssistantTextDelta {
                        delta: delta.to_string(),
                    })
                    .await;
            }
        }

        "message.part.updated" => {
            let part = match props.get("part") {
                Some(p) => p,
                None => return Ok(()),
            };
            let part_type = part.get("type").and_then(Value::as_str).unwrap_or_default();

            match part_type {
                // Text parts can arrive via `updated` too (final
                // snapshot on the last assistant message). We've
                // already been streaming via `message.part.delta`, so
                // emitting again here would double-print. Skip.
                "text" | "reasoning" => {}

                "tool" => {
                    let call_id = part
                        .get("callID")
                        .or_else(|| part.get("id"))
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let name = part
                        .get("tool")
                        .and_then(Value::as_str)
                        .unwrap_or("tool")
                        .to_string();
                    let tool_state = part.get("state").cloned().unwrap_or(Value::Null);
                    let status = tool_state
                        .get("status")
                        .and_then(Value::as_str)
                        .unwrap_or("pending");
                    let input = tool_state
                        .get("input")
                        .cloned()
                        .unwrap_or(Value::Object(Default::default()));
                    let input_is_populated = match &input {
                        Value::Object(map) => !map.is_empty(),
                        Value::Null => false,
                        // Anything else (string, number, array) — treat
                        // as populated. Opencode normalises tool input
                        // to an object in practice, but we don't want
                        // to drop a non-object payload on the floor.
                        _ => true,
                    };

                    match status {
                        "pending" | "running" => {
                            // Opencode fires `message.part.updated`
                            // multiple times for one tool call as it
                            // accretes:
                            //
                            //   1. status=pending, input={}
                            //   2. status=running, input={...args...}
                            //   3. status=running, input={...args...}  (heartbeats)
                            //   N. status=completed, input={...args...}, output=…
                            //
                            // Verified live against opencode 1.14.41 — the
                            // initial `pending` event always carries an
                            // empty input, because the model hasn't
                            // finished generating the tool-call JSON
                            // yet. Emitting `ToolCallStarted` on that
                            // event then dedup-dropping every later
                            // update means the UI sees `args: {}` and
                            // renders "no args" forever.
                            //
                            // Fix: defer the started emission until we
                            // see a status=running event *or* an event
                            // with non-empty input. Either signal
                            // means the args have landed. The first
                            // such event wins; later updates are
                            // dropped via `open_tool_calls` dedup.
                            //
                            // Tools with genuinely empty args (none
                            // observed in the wild on 1.14.41 but
                            // possible on custom MCP tools) still
                            // surface — `running` is enough on its
                            // own. And as a final defence, the
                            // `completed`/`error` arms below
                            // re-emit `ToolCallStarted` if we never
                            // saw a started-eligible event so the
                            // card is never silently absent.
                            let ready = status == "running" || input_is_populated;
                            if ready
                                && !call_id.is_empty()
                                && state.open_tool_calls.insert(call_id.clone())
                            {
                                state
                                    .sink
                                    .send(ProviderTurnEvent::ToolCallStarted {
                                        call_id,
                                        name,
                                        args: input,
                                        parent_call_id: None,
                                    })
                                    .await;
                            }
                        }
                        "completed" => {
                            // Defensive started-emission: if every
                            // pending/running update arrived with empty
                            // args (or the started arm dedup-dropped
                            // them), emit `ToolCallStarted` now using
                            // the args from this completion event.
                            // Opencode's `completed` event still
                            // carries the final `input`, so this is
                            // the last chance to put real args on the
                            // tool card before the completion lands.
                            if !call_id.is_empty()
                                && state.open_tool_calls.insert(call_id.clone())
                            {
                                state
                                    .sink
                                    .send(ProviderTurnEvent::ToolCallStarted {
                                        call_id: call_id.clone(),
                                        name: name.clone(),
                                        args: input,
                                        parent_call_id: None,
                                    })
                                    .await;
                            }
                            let output = tool_state
                                .get("output")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string();
                            state.open_tool_calls.remove(&call_id);
                            state
                                .sink
                                .send(ProviderTurnEvent::ToolCallCompleted {
                                    call_id,
                                    output,
                                    error: None,
                                })
                                .await;
                        }
                        "error" => {
                            // Same defensive started-emission as the
                            // completed arm — see comment there.
                            if !call_id.is_empty()
                                && state.open_tool_calls.insert(call_id.clone())
                            {
                                state
                                    .sink
                                    .send(ProviderTurnEvent::ToolCallStarted {
                                        call_id: call_id.clone(),
                                        name: name.clone(),
                                        args: input,
                                        parent_call_id: None,
                                    })
                                    .await;
                            }
                            let error = tool_state
                                .get("error")
                                .and_then(Value::as_str)
                                .unwrap_or("tool failed")
                                .to_string();
                            state.open_tool_calls.remove(&call_id);
                            state
                                .sink
                                .send(ProviderTurnEvent::ToolCallCompleted {
                                    call_id,
                                    output: String::new(),
                                    error: Some(error),
                                })
                                .await;
                        }
                        _ => {}
                    }
                }

                _ => {
                    debug!(part_type, "opencode tool/part type ignored");
                }
            }
        }

        // `session.idle` is the real end-of-turn signal. Confirmed
        // via live probe against opencode 1.4.3: a fresh standalone
        // event (no nested `status` object), fired exactly once when
        // the server finishes streaming the assistant's reply.
        "session.idle" => {
            if let Some(sender) = state.completion.take() {
                let final_text = std::mem::take(&mut state.accumulated_output);
                let _ = sender.send(Ok(final_text));
            }
        }

        // Coarse lifecycle signal. Live probe showed only `busy` and
        // `retry` here — `idle` never appears inside `session.status`,
        // it's the dedicated event above. `busy` arrives on every
        // turn start and we don't need to surface it (the runtime
        // already shows a thinking indicator). `retry` surfaces as
        // an info event so the user sees why a turn stalled.
        "session.status" => {
            let status_type = props
                .get("status")
                .and_then(|s| s.get("type"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            if status_type == "retry" {
                let message = props
                    .get("status")
                    .and_then(|s| s.get("message"))
                    .and_then(Value::as_str)
                    .unwrap_or("opencode is retrying")
                    .to_string();
                state.sink.send(ProviderTurnEvent::Info { message }).await;
            }
        }

        // Server-side turn failure. Route to the completion oneshot
        // as `Err` so the adapter reports the turn as failed instead
        // of timing out.
        "session.error" => {
            let message = props
                .get("error")
                .and_then(|e| {
                    e.get("message")
                        .and_then(Value::as_str)
                        .or_else(|| e.get("name").and_then(Value::as_str))
                })
                .or_else(|| props.get("error").and_then(Value::as_str))
                .unwrap_or("opencode emitted an unknown error")
                .to_string();
            if let Some(sender) = state.completion.take() {
                let _ = sender.send(Err(format!("opencode: {message}")));
            }
        }

        // Role transition, message deletion, session metadata /
        // diff updates, question-flow events, handshake banner.
        // None of these need sink surfacing today; logged for
        // discoverability. Event list verified via live probe
        // against opencode 1.4.3 — update this arm if a new event
        // type warrants real handling rather than a silent
        // fallthrough.
        "message.updated" | "message.removed" | "session.updated" | "session.diff"
        | "server.connected" | "server.heartbeat" | "question.rejected" | "session.started"
        | "session.exited" => {
            debug!(event_type, "opencode lifecycle event acknowledged");
        }

        other => {
            debug!(event_type = other, "opencode SSE event ignored");
        }
    }

    Ok(())
}

/// Prompt the user via the session's sink for a permission decision,
/// then POST the translated reply to opencode. Runs on its own task
/// because the user may take seconds or minutes to answer and we
/// must not block the SSE reader while that happens.
///
/// Opencode's `permission.asked.properties` carries:
/// - `id` — opaque permission id; used verbatim on the reply URL.
/// - `permission` — high-level category (`"bash"`, `"edit"`, `"read"`).
/// - `patterns` — array of specific patterns (e.g. the shell command,
///   a glob); surfaced to the user as supporting detail.
/// - `metadata` — arbitrary tool-specific args; forwarded as the
///   `input` blob on the `PermissionRequest` event so the UI can
///   render a rich preview.
///
/// The shape is permissive to defend against opencode releases that
/// add or rename fields; missing fields degrade to sensible defaults
/// rather than failing the permission round-trip outright.
async fn handle_permission_asked(
    client: Arc<OpenCodeClient>,
    sink: TurnEventSink,
    session_id: String,
    props: Value,
    directory: String,
) {
    let permission_id = props
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if permission_id.is_empty() {
        warn!(
            %session_id,
            "opencode permission.asked event missing `id`; cannot reply — auto-rejecting"
        );
        return;
    }

    // Tool-name the UI will show. Opencode's `permission` field is a
    // category like `"bash"` or `"edit"`; when `patterns` exists we
    // concatenate the first pattern onto the tool name so the user
    // sees e.g. `"bash: rm -rf …"` in the prompt instead of a bare
    // `"bash"`.
    let category = props
        .get("permission")
        .and_then(Value::as_str)
        .unwrap_or("tool")
        .to_string();
    let first_pattern = props
        .get("patterns")
        .and_then(Value::as_array)
        .and_then(|arr| arr.first())
        .and_then(Value::as_str)
        .map(str::to_string);
    let tool_name = match first_pattern {
        Some(p) if !p.is_empty() => format!("{category}: {p}"),
        _ => category,
    };

    // Input blob surfaced to the UI. Prefer opencode's `metadata`
    // (tool args) when present; fall back to the whole properties
    // object so the UI always has *something* to render.
    let input = props
        .get("metadata")
        .cloned()
        .unwrap_or_else(|| props.clone());

    let (decision, _mode_override, _denial_reason) = sink
        .request_permission(tool_name, input, PermissionDecision::Allow)
        .await;

    let reply = PermissionReply::from_decision(decision);
    if let Err(err) = client
        .respond_to_permission(&session_id, &permission_id, reply, Some(&directory))
        .await
    {
        warn!(
            %session_id,
            %permission_id,
            %err,
            "failed to deliver permission reply to opencode"
        );
    }
}

/// Handle a `question.asked` event: translate opencode's question
/// payload into flowstate `UserInputQuestion`s, await the user via
/// the sink, then POST the answers back to opencode.
///
/// Event shape (verified via probe3.mjs against opencode 1.4.3):
///
/// ```text
/// {
///   "type": "question.asked",
///   "properties": {
///     "id": "que_...",            ← requestID; used on the reply URL + body
///     "sessionID": "ses_...",
///     "questions": [
///       { "question": "...", "header": "...",
///         "options": [ { "label": "Yes", "description": "..." }, ... ] }
///     ],
///     "tool": { "messageID": "msg_...", "callID": "functions.question:N" }
///   }
/// }
/// ```
///
/// Reply endpoint: `POST /question/{id}/reply`
/// Body: `{ "requestID": id, "answers": [ ["Yes"], ... ] }` — one
/// inner array per question. Each inner array is the selected option
/// labels (multi-select aware).
async fn handle_question_asked(
    client: Arc<OpenCodeClient>,
    sink: TurnEventSink,
    props: Value,
    directory: String,
) {
    let request_id = props
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if request_id.is_empty() {
        warn!("opencode question.asked missing `id`; cannot reply");
        return;
    }

    let raw_questions = match props.get("questions").and_then(Value::as_array) {
        Some(list) if !list.is_empty() => list.clone(),
        _ => {
            warn!(%request_id, "opencode question.asked carried no questions");
            return;
        }
    };

    // Translate each opencode question into a flowstate
    // `UserInputQuestion`. We assign each question a stable id of
    // `idx-{n}` so the response ordering is unambiguous when the sink
    // comes back with answers.
    let mut questions = Vec::with_capacity(raw_questions.len());
    for (idx, q) in raw_questions.iter().enumerate() {
        let text = q
            .get("question")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let header = q.get("header").and_then(Value::as_str).map(str::to_string);
        let opts = q
            .get("options")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let options: Vec<UserInputOption> = opts
            .into_iter()
            .filter_map(|o| {
                let label = o.get("label").and_then(Value::as_str)?.to_string();
                let description = o
                    .get("description")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                // Use the label as the option id — opencode's reply
                // body expects the label strings verbatim, so
                // round-tripping through a different id would add a
                // pointless lookup.
                Some(UserInputOption {
                    id: label.clone(),
                    label,
                    description,
                })
            })
            .collect();
        // Multi-select inferred from opencode's schema absence of a
        // single-answer hint. Treat as single-select (radio) which
        // matches the yes/no shape seen in the wild; we can refine
        // later if opencode grows a `multiSelect` flag.
        questions.push(UserInputQuestion {
            id: format!("idx-{idx}"),
            text,
            header,
            options,
            multi_select: false,
            allow_freeform: false,
            is_secret: false,
        });
    }

    let answers = match sink.ask_user(questions.clone()).await {
        Some(a) => a,
        None => {
            // User dismissed / turn was interrupted. Send an empty
            // reply so opencode stops waiting — a missing reply would
            // hang the agent forever.
            warn!(%request_id, "user dismissed question; replying empty");
            let empty: Vec<Vec<String>> = questions.iter().map(|_| Vec::new()).collect();
            if let Err(err) = client
                .respond_to_question(&request_id, empty, Some(&directory))
                .await
            {
                warn!(%request_id, %err, "dismissal reply to opencode failed");
            }
            return;
        }
    };

    // Project answers onto the `Vec<Vec<String>>` shape opencode
    // expects. For each original question (by index), collect the
    // matching answer's `option_ids` (which we encoded as the label
    // strings) — or the freeform `answer` text if no options were
    // picked.
    let mut reply: Vec<Vec<String>> = Vec::with_capacity(questions.len());
    for q in &questions {
        let matched = answers.iter().find(|a| a.question_id == q.id);
        let values = match matched {
            Some(a) if !a.option_ids.is_empty() => a.option_ids.clone(),
            Some(a) if !a.answer.trim().is_empty() => vec![a.answer.clone()],
            _ => Vec::new(),
        };
        reply.push(values);
    }

    if let Err(err) = client
        .respond_to_question(&request_id, reply, Some(&directory))
        .await
    {
        warn!(%request_id, %err, "failed to deliver question reply to opencode");
    }
}

/// Opencode's SSE payloads tuck the session id in a few different
/// places depending on event type. Concrete paths observed in the
/// wild:
///   - `properties.sessionID`           — session.status, session.error,
///                                         permission.asked, session.*
///   - `properties.info.sessionID`      — message.updated
///   - `properties.part.sessionID`      — message.part.updated
///   - Top-level `sessionID`            — occasional legacy shape
///
/// We walk them in order and return on the first hit.
fn find_session_id(payload: &Value) -> Option<String> {
    const KEYS: &[&str] = &["sessionID", "sessionId", "session_id"];
    let candidates = [
        payload.pointer("/properties"),
        payload.pointer("/properties/info"),
        payload.pointer("/properties/part"),
        Some(payload),
    ];
    for container in candidates.into_iter().flatten() {
        for key in KEYS {
            if let Some(id) = container.get(*key).and_then(Value::as_str) {
                if !id.is_empty() {
                    return Some(id.to_string());
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    //! In-process tests for the SSE dispatcher. No network, no
    //! spawned subprocess — all fixture JSON is copy-pasted from the
    //! live probe at `/tmp/opencode-probe/probe.mjs` so drift between
    //! our adapter and the real opencode wire format fails the suite
    //! at `cargo test -p zenui-provider-opencode` speed.
    //!
    //! Permission / question handlers spawn tokio tasks that POST to
    //! `client.base_url`. The tests point the client at
    //! `http://127.0.0.1:1` so those POSTs fail fast with connection
    //! refused; we only assert on the sink emission, which happens
    //! before the POST.
    //!
    //! The canonical reference for every event shape lives in
    //! `crates/core/provider-opencode/PROTOCOL.md` — update the tests
    //! and the doc together when opencode ships a new event.

    use super::*;
    use serde_json::json;
    use std::time::Duration;
    use tokio::sync::mpsc;
    use zenui_provider_api::TurnEventSink;

    const SESSION_ID: &str = "ses_test";

    fn dummy_client() -> Arc<OpenCodeClient> {
        // Point at a definitely-unreachable port so any spawned POST
        // fails fast. Tests never assert on the POST's outcome.
        Arc::new(OpenCodeClient::new(
            "http://127.0.0.1:1".to_string(),
            "test-password".to_string(),
        ))
    }

    /// Build a router with one subscribed session and return every
    /// event the sink receives after dispatching `frames`.
    async fn drive(frames: &[Value]) -> Vec<ProviderTurnEvent> {
        let router = Arc::new(EventRouter::new());
        let client = dummy_client();
        let (tx, mut rx) = mpsc::channel(256);
        let sink = TurnEventSink::new(tx);
        let _sub = router
            .subscribe(SESSION_ID.to_string(), sink, "/test-cwd".to_string())
            .await;

        for payload in frames {
            let raw = serde_json::to_string(payload).unwrap();
            dispatch_frame(&router, &client, &raw).await.unwrap();
        }

        // Give any tokio tasks spawned by the dispatcher (permission,
        // question) a chance to push their events to the sink before
        // we drain.
        tokio::task::yield_now().await;
        tokio::time::sleep(Duration::from_millis(25)).await;

        let mut events = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            events.push(ev);
        }
        events
    }

    // ── pure helpers ────────────────────────────────────────────

    #[test]
    fn find_session_id_reads_properties_sessionid() {
        let ev = json!({ "properties": { "sessionID": "ses_abc" } });
        assert_eq!(find_session_id(&ev), Some("ses_abc".to_string()));
    }

    #[test]
    fn find_session_id_reads_message_updated_info_path() {
        let ev = json!({
            "type": "message.updated",
            "properties": { "info": { "sessionID": "ses_info" } }
        });
        assert_eq!(find_session_id(&ev), Some("ses_info".to_string()));
    }

    #[test]
    fn find_session_id_reads_message_part_updated_part_path() {
        let ev = json!({
            "type": "message.part.updated",
            "properties": { "part": { "sessionID": "ses_part" } }
        });
        assert_eq!(find_session_id(&ev), Some("ses_part".to_string()));
    }

    #[test]
    fn find_session_id_rejects_empty_and_missing() {
        assert_eq!(find_session_id(&json!({})), None);
        assert_eq!(
            find_session_id(&json!({ "properties": { "sessionID": "" } })),
            None
        );
    }

    // ── dispatcher: happy-path streaming ────────────────────────

    #[tokio::test]
    async fn message_part_delta_emits_assistant_text_delta() {
        // Fixture copied from live probe: opencode streams real text
        // deltas via `message.part.delta.properties.delta`.
        let frame = json!({
            "type": "message.part.delta",
            "properties": {
                "sessionID": SESSION_ID,
                "messageID": "msg_x",
                "partID": "prt_x",
                "field": "text",
                "delta": " Hello"
            }
        });
        let events = drive(&[frame]).await;
        assert!(
            matches!(
                events.as_slice(),
                [ProviderTurnEvent::AssistantTextDelta { delta }] if delta == " Hello"
            ),
            "got {events:?}"
        );
    }

    #[tokio::test]
    async fn multiple_deltas_stream_in_order() {
        let frames = vec![
            json!({
                "type": "message.part.delta",
                "properties": {
                    "sessionID": SESSION_ID, "partID": "p", "delta": "Hel"
                }
            }),
            json!({
                "type": "message.part.delta",
                "properties": {
                    "sessionID": SESSION_ID, "partID": "p", "delta": "lo!"
                }
            }),
        ];
        let events = drive(&frames).await;
        let deltas: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                ProviderTurnEvent::AssistantTextDelta { delta } => Some(delta.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(deltas, vec!["Hel", "lo!"]);
    }

    #[tokio::test]
    async fn empty_delta_is_dropped() {
        let frame = json!({
            "type": "message.part.delta",
            "properties": {
                "sessionID": SESSION_ID, "partID": "p", "delta": ""
            }
        });
        let events = drive(&[frame]).await;
        assert!(events.is_empty(), "empty deltas should not reach the sink");
    }

    // ── dispatcher: tool lifecycle ──────────────────────────────

    #[tokio::test]
    async fn tool_part_pending_emits_tool_call_started_once() {
        // Opencode fires `message.part.updated` multiple times as a
        // tool call accretes (pending → running → completed). The
        // dispatcher must emit `ToolCallStarted` exactly once.
        let pending = json!({
            "type": "message.part.updated",
            "properties": {
                "sessionID": SESSION_ID,
                "part": {
                    "type": "tool",
                    "callID": "call_1",
                    "tool": "bash",
                    "sessionID": SESSION_ID,
                    "state": { "status": "pending", "input": { "cmd": "ls" } }
                }
            }
        });
        let running = json!({
            "type": "message.part.updated",
            "properties": {
                "sessionID": SESSION_ID,
                "part": {
                    "type": "tool",
                    "callID": "call_1",
                    "tool": "bash",
                    "sessionID": SESSION_ID,
                    "state": { "status": "running", "input": { "cmd": "ls" } }
                }
            }
        });
        let events = drive(&[pending, running]).await;
        let starts = events
            .iter()
            .filter(|e| matches!(e, ProviderTurnEvent::ToolCallStarted { .. }))
            .count();
        assert_eq!(starts, 1, "expected one ToolCallStarted, got {starts}");
    }

    #[tokio::test]
    async fn tool_args_arrive_on_running_event_after_empty_pending() {
        // Live-probe shape (opencode 1.14.41): the very first `pending`
        // event for a tool call carries `input: {}` because the model
        // hasn't finished generating the tool-call JSON yet. Args first
        // appear on the subsequent `running` event. The dispatcher
        // must therefore wait for the running update before emitting
        // `ToolCallStarted`, otherwise the UI shows "no args" forever.
        let pending_empty = json!({
            "type": "message.part.updated",
            "properties": {
                "sessionID": SESSION_ID,
                "part": {
                    "type": "tool",
                    "callID": "functions.bash:0",
                    "tool": "bash",
                    "sessionID": SESSION_ID,
                    "state": { "status": "pending", "input": {} }
                }
            }
        });
        let running_with_args = json!({
            "type": "message.part.updated",
            "properties": {
                "sessionID": SESSION_ID,
                "part": {
                    "type": "tool",
                    "callID": "functions.bash:0",
                    "tool": "bash",
                    "sessionID": SESSION_ID,
                    "state": {
                        "status": "running",
                        "input": { "command": "ls /tmp", "description": "list /tmp" }
                    }
                }
            }
        });
        let events = drive(&[pending_empty, running_with_args]).await;
        let started = events.iter().find_map(|e| match e {
            ProviderTurnEvent::ToolCallStarted { args, name, .. } => Some((args.clone(), name.clone())),
            _ => None,
        });
        let (args, name) = started.expect("expected exactly one ToolCallStarted");
        assert_eq!(name, "bash");
        assert_eq!(args["command"], "ls /tmp");
        assert_eq!(args["description"], "list /tmp");
        let started_count = events
            .iter()
            .filter(|e| matches!(e, ProviderTurnEvent::ToolCallStarted { .. }))
            .count();
        assert_eq!(started_count, 1, "should emit exactly once, got {started_count}");
    }

    #[tokio::test]
    async fn tool_args_emit_immediately_on_pending_with_populated_input() {
        // Custom MCP tools sometimes ship the args on the very first
        // pending event (no separate running step). The dispatcher
        // should still emit on that first event — the gating rule is
        // "ready = status==running OR input populated", so a
        // populated-input pending qualifies on its own.
        let pending_with_args = json!({
            "type": "message.part.updated",
            "properties": {
                "sessionID": SESSION_ID,
                "part": {
                    "type": "tool",
                    "callID": "call_1",
                    "tool": "glob",
                    "sessionID": SESSION_ID,
                    "state": { "status": "pending", "input": { "pattern": "**/*.rs" } }
                }
            }
        });
        let events = drive(&[pending_with_args]).await;
        let started = events.iter().find_map(|e| match e {
            ProviderTurnEvent::ToolCallStarted { args, .. } => Some(args.clone()),
            _ => None,
        });
        let args = started.expect("expected ToolCallStarted on populated pending");
        assert_eq!(args["pattern"], "**/*.rs");
    }

    #[tokio::test]
    async fn tool_completion_synthesises_started_when_pending_was_skipped() {
        // Pathological / fast-path case: only the `completed` event
        // ever reaches us (e.g. we subscribed mid-turn and missed the
        // earlier updates, or opencode coalesces). The dispatcher must
        // emit `ToolCallStarted` synthesised from the completion's
        // `input` so the UI never sees a `ToolCallCompleted` for a
        // tool that was never started.
        let completed = json!({
            "type": "message.part.updated",
            "properties": {
                "sessionID": SESSION_ID,
                "part": {
                    "type": "tool",
                    "callID": "call_late",
                    "tool": "bash",
                    "sessionID": SESSION_ID,
                    "state": {
                        "status": "completed",
                        "input": { "command": "echo hi" },
                        "output": "hi"
                    }
                }
            }
        });
        let events = drive(&[completed]).await;
        let started = events.iter().find_map(|e| match e {
            ProviderTurnEvent::ToolCallStarted { args, call_id, .. } if call_id == "call_late" =>
                Some(args.clone()),
            _ => None,
        });
        let args = started.expect("expected synthesised ToolCallStarted on bare completion");
        assert_eq!(args["command"], "echo hi");
        let saw_completion = events
            .iter()
            .any(|e| matches!(e, ProviderTurnEvent::ToolCallCompleted { call_id, .. } if call_id == "call_late"));
        assert!(saw_completion, "completion event should still be emitted");
    }

    #[tokio::test]
    async fn tool_pending_with_empty_args_alone_emits_nothing() {
        // The first pending event with empty args carries no useful
        // information (status nor args). On its own it must not emit
        // `ToolCallStarted` — that would lock in `args: {}` and the
        // dedup would suppress the real running event.
        let pending_empty = json!({
            "type": "message.part.updated",
            "properties": {
                "sessionID": SESSION_ID,
                "part": {
                    "type": "tool",
                    "callID": "call_1",
                    "tool": "bash",
                    "sessionID": SESSION_ID,
                    "state": { "status": "pending", "input": {} }
                }
            }
        });
        let events = drive(&[pending_empty]).await;
        assert!(
            events.is_empty(),
            "empty-pending should not emit any sink events; got {events:?}"
        );
    }

    #[tokio::test]
    async fn tool_part_completed_emits_completion() {
        // Start then complete in one session.
        let start = json!({
            "type": "message.part.updated",
            "properties": {
                "sessionID": SESSION_ID,
                "part": {
                    "type": "tool", "callID": "c", "tool": "bash",
                    "sessionID": SESSION_ID,
                    "state": { "status": "running", "input": {} }
                }
            }
        });
        let done = json!({
            "type": "message.part.updated",
            "properties": {
                "sessionID": SESSION_ID,
                "part": {
                    "type": "tool", "callID": "c", "tool": "bash",
                    "sessionID": SESSION_ID,
                    "state": { "status": "completed", "output": "ok" }
                }
            }
        });
        let events = drive(&[start, done]).await;
        let completed = events.iter().any(|e| {
            matches!(e,
                ProviderTurnEvent::ToolCallCompleted { output, error: None, .. }
                if output == "ok")
        });
        assert!(
            completed,
            "expected ToolCallCompleted with output='ok'; got {events:?}"
        );
    }

    #[tokio::test]
    async fn tool_error_state_emits_failed_completion() {
        let err = json!({
            "type": "message.part.updated",
            "properties": {
                "sessionID": SESSION_ID,
                "part": {
                    "type": "tool", "callID": "c", "tool": "bash",
                    "sessionID": SESSION_ID,
                    "state": { "status": "error", "error": "boom" }
                }
            }
        });
        let events = drive(&[err]).await;
        let failed = events.iter().any(|e| {
            matches!(e,
                ProviderTurnEvent::ToolCallCompleted { error: Some(m), .. }
                if m == "boom")
        });
        assert!(failed, "expected failed ToolCallCompleted, got {events:?}");
    }

    // ── dispatcher: completion signals ──────────────────────────

    #[tokio::test]
    async fn session_idle_resolves_subscription_completion() {
        let router = Arc::new(EventRouter::new());
        let client = dummy_client();
        let (tx, _rx) = mpsc::channel(8);
        let sink = TurnEventSink::new(tx);
        let sub = router
            .subscribe(SESSION_ID.to_string(), sink, "/test-cwd".to_string())
            .await;

        let frame = json!({
            "type": "session.idle",
            "properties": { "sessionID": SESSION_ID }
        });
        dispatch_frame(&router, &client, &frame.to_string())
            .await
            .unwrap();

        let result = sub.wait_for_completion(Duration::from_millis(100)).await;
        assert!(
            matches!(result, Ok(_)),
            "session.idle should resolve completion Ok, got {result:?}"
        );
    }

    #[tokio::test]
    async fn session_error_resolves_subscription_err() {
        let router = Arc::new(EventRouter::new());
        let client = dummy_client();
        let (tx, _rx) = mpsc::channel(8);
        let sink = TurnEventSink::new(tx);
        let sub = router
            .subscribe(SESSION_ID.to_string(), sink, "/test-cwd".to_string())
            .await;

        let frame = json!({
            "type": "session.error",
            "properties": {
                "sessionID": SESSION_ID,
                "error": { "message": "provider rate-limited" }
            }
        });
        dispatch_frame(&router, &client, &frame.to_string())
            .await
            .unwrap();

        let result = sub.wait_for_completion(Duration::from_millis(100)).await;
        let err = result.expect_err("session.error should surface as Err");
        assert!(err.contains("rate-limited"), "got {err:?}");
    }

    // ── dispatcher: question & permission flows ─────────────────

    #[tokio::test]
    async fn question_asked_emits_user_question_event() {
        // Fixture copied from probe2.mjs. We only assert that the
        // event lands on the sink; the follow-up POST to
        // `/question/{id}/reply` will fail (connection refused to
        // 127.0.0.1:1) but the sink emission happens before that.
        let frame = json!({
            "type": "question.asked",
            "properties": {
                "id": "que_1",
                "sessionID": SESSION_ID,
                "questions": [{
                    "question": "Do you want me to proceed with the cleanup?",
                    "header": "Confirmation",
                    "options": [
                        { "label": "Yes", "description": "proceed" },
                        { "label": "No",  "description": "abort" }
                    ]
                }],
                "tool": { "messageID": "m", "callID": "functions.question:0" }
            }
        });
        let events = drive(&[frame]).await;

        let user_q = events.iter().find_map(|e| match e {
            ProviderTurnEvent::UserQuestion {
                request_id,
                questions,
            } => Some((request_id.clone(), questions.clone())),
            _ => None,
        });
        let (request_id, questions) = user_q.expect("expected UserQuestion event");
        assert!(!request_id.is_empty());
        assert_eq!(questions.len(), 1);
        assert_eq!(
            questions[0].text,
            "Do you want me to proceed with the cleanup?"
        );
        assert_eq!(questions[0].options.len(), 2);
        assert_eq!(questions[0].options[0].label, "Yes");
    }

    #[tokio::test]
    async fn permission_asked_emits_permission_request_event() {
        let frame = json!({
            "type": "permission.asked",
            "properties": {
                "id": "perm_1",
                "sessionID": SESSION_ID,
                "permission": "bash",
                "patterns": ["rm -rf /tmp/foo"],
                "metadata": { "command": "rm -rf /tmp/foo" }
            }
        });
        let events = drive(&[frame]).await;
        let saw = events
            .iter()
            .any(|e| matches!(e, ProviderTurnEvent::PermissionRequest { .. }));
        assert!(saw, "expected PermissionRequest, got {events:?}");
    }

    // ── dispatcher: drop events we don't care about ─────────────

    // ── router: idle-kill sweep ─────────────────────────────────

    #[tokio::test]
    async fn fail_all_in_flight_resolves_subscription_with_reason() {
        // Subscribe a session, then simulate the idle-kill path
        // calling `fail_all_in_flight` while the turn is still
        // waiting on completion. The subscription's
        // `wait_for_completion` must surface the supplied reason
        // as `Err` rather than hanging on the oneshot.
        let router = Arc::new(EventRouter::new());
        let (tx, _rx) = mpsc::channel(8);
        let sink = TurnEventSink::new(tx);
        let sub = router
            .subscribe(SESSION_ID.to_string(), sink, "/test-cwd".to_string())
            .await;

        router
            .fail_all_in_flight("opencode server idle-killed; respawning on next use")
            .await;

        let result = sub.wait_for_completion(Duration::from_millis(100)).await;
        let err = result.expect_err("fail_all_in_flight should surface as Err");
        assert!(
            err.contains("idle-killed"),
            "reason should appear in error message, got {err:?}"
        );
    }

    #[tokio::test]
    async fn fail_all_in_flight_is_noop_when_empty() {
        // Calling the sweep with no subscribers must not panic or
        // allocate a sender — the idle watcher runs it defensively
        // every kill, often with an already-empty map (execute_turn
        // holds a lease across wait_for_completion, so `inflight
        // == 0` already implies no subscribers).
        let router = Arc::new(EventRouter::new());
        router.fail_all_in_flight("whatever").await;
        // No assertion beyond "does not panic"; empty is the happy
        // path.
    }

    #[tokio::test]
    async fn known_ignored_events_produce_nothing() {
        let fixtures = [
            json!({ "type": "server.connected", "properties": {} }),
            json!({ "type": "server.heartbeat", "properties": {} }),
            json!({ "type": "session.updated",
                    "properties": { "sessionID": SESSION_ID, "info": {} } }),
            json!({ "type": "session.diff",
                    "properties": { "sessionID": SESSION_ID, "diff": [] } }),
            json!({ "type": "message.updated",
                    "properties": { "sessionID": SESSION_ID, "info": {} } }),
            // A completely unknown event type must not panic.
            json!({ "type": "brand.new.unknown.event",
                    "properties": { "sessionID": SESSION_ID } }),
        ];
        let events = drive(&fixtures).await;
        assert!(events.is_empty(), "known-ignored events leaked: {events:?}");
    }
}
