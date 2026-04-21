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

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::orchestration::{RuntimeCall, RuntimeCallResult};
use crate::{PermissionMode, ProviderKind, ReasoningEffort};

// --- Single-source enum tag arrays ------------------------------------
//
// The JSON Schema `"enum"` arrays below are derived from the Rust
// wire enums' `ALL` / `as_tag` helpers. Adding a variant means editing
// the enum in `types.rs` — these schemas and every bridge that consumes
// them pick up the change automatically. Previously these arrays were
// hand-typed in three places inside this file AND again in each
// provider bridge's tool declaration; see
// `crates/core/provider-claude-sdk/bridge/src/index.ts` for how the
// Claude bridge now consumes `capability_tools()` directly instead of
// re-declaring the schemas.

fn provider_kind_tags() -> Vec<&'static str> {
    ProviderKind::ALL.iter().map(|k| k.as_tag()).collect()
}

fn permission_mode_tags() -> Vec<&'static str> {
    PermissionMode::ALL.iter().map(|m| m.as_tag()).collect()
}

fn reasoning_effort_tags() -> Vec<&'static str> {
    ReasoningEffort::ALL.iter().map(|e| e.as_tag()).collect()
}

/// Human-readable provider list used in tool descriptions. Kept in
/// sync with [`ProviderKind::ALL`] via the same iterator so "Provider
/// kind: …" copy in the description never drifts from the `enum` array.
fn provider_kind_description_list() -> String {
    provider_kind_tags().join(", ")
}

/// A single tool the runtime exposes to the agent. Providers register
/// these with their native tool mechanism; tool invocations are serialized
/// as a [`RuntimeCall`] by [`parse_runtime_call`] and routed through the
/// bridge RPC back to `runtime-core`.
#[derive(Debug, Clone)]
pub struct AgentCapabilityTool {
    /// Canonical tool name. Providers using MCP-style transports (Claude
    /// Agent SDK) namespace this under a server prefix on the wire — the
    /// model sees `mcp__flowstate__<name>` — so we deliberately leave the
    /// name itself bare. Other providers may add their own prefix when
    /// they wire up.
    pub name: &'static str,
    pub description: &'static str,
    /// JSON Schema describing the tool's input args. Rendered into
    /// whatever shape the provider expects (MCP uses `inputSchema`,
    /// some bridges wrap it).
    pub input_schema: Value,
}

/// Wire form of [`AgentCapabilityTool`] with owned `String`s so it
/// round-trips cleanly over the bridge RPC. Built from the in-process
/// catalog by [`capability_tools_wire`] — bridges deserialize this
/// vector at startup and feed each entry into their native tool
/// registration (Claude SDK `createSdkMcpServer`, Copilot SDK tool
/// registry, opencode HTTP `/tools`, Codex JSON-RPC tool defs). Wire
/// schema is intentionally trivial so new provider bridges only need
/// a JSON-Schema-to-native-tool adapter, not a re-declaration of the
/// catalog itself.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-bindings", derive(ts_rs::TS))]
pub struct ToolCatalogEntry {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

impl From<&AgentCapabilityTool> for ToolCatalogEntry {
    fn from(t: &AgentCapabilityTool) -> Self {
        Self {
            name: t.name.to_string(),
            description: t.description.to_string(),
            input_schema: t.input_schema.clone(),
        }
    }
}

/// Same catalog as [`capability_tools`], wire-friendly. Every provider
/// bridge consumes this at startup — it's the single source of truth for
/// the orchestration tool surface exposed to an LLM.
pub fn capability_tools_wire() -> Vec<ToolCatalogEntry> {
    capability_tools()
        .iter()
        .map(ToolCatalogEntry::from)
        .collect()
}

/// Canonical tool catalog. Ordered by expected frequency.
pub fn capability_tools() -> Vec<AgentCapabilityTool> {
    vec![
        AgentCapabilityTool {
            name: "spawn_and_await",
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
                        "description": format!("Provider kind: {}. Omit to inherit the caller's provider.", provider_kind_description_list()),
                        "enum": provider_kind_tags()
                    },
                    "model": {
                        "type": "string",
                        "description": "Optional model override; the new session inherits the caller's default model if omitted."
                    },
                    "initial_message": {
                        "type": "string",
                        "description": "The user-message the spawned agent starts with."
                    },
                    "permission_mode": {
                        "type": "string",
                        "description": "Permission mode for the spawned session's first turn. Omit for default (strictest).",
                        "enum": permission_mode_tags()
                    },
                    "reasoning_effort": {
                        "type": "string",
                        "description": "Reasoning effort for the first turn. Honoured by providers where thinking_effort is supported (Claude, Codex, opencode). `xhigh` / `max` are further gated per-model; unsupported values are clamped.",
                        "enum": reasoning_effort_tags()
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
            name: "spawn",
            description: "Create a brand-new flowstate session with an initial user message, \
                and return its session id immediately without waiting for a reply. Use this \
                when you want to start a peer running in the background; poll for its reply \
                with the poll tool.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "project_id": { "type": "string" },
                    "provider": {
                        "type": "string",
                        "enum": provider_kind_tags()
                    },
                    "model": { "type": "string" },
                    "initial_message": { "type": "string" },
                    "permission_mode": {
                        "type": "string",
                        "description": "Permission mode for the spawned session's first turn. Omit for default (strictest).",
                        "enum": permission_mode_tags()
                    },
                    "reasoning_effort": {
                        "type": "string",
                        "description": "Reasoning effort for the first turn. Honoured by providers where thinking_effort is supported (Claude, Codex, opencode). `xhigh` / `max` are further gated per-model; unsupported values are clamped.",
                        "enum": reasoning_effort_tags()
                    }
                },
                "required": ["initial_message"]
            }),
        },
        AgentCapabilityTool {
            name: "send_and_await",
            description: "Deliver a message to an existing flowstate session and block until \
                that session's next assistant reply. Use when you want a quick round of chat \
                with an already-running peer.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "session_id": {
                        "type": "string",
                        "description": "Id of the target session (get it from a previous spawn call, list_sessions, or your roster)."
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
            name: "send",
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
            name: "poll",
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
            name: "read_session",
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
            name: "list_sessions",
            description: "List sessions you can message. Filter by `project_id` if you know it; \
                omit to list across all projects. Each entry includes a short preview of the \
                first user message and last assistant reply so you can pick the right thread \
                without a follow-up read_session call.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "project_id": { "type": "string" }
                }
            }),
        },
        AgentCapabilityTool {
            name: "list_projects",
            description: "List every project the runtime knows about. Returns \
                `{project_id, path}` entries. Use this when the user mentions a project by \
                name and you don't already have its id — match `path` against the user's words \
                (e.g. 'my python project' → the project whose path ends in `/python-stuff`).",
            input_schema: json!({
                "type": "object",
                "properties": {}
            }),
        },
        AgentCapabilityTool {
            name: "list_providers",
            description: "Enumerate every provider flowstate knows about, with their \
                available models, per-model reasoning effort levels, feature flags, \
                and the wire-level `permission_mode` / `reasoning_effort` vocabularies \
                the spawn tools accept. Use this before `spawn` / `spawn_and_await` / \
                `spawn_in_worktree` whenever you're unsure about the exact `provider` \
                tag (e.g. is it `opencode` or `open_code`?) or `model` string (e.g. \
                `opencode/kimi-k2.5` vs `moonshotai/kimi-k2` — the former is correct for \
                opencode, the latter is a different catalog entirely). Returns \
                `{providers: [{kind, label, enabled, status, status_message?, features, \
                models: [{value, label, supports_effort, supported_effort_levels, ...}]}], \
                permission_modes, reasoning_efforts}`. Pass `provider = kind`, \
                `model = models[].value` verbatim to the spawn tools.",
            input_schema: json!({
                "type": "object",
                "properties": {}
            }),
        },
        AgentCapabilityTool {
            name: "create_worktree",
            description: "Create a git worktree off an existing project. Runs \
                `git worktree add` at a host-chosen path, creates a new flowstate project \
                for it, and links it to the parent. Returns the new `{project_id, path, \
                branch, parent_project_id}`. Pass `create_branch: true` to create a fresh \
                branch from `base_ref` (defaults to HEAD); pass `create_branch: false` to \
                check out an existing branch. Use this when the user says something like \
                'spin up a worktree for fix/login and work there'.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "base_project_id": {
                        "type": "string",
                        "description": "Project to branch the worktree from. Get it from list_projects."
                    },
                    "branch": {
                        "type": "string",
                        "description": "Branch name to check out in the new worktree."
                    },
                    "base_ref": {
                        "type": "string",
                        "description": "When create_branch is true, git forks the new branch from this ref. Defaults to HEAD."
                    },
                    "create_branch": {
                        "type": "boolean",
                        "description": "true (default): create the branch fresh. false: check out an already-existing branch."
                    }
                },
                "required": ["base_project_id", "branch"]
            }),
        },
        AgentCapabilityTool {
            name: "list_worktrees",
            description: "List git worktrees the runtime knows about. Pass \
                `base_project_id` to filter to worktrees descending from one project; omit \
                to list every worktree. Useful when the user says 'continue the work I \
                started in that worktree'.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "base_project_id": { "type": "string" }
                }
            }),
        },
        AgentCapabilityTool {
            name: "spawn_in_worktree",
            description: "Convenience combo: create a git worktree for `branch` off \
                `base_project_id`, then spawn a new session inside it with `initial_message`. \
                If `await_reply` is true, blocks until the new session produces its first \
                assistant reply (same contract as spawn_and_await). Returns the worktree \
                metadata and the new session id.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "base_project_id": { "type": "string" },
                    "branch": { "type": "string" },
                    "base_ref": { "type": "string" },
                    "create_branch": { "type": "boolean" },
                    "initial_message": { "type": "string" },
                    "provider": {
                        "type": "string",
                        "enum": provider_kind_tags()
                    },
                    "model": { "type": "string" },
                    "await_reply": { "type": "boolean" },
                    "permission_mode": {
                        "type": "string",
                        "description": "Permission mode for the spawned session's first turn. Omit for default (strictest).",
                        "enum": permission_mode_tags()
                    },
                    "reasoning_effort": {
                        "type": "string",
                        "description": "Reasoning effort for the first turn. Honoured by providers where thinking_effort is supported (Claude, Codex, opencode). `xhigh` / `max` are further gated per-model; unsupported values are clamped.",
                        "enum": reasoning_effort_tags()
                    },
                    "timeout_secs": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 600
                    }
                },
                "required": ["base_project_id", "branch", "initial_message"]
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
    // Invalid enum strings fall back to `None` instead of failing the
    // whole call — matches `get_provider` above and keeps a typo on
    // `permission_mode: "wat"` from making the whole spawn unreachable.
    let get_permission_mode = || -> Option<crate::PermissionMode> {
        let raw = get_str("permission_mode")?;
        serde_json::from_value::<crate::PermissionMode>(Value::String(raw)).ok()
    };
    let get_reasoning_effort = || -> Option<crate::ReasoningEffort> {
        let raw = get_str("reasoning_effort")?;
        serde_json::from_value::<crate::ReasoningEffort>(Value::String(raw)).ok()
    };

    match tool_name {
        "spawn_and_await" => Ok(RuntimeCall::SpawnAndAwait {
            project_id: get_str("project_id"),
            provider: get_provider(),
            model: get_str("model"),
            initial_message: get_required_str("initial_message")?,
            permission_mode: get_permission_mode(),
            reasoning_effort: get_reasoning_effort(),
            timeout_secs: get_u64("timeout_secs"),
        }),
        "spawn" => Ok(RuntimeCall::Spawn {
            project_id: get_str("project_id"),
            provider: get_provider(),
            model: get_str("model"),
            initial_message: get_required_str("initial_message")?,
            permission_mode: get_permission_mode(),
            reasoning_effort: get_reasoning_effort(),
        }),
        "send_and_await" => Ok(RuntimeCall::SendAndAwait {
            session_id: get_required_str("session_id")?,
            message: get_required_str("message")?,
            timeout_secs: get_u64("timeout_secs"),
        }),
        "send" => Ok(RuntimeCall::Send {
            session_id: get_required_str("session_id")?,
            message: get_required_str("message")?,
        }),
        "poll" => Ok(RuntimeCall::Poll {
            session_id: get_required_str("session_id")?,
            since_turn_id: get_str("since_turn_id"),
        }),
        "read_session" => Ok(RuntimeCall::ReadSession {
            session_id: get_required_str("session_id")?,
            last_turns: get_u32("last_turns"),
        }),
        "list_sessions" => Ok(RuntimeCall::ListSessions {
            project_id: get_str("project_id"),
        }),
        "list_projects" => Ok(RuntimeCall::ListProjects),
        "list_providers" => Ok(RuntimeCall::ListProviders),
        "create_worktree" => Ok(RuntimeCall::CreateWorktree {
            base_project_id: get_required_str("base_project_id")?,
            branch: get_required_str("branch")?,
            base_ref: get_str("base_ref"),
            create_branch: obj.get("create_branch").and_then(Value::as_bool),
        }),
        "list_worktrees" => Ok(RuntimeCall::ListWorktrees {
            base_project_id: get_str("base_project_id"),
        }),
        "spawn_in_worktree" => Ok(RuntimeCall::SpawnInWorktree {
            base_project_id: get_required_str("base_project_id")?,
            branch: get_required_str("branch")?,
            base_ref: get_str("base_ref"),
            create_branch: obj.get("create_branch").and_then(Value::as_bool),
            initial_message: get_required_str("initial_message")?,
            provider: get_provider(),
            model: get_str("model"),
            permission_mode: get_permission_mode(),
            reasoning_effort: get_reasoning_effort(),
            await_reply: obj.get("await_reply").and_then(Value::as_bool),
            timeout_secs: get_u64("timeout_secs"),
        }),
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
        let call = parse_runtime_call("spawn_and_await", &args).unwrap();
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
        let call = parse_runtime_call("send_and_await", &args).unwrap();
        assert!(matches!(call, RuntimeCall::SendAndAwait { .. }));
    }

    #[test]
    fn rejects_unknown_tool() {
        let err = parse_runtime_call("flowstate_bogus", &json!({})).unwrap_err();
        assert!(err.contains("unknown"));
    }

    #[test]
    fn rejects_missing_required_field() {
        let err = parse_runtime_call("spawn", &json!({})).unwrap_err();
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

    // ------- drift guards: JSON Schema enum arrays == Rust enum ALL -------
    //
    // These tests lock the cross-provider contract: every orchestration
    // tool schema's `provider` / `permission_mode` / `reasoning_effort`
    // enum array must be the wire-tag projection of the corresponding
    // Rust enum's `ALL`. Adding a new `ProviderKind` variant (e.g.
    // `opencode`) without updating every tool schema used to be a silent
    // drift source — and a stale build of the Claude bridge is what
    // blocked the first opencode spawn attempt on 2026-04-21. These
    // assertions fail the moment a future variant gets added without the
    // schemas being regenerated from `ALL`.

    fn expected_provider_tags() -> Vec<&'static str> {
        crate::ProviderKind::ALL
            .iter()
            .map(|k| k.as_tag())
            .collect()
    }

    fn expected_permission_mode_tags() -> Vec<&'static str> {
        crate::PermissionMode::ALL
            .iter()
            .map(|m| m.as_tag())
            .collect()
    }

    fn expected_reasoning_effort_tags() -> Vec<&'static str> {
        crate::ReasoningEffort::ALL
            .iter()
            .map(|e| e.as_tag())
            .collect()
    }

    fn enum_array(schema: &Value, property: &str) -> Option<Vec<String>> {
        Some(
            schema["properties"][property]["enum"]
                .as_array()?
                .iter()
                .map(|v| v.as_str().unwrap_or_default().to_string())
                .collect(),
        )
    }

    #[test]
    fn tool_schemas_provider_enum_matches_provider_kind_all() {
        let expected = expected_provider_tags();
        for tool in capability_tools() {
            if let Some(actual) = enum_array(&tool.input_schema, "provider") {
                assert_eq!(
                    actual, expected,
                    "tool `{}` provider enum drifted from ProviderKind::ALL",
                    tool.name
                );
            }
        }
    }

    #[test]
    fn tool_schemas_permission_mode_enum_matches_permission_mode_all() {
        let expected = expected_permission_mode_tags();
        for tool in capability_tools() {
            if let Some(actual) = enum_array(&tool.input_schema, "permission_mode") {
                assert_eq!(
                    actual, expected,
                    "tool `{}` permission_mode enum drifted from PermissionMode::ALL",
                    tool.name
                );
            }
        }
    }

    #[test]
    fn tool_schemas_reasoning_effort_enum_matches_reasoning_effort_all() {
        let expected = expected_reasoning_effort_tags();
        for tool in capability_tools() {
            if let Some(actual) = enum_array(&tool.input_schema, "reasoning_effort") {
                assert_eq!(
                    actual, expected,
                    "tool `{}` reasoning_effort enum drifted from ReasoningEffort::ALL",
                    tool.name
                );
            }
        }
    }

    #[test]
    fn capability_tools_wire_round_trips() {
        // Wire form must serialize and deserialize without data loss so
        // the bridge → runtime RPC stays lossless.
        let wire = capability_tools_wire();
        assert_eq!(wire.len(), capability_tools().len());
        let json = serde_json::to_string(&wire).expect("serialize wire catalog");
        let back: Vec<ToolCatalogEntry> =
            serde_json::from_str(&json).expect("deserialize wire catalog");
        assert_eq!(back.len(), wire.len());
        for (a, b) in wire.iter().zip(back.iter()) {
            assert_eq!(a.name, b.name);
            assert_eq!(a.description, b.description);
            assert_eq!(a.input_schema, b.input_schema);
        }
    }

    // ------- permission_mode + reasoning_effort on spawn* tools -------
    //
    // The three spawn tools (`spawn`, `spawn_and_await`, `spawn_in_worktree`)
    // accept optional `permission_mode` and `reasoning_effort` so an agent
    // can configure the spawned session's *first turn*. Round-trip both,
    // and verify the "omit = None" + "typo = None" contracts — a bad enum
    // string must not take the whole call down, same as the existing
    // `get_provider` behavior.

    #[test]
    fn spawn_carries_permission_mode_and_reasoning_effort() {
        let args = json!({
            "initial_message": "hi",
            "permission_mode": "plan",
            "reasoning_effort": "high"
        });
        let call = parse_runtime_call("spawn", &args).unwrap();
        match call {
            RuntimeCall::Spawn {
                permission_mode,
                reasoning_effort,
                ..
            } => {
                assert_eq!(permission_mode, Some(crate::PermissionMode::Plan));
                assert_eq!(reasoning_effort, Some(crate::ReasoningEffort::High));
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn spawn_and_await_carries_permission_mode_and_reasoning_effort() {
        let args = json!({
            "initial_message": "hi",
            "permission_mode": "auto",
            "reasoning_effort": "xhigh"
        });
        let call = parse_runtime_call("spawn_and_await", &args).unwrap();
        match call {
            RuntimeCall::SpawnAndAwait {
                permission_mode,
                reasoning_effort,
                ..
            } => {
                assert_eq!(permission_mode, Some(crate::PermissionMode::Auto));
                assert_eq!(reasoning_effort, Some(crate::ReasoningEffort::Xhigh));
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn spawn_in_worktree_carries_permission_mode_and_reasoning_effort() {
        let args = json!({
            "base_project_id": "p",
            "branch": "b",
            "initial_message": "hi",
            "permission_mode": "bypass",
            "reasoning_effort": "max"
        });
        let call = parse_runtime_call("spawn_in_worktree", &args).unwrap();
        match call {
            RuntimeCall::SpawnInWorktree {
                permission_mode,
                reasoning_effort,
                ..
            } => {
                assert_eq!(permission_mode, Some(crate::PermissionMode::Bypass));
                assert_eq!(reasoning_effort, Some(crate::ReasoningEffort::Max));
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn spawn_fields_default_to_none_when_omitted() {
        let args = json!({ "initial_message": "hi" });
        let call = parse_runtime_call("spawn", &args).unwrap();
        match call {
            RuntimeCall::Spawn {
                permission_mode,
                reasoning_effort,
                ..
            } => {
                assert!(permission_mode.is_none());
                assert!(reasoning_effort.is_none());
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn spawn_tolerates_invalid_enum_strings() {
        // Typos must not take the whole call down — mirrors
        // `get_provider`'s existing tolerance.
        let args = json!({
            "initial_message": "hi",
            "permission_mode": "wat",
            "reasoning_effort": "turbo"
        });
        let call = parse_runtime_call("spawn", &args).unwrap();
        match call {
            RuntimeCall::Spawn {
                permission_mode,
                reasoning_effort,
                ..
            } => {
                assert!(permission_mode.is_none());
                assert!(reasoning_effort.is_none());
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }
}
