//! Translate bridge stream events into the provider-api
//! `ProviderTurnEvent` enum the runtime consumes.
//!
//! Extracted from `lib.rs` in the phase 3 god-file split. This is a
//! large pure function (`forward_stream`) that receives one bridge-
//! emitted `stream` message and pushes zero-or-more typed events
//! into the per-turn sink. Protocol framing lives in `wire.rs`.

use serde_json::Value;
use tracing::{debug, info, warn};

use zenui_provider_api::{BackgroundShellStatus, ProviderTurnEvent, TurnEventSink};

pub(crate) async fn forward_stream(
    events: &TurnEventSink,
    event: &str,
    delta: Option<String>,
    call_id: Option<String>,
    name: Option<String>,
    args: Option<Value>,
    output: Option<String>,
    error: Option<String>,
    message: Option<String>,
    path: Option<String>,
    operation: Option<String>,
    before: Option<String>,
    after: Option<String>,
    parent_call_id: Option<String>,
    agent_id: Option<String>,
    agent_type: Option<String>,
    prompt: Option<String>,
    plan_id: Option<String>,
    title: Option<String>,
    steps: Option<Value>,
    raw: Option<String>,
    nested_event: Option<Value>,
    model: Option<String>,
    // `is_background`: set on `tool_started` events when the bridge
    // detected `Bash { run_in_background: true }`. Threaded through
    // to `ProviderTurnEvent::ToolCallStarted::is_background`.
    is_background: Option<bool>,
    // `bash_id`: set on `background_bash_registered` (the SDK-issued
    // shell id for an originating background-Bash) and on
    // `bash_output` / `bash_killed` (the shell id whose live output
    // or termination is being delivered). Drives the host's
    // background-task panel.
    bash_id: Option<String>,
    // `shell_status`: set on `bash_output` events. Bridge-parsed
    // SDK shell state (`'running'|'completed'|'failed'|'killed'`).
    // Lets the runtime decide the panel row's terminal transition
    // without re-parsing the raw output.
    shell_status: Option<String>,
) {
    use zenui_provider_api::{FileOperation, PlanStep};
    match event {
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
                let bg = is_background.unwrap_or(false);
                info!(
                    call_id = %cid,
                    name = %n,
                    parent = ?parent_call_id,
                    is_background = bg,
                    "bridge tool_started"
                );
                events
                    .send(ProviderTurnEvent::ToolCallStarted {
                        call_id: cid,
                        name: n,
                        args: args.unwrap_or(Value::Null),
                        parent_call_id,
                        is_background: bg,
                    })
                    .await;
            } else {
                warn!("bridge tool_started missing call_id/name");
            }
        }
        "background_bash_registered" => {
            // Emitted once per background-Bash, immediately after its
            // tool_result lands at the bridge. Drives the panel row's
            // Pending → Running transition. `bash_id` is optional —
            // the bridge emits the event even when its shell-id
            // parser doesn't recognise the SDK's output shape, so the
            // row at least leaves "Starting…" instead of getting
            // stuck forever.
            if let Some(cid) = call_id {
                info!(
                    call_id = %cid,
                    bash_id = ?bash_id,
                    "bridge background_bash_registered"
                );
                events
                    .send(ProviderTurnEvent::BackgroundBashRegistered {
                        call_id: cid,
                        bash_id,
                    })
                    .await;
            } else {
                warn!("bridge background_bash_registered missing call_id");
            }
        }
        "bash_output" => {
            // Live stdout/stderr snapshot for a running background
            // shell, pre-correlated by the bridge to the originating
            // Bash call_id. The runtime updates the matching
            // ToolCall's `latest_bash_output` and re-emits a
            // BackgroundTaskUpdated for the panel.
            if let (Some(cid), Some(bid)) = (call_id, bash_id) {
                let parsed_status = match shell_status.as_deref() {
                    Some("completed") => BackgroundShellStatus::Completed,
                    Some("failed") => BackgroundShellStatus::Failed,
                    Some("killed") => BackgroundShellStatus::Killed,
                    // Default to Running so an SDK shape the bridge's
                    // parser doesn't recognise fails open — the row
                    // stays visible rather than vanishing into a
                    // bogus terminal status.
                    _ => BackgroundShellStatus::Running,
                };
                events
                    .send(ProviderTurnEvent::BackgroundBashOutput {
                        call_id: cid,
                        bash_id: bid,
                        output: output.unwrap_or_default(),
                        error,
                        shell_status: parsed_status,
                    })
                    .await;
            } else {
                warn!("bridge bash_output missing call_id/bash_id");
            }
        }
        "bash_killed" => {
            // Terminal kill of a background shell. Same shape as
            // `bash_output` but carries the row to a Killed status
            // and clears it from the active-tasks index.
            if let (Some(cid), Some(bid)) = (call_id, bash_id) {
                events
                    .send(ProviderTurnEvent::BackgroundBashKilled {
                        call_id: cid,
                        bash_id: bid,
                        output: output.unwrap_or_default(),
                        error,
                    })
                    .await;
            } else {
                warn!("bridge bash_killed missing call_id/bash_id");
            }
        }
        "tool_completed" => {
            if let Some(cid) = call_id {
                info!(
                    call_id = %cid,
                    has_error = error.is_some(),
                    output_len = output.as_ref().map(|s| s.len()).unwrap_or(0),
                    "bridge tool_completed"
                );
                events
                    .send(ProviderTurnEvent::ToolCallCompleted {
                        call_id: cid,
                        output: output.unwrap_or_default(),
                        error,
                    })
                    .await;
            } else {
                warn!("bridge tool_completed missing call_id");
            }
        }
        "file_change" => {
            if let (Some(cid), Some(path), Some(op)) = (call_id, path, operation) {
                let operation = match op.as_str() {
                    "edit" => FileOperation::Edit,
                    "delete" => FileOperation::Delete,
                    _ => FileOperation::Write,
                };
                events
                    .send(ProviderTurnEvent::FileChange {
                        call_id: cid,
                        path,
                        operation,
                        before,
                        after,
                    })
                    .await;
            }
        }
        "subagent_started" => {
            if let (Some(parent_id), Some(aid), Some(atype)) =
                (parent_call_id, agent_id, agent_type)
            {
                events
                    .send(ProviderTurnEvent::SubagentStarted {
                        parent_call_id: parent_id,
                        agent_id: aid,
                        agent_type: atype,
                        prompt: prompt.unwrap_or_default(),
                        model: model.clone(),
                    })
                    .await;
            }
        }
        "subagent_event" => {
            if let Some(aid) = agent_id {
                events
                    .send(ProviderTurnEvent::SubagentEvent {
                        agent_id: aid,
                        event: nested_event.unwrap_or(Value::Null),
                    })
                    .await;
            }
        }
        "subagent_completed" => {
            if let Some(aid) = agent_id {
                events
                    .send(ProviderTurnEvent::SubagentCompleted {
                        agent_id: aid,
                        output: output.unwrap_or_default(),
                        error,
                    })
                    .await;
            }
        }
        "subagent_model_observed" => {
            // Bridge emits this once per subagent when the first
            // assistant message under `parent_tool_use_id` arrives —
            // `message.model` is the SDK-resolved pinned id. Runtime-
            // core overwrites `SubagentRecord.model` from here.
            if let (Some(aid), Some(m)) = (agent_id, model) {
                if !m.is_empty() {
                    events
                        .send(ProviderTurnEvent::SubagentModelObserved {
                            agent_id: aid,
                            model: m,
                        })
                        .await;
                }
            }
        }
        "plan_proposed" => {
            if let (Some(pid), Some(t)) = (plan_id, title) {
                let parsed_steps: Vec<PlanStep> = steps
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
        "plan_mode_entered" => {
            // Informational — the frontend handles mode sync via the
            // tool_call_completed / permission_requested paths. Log
            // for observability.
            if let Some(cid) = call_id {
                tracing::info!(call_id = %cid, "EnterPlanMode tool detected");
            }
        }
        "info" | "warning" => {
            if let Some(msg) = message {
                events.send(ProviderTurnEvent::Info { message: msg }).await;
            }
        }
        _ => {
            debug!("Unknown bridge stream event: {event}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::sync::mpsc;
    use zenui_provider_api::TurnEventSink;

    /// Build a sink + receiver pair. The sink wraps an mpsc sender;
    /// the test body invokes `forward_stream` and then drains the
    /// receiver synchronously.
    fn make_sink() -> (TurnEventSink, mpsc::Receiver<ProviderTurnEvent>) {
        let (tx, rx) = mpsc::channel::<ProviderTurnEvent>(8);
        (TurnEventSink::new(tx), rx)
    }

    fn drain(rx: &mut mpsc::Receiver<ProviderTurnEvent>) -> Vec<ProviderTurnEvent> {
        let mut out = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            out.push(ev);
        }
        out
    }

    #[tokio::test]
    async fn tool_started_threads_is_background_flag() {
        let (sink, mut rx) = make_sink();
        forward_stream(
            &sink,
            "tool_started",
            None,
            Some("call-1".to_string()),
            Some("Bash".to_string()),
            Some(json!({"command": "tail -f /tmp/log", "run_in_background": true})),
            None, None, None, None, None, None, None, None, None, None, None,
            None, None, None, None, None, None,
            Some(true),
            None,
            None, // shell_status — irrelevant for tool_started events
        )
        .await;
        let events = drain(&mut rx);
        assert_eq!(events.len(), 1, "expected exactly one event");
        match events.into_iter().next().unwrap() {
            ProviderTurnEvent::ToolCallStarted {
                call_id,
                name,
                is_background,
                ..
            } => {
                assert_eq!(call_id, "call-1");
                assert_eq!(name, "Bash");
                assert!(
                    is_background,
                    "is_background should round-trip from wire to ProviderTurnEvent"
                );
            }
            other => panic!("expected ToolCallStarted, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn tool_started_omits_is_background_for_foreground_tools() {
        let (sink, mut rx) = make_sink();
        forward_stream(
            &sink,
            "tool_started",
            None,
            Some("call-2".to_string()),
            Some("Read".to_string()),
            Some(json!({"file_path": "/tmp/x"})),
            None, None, None, None, None, None, None, None, None, None, None,
            None, None, None, None, None, None,
            None, // bridge omits is_background for non-background calls
            None,
            None, // shell_status
        )
        .await;
        let events = drain(&mut rx);
        match events.into_iter().next().unwrap() {
            ProviderTurnEvent::ToolCallStarted { is_background, .. } => {
                assert!(!is_background, "absent flag should default to false");
            }
            other => panic!("expected ToolCallStarted, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn background_bash_registered_emits_typed_event() {
        let (sink, mut rx) = make_sink();
        // Positions after `events, event`:
        //   delta(1), call_id(2), name(3), args(4), output(5),
        //   error(6), message(7), path(8), operation(9), before(10),
        //   after(11), parent_call_id(12), agent_id(13),
        //   agent_type(14), prompt(15), plan_id(16), title(17),
        //   steps(18), raw(19), nested_event(20), model(21),
        //   is_background(22), bash_id(23). 23 trailing args + 2
        //   leading = 25 total.
        forward_stream(
            &sink,
            "background_bash_registered",
            None,                       // delta
            Some("call-3".to_string()), // call_id
            // 19 Nones for positions 3 (name) through 21 (model):
            None, None, None, None, None, None, None, None, None, None,
            None, None, None, None, None, None, None, None, None,
            None,                          // is_background
            Some("bash_42".to_string()),   // bash_id
            None,                          // shell_status
        )
        .await;
        let events = drain(&mut rx);
        match events.into_iter().next().unwrap() {
            ProviderTurnEvent::BackgroundBashRegistered { call_id, bash_id } => {
                assert_eq!(call_id, "call-3");
                assert_eq!(bash_id.as_deref(), Some("bash_42"));
            }
            other => panic!("expected BackgroundBashRegistered, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn background_bash_registered_threads_missing_bash_id() {
        // Fail-open path: the bridge couldn't parse a shell id from
        // the SDK output but emitted the registration anyway so the
        // panel row leaves the Pending state. Runtime should still
        // fire the event with `None` for bash_id.
        let (sink, mut rx) = make_sink();
        forward_stream(
            &sink,
            "background_bash_registered",
            None,
            Some("call-4".to_string()),
            None, None, None, None, None, None, None, None, None, None,
            None, None, None, None, None, None, None, None, None,
            None, // is_background
            None, // bash_id absent — fail-open
            None, // shell_status
        )
        .await;
        let events = drain(&mut rx);
        match events.into_iter().next().unwrap() {
            ProviderTurnEvent::BackgroundBashRegistered { call_id, bash_id } => {
                assert_eq!(call_id, "call-4");
                assert!(bash_id.is_none());
            }
            other => panic!("expected BackgroundBashRegistered, got {other:?}"),
        }
    }
}
