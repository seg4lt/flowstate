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
use std::sync::Arc;
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
            Ok(Err(_)) => Err(
                "opencode SSE channel closed before the turn completed \
                 (server may have restarted)"
                    .to_string(),
            ),
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
    sessions: Mutex<HashMap<String, SessionState>>,
    /// Marks whether the background SSE reader has been spawned.
    /// Toggled once; subsequent `spawn_reader` calls are no-ops.
    reader_started: Mutex<bool>,
}

impl EventRouter {
    pub fn new() -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
            reader_started: Mutex::new(false),
        }
    }

    /// Register a sink for a given opencode session id and receive a
    /// [`Subscription`] that resolves when the turn finishes.
    pub async fn subscribe(
        self: &Arc<Self>,
        native_session_id: String,
        sink: TurnEventSink,
    ) -> Subscription {
        let (tx, rx) = oneshot::channel();
        let state = SessionState {
            sink,
            accumulated_output: String::new(),
            open_tool_calls: HashSet::new(),
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

    /// Spawn the background SSE reader if it isn't already running.
    /// Idempotent — the first `subscribe` kicks this off and later
    /// calls are cheap.
    pub fn spawn_reader(
        self: &Arc<Self>,
        _server: Arc<OpenCodeServer>,
        client: Arc<OpenCodeClient>,
    ) {
        let router = self.clone();
        tokio::spawn(async move {
            {
                let mut guard = router.reader_started.lock().await;
                if *guard {
                    return;
                }
                *guard = true;
            }
            read_forever(router, client).await;
        });
    }
}

/// Long-lived SSE reader. Reconnects on transient failures with a
/// short backoff so a momentary network blip doesn't orphan every
/// in-flight turn. When the enclosing `OpenCodeServer` is dropped
/// the underlying TCP connection closes and we exit.
async fn read_forever(router: Arc<EventRouter>, client: Arc<OpenCodeClient>) {
    let (user, pass) = client.credentials();
    let user = user.to_string();
    let pass = pass.to_string();
    let url = format!("{}/event", client.base_url());

    // The SSE client wants no read timeout — events arrive at their
    // own pace. We still time out the initial connect so a wedged
    // server doesn't hang the reader forever.
    let http = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .build()
        .expect("reqwest SSE client build should not fail");

    loop {
        let response = match http
            .get(&url)
            .basic_auth(&user, Some(&pass))
            .header("accept", "text/event-stream")
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
        debug!(event_type, "opencode SSE event without session id; skipping");
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
        let sink_clone = {
            let sessions = router.sessions.lock().await;
            sessions.get(&session_id).map(|s| s.sink.clone())
        };
        let Some(sink) = sink_clone else {
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
            ));
        } else {
            tokio::spawn(handle_question_asked(client.clone(), sink, props));
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
            let part_type = part
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default();

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

                    match status {
                        "pending" | "running" => {
                            // De-dupe: a single tool call fires
                            // multiple `updated` events as it
                            // progresses (args arriving, state
                            // changing). Only emit `ToolCallStarted`
                            // the first time we see the id; skip
                            // subsequent in-progress updates so the
                            // runtime doesn't log a fresh tool card
                            // per heartbeat.
                            if !call_id.is_empty()
                                && state.open_tool_calls.insert(call_id.clone())
                            {
                                let args = tool_state
                                    .get("input")
                                    .cloned()
                                    .unwrap_or(Value::Object(Default::default()));
                                state
                                    .sink
                                    .send(ProviderTurnEvent::ToolCallStarted {
                                        call_id,
                                        name,
                                        args,
                                        parent_call_id: None,
                                    })
                                    .await;
                            }
                        }
                        "completed" => {
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
                state
                    .sink
                    .send(ProviderTurnEvent::Info { message })
                    .await;
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
        "message.updated"
        | "message.removed"
        | "session.updated"
        | "session.diff"
        | "server.connected"
        | "server.heartbeat"
        | "question.rejected"
        | "session.started"
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

    let (decision, _mode_override) = sink
        .request_permission(tool_name, input, PermissionDecision::Allow)
        .await;

    let reply = PermissionReply::from_decision(decision);
    if let Err(err) = client
        .respond_to_permission(&session_id, &permission_id, reply)
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
        let header = q
            .get("header")
            .and_then(Value::as_str)
            .map(str::to_string);
        let opts = q.get("options").and_then(Value::as_array).cloned().unwrap_or_default();
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
            if let Err(err) = client.respond_to_question(&request_id, empty).await {
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

    if let Err(err) = client.respond_to_question(&request_id, reply).await {
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
        let _sub = router.subscribe(SESSION_ID.to_string(), sink).await;

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
        assert!(matches!(
            events.as_slice(),
            [ProviderTurnEvent::AssistantTextDelta { delta }] if delta == " Hello"
        ), "got {events:?}");
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
        assert!(completed, "expected ToolCallCompleted with output='ok'; got {events:?}");
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
        let sub = router.subscribe(SESSION_ID.to_string(), sink).await;

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
        let sub = router.subscribe(SESSION_ID.to_string(), sink).await;

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
        assert_eq!(questions[0].text, "Do you want me to proceed with the cleanup?");
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
