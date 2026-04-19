//! Abstract tool surface every provider exposes to its underlying
//! agent for cross-session orchestration. The schemas here are the
//! source of truth — adapter bridges read [`capability_tools`] and
//! register each entry with their provider's native tool mechanism
//! (Claude Agent SDK in-process MCP server, Codex JSON-RPC tool defs,
//! etc). The wire format, tool name, and args schema live exactly here.
//!
//! Parse/encode helpers at the bottom handle the JSON round-trip so
//! each bridge writes ~40 lines of glue instead of re-deriving the
//! call shape.

use serde_json::{Value, json};

use crate::orchestration::{RuntimeCall, RuntimeCallResult};

/// A single tool the runtime exposes to the agent. Providers register
/// these with their native tool mechanism; tool invocations are serialized
/// as a [`RuntimeCall`] by [`parse_runtime_call`] and routed through the
/// bridge RPC back to `runtime-core`.
#[derive(Debug, Clone)]
pub struct AgentCapabilityTool {
    /// Tool name as the model sees it. Convention: `flowstate_<verb>`.
    /// Stable across providers so prompts and docs can address a tool
    /// consistently.
    pub name: &'static str,
    pub description: &'static str,
    /// JSON Schema describing the tool's input args. Rendered into
    /// whatever shape the provider expects (MCP uses `inputSchema`,
    /// some bridges wrap it).
    pub input_schema: Value,
}

/// Canonical tool catalog. Ordered by expected frequency.
pub fn capability_tools() -> Vec<AgentCapabilityTool> {
    vec![
        AgentCapabilityTool {
            name: "flowstate_spawn_and_await",
            description: "Create a brand-new flowstate session (optionally in a different project) \
                with an initial user message, and block until that session produces its next \
                assistant reply. Returns the new session's id and the reply text. Use when you \
                want to delegate a self-contained task and wait for the result.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "project_id": {
                        "type": "string",
                        "description": "Target project. Omit to inherit the caller's project."
                    },
                    "provider": {
                        "type": "string",
                        "description": "Provider kind: claude, codex, github_copilot, claude_cli, github_copilot_cli. Omit to inherit the caller's provider.",
                        "enum": ["claude", "codex", "github_copilot", "claude_cli", "github_copilot_cli"]
                    },
                    "model": {
                        "type": "string",
                        "description": "Optional model override; the new session inherits the caller's default model if omitted."
                    },
                    "initial_message": {
                        "type": "string",
                        "description": "The user-message the spawned agent starts with."
                    },
                    "timeout_secs": {
                        "type": "integer",
                        "description": "Seconds to wait for the reply before giving up. Defaults to 120, max 600.",
                        "minimum": 1,
                        "maximum": 600
                    }
                },
                "required": ["initial_message"]
            }),
        },
        AgentCapabilityTool {
            name: "flowstate_spawn",
            description: "Create a brand-new flowstate session with an initial user message, \
                and return its session id immediately without waiting for a reply. Use this \
                when you want to start a peer running in the background; poll for its reply \
                with flowstate_poll.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "project_id": { "type": "string" },
                    "provider": {
                        "type": "string",
                        "enum": ["claude", "codex", "github_copilot", "claude_cli", "github_copilot_cli"]
                    },
                    "model": { "type": "string" },
                    "initial_message": { "type": "string" }
                },
                "required": ["initial_message"]
            }),
        },
        AgentCapabilityTool {
            name: "flowstate_send_and_await",
            description: "Deliver a message to an existing flowstate session and block until \
                that session's next assistant reply. Use when you want a quick round of chat \
                with an already-running peer.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "session_id": {
                        "type": "string",
                        "description": "Id of the target session (get it from flowstate_spawn, flowstate_list_sessions, or your roster)."
                    },
                    "message": { "type": "string" },
                    "timeout_secs": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 600
                    }
                },
                "required": ["session_id", "message"]
            }),
        },
        AgentCapabilityTool {
            name: "flowstate_send",
            description: "Deliver a message to an existing flowstate session without blocking. \
                If the target is idle, a turn starts immediately; otherwise the message is \
                queued for delivery at the next turn boundary.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "message": { "type": "string" }
                },
                "required": ["session_id", "message"]
            }),
        },
        AgentCapabilityTool {
            name: "flowstate_poll",
            description: "Return the most recent completed reply from the target session, \
                optionally after a specific turn id. Returns {status: 'pending'} if nothing new.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "since_turn_id": { "type": "string" }
                },
                "required": ["session_id"]
            }),
        },
        AgentCapabilityTool {
            name: "flowstate_read_session",
            description: "Read a session's summary and most-recent turns — useful for catching up \
                on a peer's conversation before messaging. `last_turns` caps how many turns come \
                back.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "last_turns": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 100
                    }
                },
                "required": ["session_id"]
            }),
        },
        AgentCapabilityTool {
            name: "flowstate_list_sessions",
            description: "List sessions you can message. Filter by `project_id` if you know it; \
                omit to list across all projects. Each entry includes a short preview of the \
                first user message and last assistant reply so you can pick the right thread \
                without a follow-up flowstate_read_session call.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "project_id": { "type": "string" }
                }
            }),
        },
        AgentCapabilityTool {
            name: "flowstate_list_projects",
            description: "List every project the runtime knows about. Returns \
                `{project_id, path}` entries. Use this when the user mentions a project by \
                name and you don't already have its id — match `path` against the user's words \
                (e.g. 'my python project' → the project whose path ends in `/python-stuff`).",
            input_schema: json!({
                "type": "object",
                "properties": {}
            }),
        },
    ]
}

/// Parse a tool call dispatched by the agent into a typed [`RuntimeCall`].
/// The bridge passes `tool_name` and `args` verbatim from the model; this
/// function picks the matching variant and validates required fields.
///
/// Returns `Err(message)` on unknown tool names or missing required args —
/// the bridge surfaces the error string as the tool result so the model
/// can self-correct.
pub fn parse_runtime_call(tool_name: &str, args: &Value) -> Result<RuntimeCall, String> {
    let obj = args
        .as_object()
        .ok_or_else(|| "tool args must be a JSON object".to_string())?;

    let get_str =
        |key: &str| -> Option<String> { obj.get(key).and_then(Value::as_str).map(str::to_string) };
    let get_required_str = |key: &str| -> Result<String, String> {
        get_str(key).ok_or_else(|| format!("missing required string `{key}`"))
    };
    let get_u32 = |key: &str| -> Option<u32> {
        obj.get(key)
            .and_then(Value::as_u64)
            .and_then(|n| u32::try_from(n).ok())
    };
    let get_u64 = |key: &str| -> Option<u64> { obj.get(key).and_then(Value::as_u64) };

    let get_provider = || -> Option<crate::ProviderKind> {
        let raw = get_str("provider")?;
        serde_json::from_value::<crate::ProviderKind>(Value::String(raw)).ok()
    };

    match tool_name {
        "flowstate_spawn_and_await" => Ok(RuntimeCall::SpawnAndAwait {
            project_id: get_str("project_id"),
            provider: get_provider(),
            model: get_str("model"),
            initial_message: get_required_str("initial_message")?,
            timeout_secs: get_u64("timeout_secs"),
        }),
        "flowstate_spawn" => Ok(RuntimeCall::Spawn {
            project_id: get_str("project_id"),
            provider: get_provider(),
            model: get_str("model"),
            initial_message: get_required_str("initial_message")?,
        }),
        "flowstate_send_and_await" => Ok(RuntimeCall::SendAndAwait {
            session_id: get_required_str("session_id")?,
            message: get_required_str("message")?,
            timeout_secs: get_u64("timeout_secs"),
        }),
        "flowstate_send" => Ok(RuntimeCall::Send {
            session_id: get_required_str("session_id")?,
            message: get_required_str("message")?,
        }),
        "flowstate_poll" => Ok(RuntimeCall::Poll {
            session_id: get_required_str("session_id")?,
            since_turn_id: get_str("since_turn_id"),
        }),
        "flowstate_read_session" => Ok(RuntimeCall::ReadSession {
            session_id: get_required_str("session_id")?,
            last_turns: get_u32("last_turns"),
        }),
        "flowstate_list_sessions" => Ok(RuntimeCall::ListSessions {
            project_id: get_str("project_id"),
        }),
        "flowstate_list_projects" => Ok(RuntimeCall::ListProjects),
        other => Err(format!("unknown runtime call tool `{other}`")),
    }
}

/// Encode a dispatcher result back into the JSON shape the bridge
/// returns to the model. Keeps the bridge side trivial.
pub fn encode_runtime_result(result: &RuntimeCallResult) -> Value {
    serde_json::to_value(result)
        .unwrap_or_else(|e| json!({ "error": format!("failed to encode runtime result: {e}") }))
}

/// Encode a dispatcher error. Bridges return this as the tool's error
/// payload so the model sees a structured failure instead of a plain
/// exception.
pub fn encode_runtime_error(err: &crate::orchestration::RuntimeCallError) -> Value {
    serde_json::to_value(err).unwrap_or_else(
        |e| json!({ "code": "internal", "message": format!("failed to encode error: {e}") }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_spawn_and_await() {
        let args = json!({
            "project_id": "proj-1",
            "initial_message": "hello",
            "timeout_secs": 60
        });
        let call = parse_runtime_call("flowstate_spawn_and_await", &args).unwrap();
        match call {
            RuntimeCall::SpawnAndAwait {
                project_id,
                initial_message,
                timeout_secs,
                ..
            } => {
                assert_eq!(project_id.as_deref(), Some("proj-1"));
                assert_eq!(initial_message, "hello");
                assert_eq!(timeout_secs, Some(60));
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn parses_send_and_await() {
        let args = json!({ "session_id": "s-1", "message": "ping" });
        let call = parse_runtime_call("flowstate_send_and_await", &args).unwrap();
        assert!(matches!(call, RuntimeCall::SendAndAwait { .. }));
    }

    #[test]
    fn rejects_unknown_tool() {
        let err = parse_runtime_call("flowstate_bogus", &json!({})).unwrap_err();
        assert!(err.contains("unknown"));
    }

    #[test]
    fn rejects_missing_required_field() {
        let err = parse_runtime_call("flowstate_spawn", &json!({})).unwrap_err();
        assert!(err.contains("initial_message"));
    }

    #[test]
    fn encodes_spawned_result() {
        let r = RuntimeCallResult::Spawned {
            session_id: "new-id".to_string(),
            reply: Some("hi".to_string()),
        };
        let v = encode_runtime_result(&r);
        assert_eq!(v["kind"], "spawned");
        assert_eq!(v["session_id"], "new-id");
        assert_eq!(v["reply"], "hi");
    }

    #[test]
    fn all_tool_schemas_are_objects() {
        for tool in capability_tools() {
            assert_eq!(tool.input_schema["type"], "object", "{}", tool.name);
        }
    }
}
