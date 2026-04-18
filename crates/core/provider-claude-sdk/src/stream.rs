//! Translate bridge stream events into the provider-api
//! `ProviderTurnEvent` enum the runtime consumes.
//!
//! Extracted from `lib.rs` in the phase 3 god-file split. This is a
//! large pure function (`forward_stream`) that receives one bridge-
//! emitted `stream` message and pushes zero-or-more typed events
//! into the per-turn sink. Protocol framing lives in `wire.rs`.

use serde_json::Value;
use tracing::{debug, info, warn};

use zenui_provider_api::{ProviderTurnEvent, TurnEventSink};

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
                info!(
                    call_id = %cid,
                    name = %n,
                    parent = ?parent_call_id,
                    "bridge tool_started"
                );
                events
                    .send(ProviderTurnEvent::ToolCallStarted {
                        call_id: cid,
                        name: n,
                        args: args.unwrap_or(Value::Null),
                        parent_call_id,
                    })
                    .await;
            } else {
                warn!("bridge tool_started missing call_id/name");
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
