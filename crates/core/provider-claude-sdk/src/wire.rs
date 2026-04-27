//! TS-bridge wire protocol: request/response envelopes plus parser
//! helpers for the Claude SDK's JSON shapes (permission decisions,
//! compact triggers, user-question tool inputs).
//!
//! Extracted from `lib.rs` in the phase 3 god-file split. Process
//! handling lives in `process.rs`; RPC types in `rpc.rs`; streaming
//! event translation in `stream.rs`.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use zenui_provider_api::{
    PermissionDecision, PermissionMode, ProviderModel, ToolCatalogEntry, UserInputAnswer,
    UserInputQuestion,
};

use crate::rpc::BridgeRpcKind;

/// Result of asking the user a question: either they answered or dismissed.
/// Carried over the writer-task channel so the bridge can be told which
/// BridgeRequest to emit.
pub(crate) enum QuestionOutcome {
    Answered(Vec<UserInputAnswer>),
    Cancelled,
}

/// Wire-shape image attachment passed through to the TS bridge. Mirrors
/// `zenui_provider_api::ImageAttachment` minus the optional display
/// `name` (the bridge doesn't need it).
#[derive(Debug, Clone, Serialize)]
pub(crate) struct BridgeImageAttachment {
    pub(crate) media_type: String,
    pub(crate) data_base64: String,
}

/// Shape of a single slash command in a `capabilities` response.
/// Mirrors the Claude Agent SDK's `SlashCommand` type (camelCase on
/// the wire).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BridgeCommand {
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) description: String,
    #[serde(default)]
    pub(crate) argument_hint: Option<String>,
}

/// Shape of a sub-agent in a `capabilities` response. Mirrors the SDK's
/// `AgentInfo`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BridgeAgent {
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) description: String,
    #[serde(default)]
    #[allow(dead_code)]
    pub(crate) model: Option<String>,
}

/// Shape of an MCP server in a `capabilities` response. Subset of the
/// SDK's `McpServerStatus` — we only need name + connection state for
/// the `McpServerInfo` wire type.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BridgeMcpServer {
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) status: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    pub(crate) scope: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    pub(crate) error: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type")]
pub(crate) enum BridgeRequest {
    #[serde(rename = "create_session")]
    CreateSession {
        cwd: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        /// Persisted Claude SDK session id from a prior turn. When present the
        /// bridge hydrates `resumeSessionId` before the next send_prompt, so a
        /// zenui restart or bridge crash recovers the conversation.
        #[serde(skip_serializing_if = "Option::is_none")]
        resume_session_id: Option<String>,
        /// Forwarded to the Claude Agent SDK as `Options.title`. When
        /// supplied, the SDK uses it as the session title and skips
        /// its own auto-title generation pass — saving one extra
        /// non-essential request to the model on the first turn.
        /// Available since `@anthropic-ai/claude-agent-sdk` v0.2.113.
        /// We pass the flowstate session id today so the title in
        /// Claude's session log matches our session id; future
        /// versions can replace this with a richer human label
        /// (e.g. last_turn_preview) once that surface exists.
        #[serde(skip_serializing_if = "Option::is_none")]
        title: Option<String>,
    },
    #[serde(rename = "send_prompt")]
    SendPrompt {
        prompt: String,
        permission_mode: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        reasoning_effort: Option<String>,
        /// Thinking-mode dial orthogonal to `reasoning_effort`:
        /// `"adaptive"` (SDK decides per-turn whether to think — current
        /// SDK default) or `"always"` (force `{ type: 'enabled',
        /// budgetTokens: N }` so every turn produces reasoning, with
        /// the budget scaled by `reasoning_effort`). Absent = bridge
        /// default (`"always"`), which restores the deterministic
        /// pre-`11232b3` behavior users expect.
        #[serde(skip_serializing_if = "Option::is_none")]
        thinking_mode: Option<String>,
        /// Multimodal image attachments. When non-empty the TS bridge
        /// switches to the `query({ prompt: AsyncIterable, … })` form
        /// and builds a user message whose `content` array carries
        /// text + base64 image blocks.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<BridgeImageAttachment>,
    },
    #[serde(rename = "answer_permission")]
    AnswerPermission {
        request_id: String,
        decision: String,
        /// Optional mode change to bundle with the approval. The bridge
        /// includes this in the SDK `PermissionResult`'s
        /// `updatedPermissions` so the SDK applies the mode AS PART OF
        /// accepting the tool call. This is the only path that makes
        /// the model continue executing in the new mode within the
        /// same turn — `set_permission_mode` alone is not enough when
        /// the active turn is `ExitPlanMode`.
        #[serde(skip_serializing_if = "Option::is_none")]
        permission_mode: Option<String>,
        /// Optional free-form feedback surfaced to the model on deny.
        /// The bridge passes this as the `message` field of
        /// `{behavior:'deny', message}` on the SDK's `PermissionResult`,
        /// so the model sees it as the tool-denial context and can
        /// iterate within the same turn (primarily used by plan-exit
        /// "Send feedback"). Ignored on allow.
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    #[serde(rename = "answer_question")]
    AnswerQuestion {
        request_id: String,
        answers: Vec<UserInputAnswer>,
    },
    #[serde(rename = "cancel_question")]
    CancelQuestion { request_id: String },
    #[serde(rename = "list_models")]
    ListModels,
    /// Enumerate the slash commands, sub-agents, and MCP servers the
    /// SDK exposes for `cwd`. Bridge spawns a throwaway `query()` with
    /// a noop prompt, reads the cached init response, and aborts —
    /// no actual API call. Fired from
    /// [`ProviderAdapter::session_command_catalog`] and safe to call
    /// on every popup open.
    #[serde(rename = "list_capabilities")]
    ListCapabilities {
        #[serde(skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        model: Option<String>,
    },
    #[serde(rename = "interrupt")]
    Interrupt,
    /// Mid-turn permission-mode switch. Bridge calls
    /// `query.setPermissionMode(...)` on the in-flight SDK Query, which
    /// applies to the rest of the current turn (and subsequent turns
    /// until changed again).
    #[serde(rename = "set_permission_mode")]
    SetPermissionMode { permission_mode: String },
    /// Mid-session model switch. Updates `this.model` on the TS bridge
    /// so that the next `query()` call uses the new model. No-op if no
    /// bridge exists yet — the runtime will pick up the new model from
    /// the session summary on the next `ensure_session_process`.
    #[serde(rename = "set_model")]
    SetModel { model: String },
    /// Request a per-category context breakdown from the live SDK
    /// Query. The bridge calls `query.getContextUsage()` and
    /// replies with `BridgeResponse::RpcResponse { request_id,
    /// kind: 'context_usage', payload | error }`. `request_id` is
    /// client-generated (UUID) so the caller can route the
    /// response through the pending-RPC map even when multiple
    /// RPCs are interleaved with the turn's stream events.
    #[serde(rename = "get_context_usage")]
    GetContextUsage { request_id: String },
    /// Rust → bridge: the runtime dispatcher resolved a cross-session
    /// orchestration call the bridge had forwarded. Exactly one of
    /// `payload` / `error` is populated. The bridge's MCP tool handler
    /// resolves its pending promise with this payload, and the
    /// underlying Claude SDK returns the result to the model as the
    /// tool's output.
    #[serde(rename = "runtime_call_response")]
    RuntimeCallResponse {
        request_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        payload: Option<Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<Value>,
    },
    /// Rust → bridge: the full orchestration tool catalog (name,
    /// description, JSON Schema) the bridge should register with its
    /// Claude Agent SDK MCP server. Sent exactly once, immediately
    /// after the bridge reports `ready`. The bridge buffers incoming
    /// `create_session` requests until the catalog lands, then builds
    /// its `createSdkMcpServer` tool list from this array instead of
    /// redeclaring schemas inline. Single source of truth lives in
    /// `zenui_provider_api::capabilities::capability_tools_wire()` —
    /// adding a tool / provider / enum variant there means zero edits
    /// on the bridge side.
    #[serde(rename = "load_tool_catalog")]
    LoadToolCatalog { tools: Vec<ToolCatalogEntry> },
    /// Append a user message to the live SDK Query *without*
    /// triggering an assistant turn. The bridge pushes the message
    /// onto its `inputQueue` with `shouldQuery: false`, which
    /// causes the SDK to persist the message into the conversation
    /// transcript but skip the post-message turn boundary — no
    /// assistant response, no tools, no billing.
    ///
    /// Useful for slipping system reminders / background context
    /// / queued user input into the transcript without paying for
    /// a turn. No-op if no Query is active on the bridge — the
    /// Rust caller is expected to have triggered at least one
    /// `send_prompt` (or `list_capabilities`) first.
    ///
    /// Available since `@anthropic-ai/claude-agent-sdk` v0.2.110
    /// (`shouldQuery` field on `SDKUserMessage`).
    #[serde(rename = "append_user_message")]
    AppendUserMessage { text: String },
    /// Rust → bridge: the user-defined MCP server list from
    /// `~/.flowstate/mcp.json`. The bridge stashes the array and
    /// merges each entry into every subsequent `createSession`'s
    /// `SessionConfig.mcpServers` map, alongside the in-process
    /// flowstate orchestration entry. Sent right after the
    /// `LoadToolCatalog` handshake so the very first session sees
    /// the user MCPs. The flowstate key is reserved — the Rust
    /// side guarantees no entry with that name is shipped, and the
    /// bridge defends in depth by always writing the orchestration
    /// entry last.
    #[serde(rename = "set_user_mcp_servers")]
    SetUserMcpServers { servers: Vec<UserMcpEntry> },
}

/// Wire-shape user MCP entry shipped to the bridge. Fields mirror
/// [`zenui_provider_api::McpServerConfig`] but flatten the
/// transport-specific union — `command`/`args`/`env` populated for
/// stdio, `url` populated for http/sse. The bridge picks the
/// transport branch based on `transport`.
#[derive(Debug, Serialize)]
pub(crate) struct UserMcpEntry {
    pub(crate) name: String,
    pub(crate) transport: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) command: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub(crate) args: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) env: Option<std::collections::BTreeMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) url: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
pub(crate) enum BridgeResponse {
    #[serde(rename = "ready")]
    Ready,
    #[serde(rename = "session_created")]
    SessionCreated { session_id: String },
    #[serde(rename = "models")]
    Models { models: Vec<ProviderModel> },
    /// Response to `BridgeRequest::ListCapabilities`. Carries the
    /// SDK-reported slash commands, sub-agents, and MCP servers for
    /// a given cwd. Descriptions come straight from the SDK so the
    /// popup renders them as-is; the adapter only adds stable ids.
    #[serde(rename = "capabilities")]
    Capabilities {
        #[serde(default)]
        commands: Vec<BridgeCommand>,
        #[serde(default)]
        agents: Vec<BridgeAgent>,
        #[serde(default)]
        mcp_servers: Vec<BridgeMcpServer>,
    },
    #[serde(rename = "response")]
    Response {
        output: String,
        /// Claude SDK session id captured from the init/result messages in the
        /// bridge. Round-tripped back to the Rust side so we can persist it on
        /// `session.provider_state.native_thread_id` and resume on the next turn.
        #[serde(default)]
        session_id: Option<String>,
    },
    #[serde(rename = "interrupted")]
    #[allow(dead_code)]
    Interrupted,
    /// Ack emitted by the bridge after `query.setPermissionMode(...)` resolves.
    /// Fire-and-forget on the Rust side: nothing awaits this, we just need a
    /// variant so serde doesn't fail the whole turn on an unknown `type`.
    #[serde(rename = "permission_mode_set")]
    #[allow(dead_code)]
    PermissionModeSet { mode: String },
    /// Response to a mid-turn RPC request. `request_id` is echoed
    /// from the originating `BridgeRequest`, `kind` names which RPC
    /// this is the response to, and exactly one of `payload` /
    /// `error` is populated. Drain-loop looks up `request_id` in the
    /// session's `pending_rpcs` map and forwards the response
    /// through the registered oneshot.
    #[serde(rename = "rpc_response")]
    RpcResponse {
        request_id: String,
        kind: BridgeRpcKind,
        #[serde(default)]
        payload: Option<Value>,
        #[serde(default)]
        error: Option<String>,
    },
    /// Bridge → Rust: the agent invoked a flowstate_* MCP tool and the
    /// bridge needs the runtime to dispatch the orchestration call.
    /// The adapter forwards the call through its `TurnEventSink`,
    /// awaits the dispatcher, and writes back a
    /// `BridgeRequest::RuntimeCallResponse` with the result. Bridge-
    /// generated UUID so the bridge can match the response to the
    /// originating tool call promise.
    #[serde(rename = "runtime_call_request")]
    RuntimeCallRequest {
        request_id: String,
        tool_name: String,
        #[serde(default)]
        args: Value,
    },
    #[serde(rename = "error")]
    Error { error: String },
    /// Streaming event emitted during send_prompt.
    #[serde(rename = "stream")]
    Stream {
        event: String,
        #[serde(default)]
        delta: Option<String>,
        #[serde(default)]
        call_id: Option<String>,
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        args: Option<Value>,
        #[serde(default)]
        output: Option<String>,
        #[serde(default)]
        error: Option<String>,
        #[serde(default)]
        message: Option<String>,
        #[serde(default)]
        request_id: Option<String>,
        #[serde(default)]
        tool_name: Option<String>,
        #[serde(default)]
        input: Option<Value>,
        #[serde(default)]
        suggested: Option<String>,
        #[serde(default)]
        question: Option<String>,
        #[serde(default)]
        questions: Option<Value>,
        #[serde(default)]
        path: Option<String>,
        #[serde(default)]
        operation: Option<String>,
        #[serde(default)]
        before: Option<String>,
        #[serde(default)]
        after: Option<String>,
        #[serde(default)]
        parent_call_id: Option<String>,
        #[serde(default)]
        agent_id: Option<String>,
        #[serde(default)]
        agent_type: Option<String>,
        #[serde(default)]
        prompt: Option<String>,
        #[serde(default)]
        plan_id: Option<String>,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        steps: Option<Value>,
        #[serde(default)]
        raw: Option<String>,
        #[serde(default)]
        nested_event: Option<Value>,
        #[serde(default)]
        usage: Option<Value>,
        #[serde(default)]
        rate_limit_info: Option<Value>,
        /// Populated on `model_resolved` events. Carries the pinned
        /// model id the SDK settled on for the current turn (e.g.
        /// `claude-sonnet-4-5-20250929`), which may differ from the
        /// alias the adapter originally asked for (`sonnet`).
        #[serde(default)]
        model: Option<String>,
        /// Populated on `compact_boundary` / `compact_summary`.
        /// Kept as free-form Value so the adapter can parse the
        /// structured payload (trigger, token counts, summary text)
        /// without bloating this struct with six more flat fields.
        #[serde(default)]
        trigger: Option<String>,
        #[serde(default)]
        pre_tokens: Option<u64>,
        #[serde(default)]
        post_tokens: Option<u64>,
        #[serde(default)]
        duration_ms: Option<u64>,
        #[serde(default)]
        summary: Option<String>,
        /// Populated on `memory_recall`. `mode` is `'select' |
        /// 'synthesize'`; `memories` is the raw array the SDK
        /// surfaced (path / scope / optional content per entry).
        #[serde(default)]
        mode: Option<String>,
        #[serde(default)]
        memories: Option<Value>,
        /// Populated on `turn_status`. Bridge maps the SDK's
        /// `status: compacting | requesting | null` to our coarse
        /// phase strings (`idle | requesting | streaming |
        /// compacting | awaiting_input`).
        #[serde(default)]
        phase: Option<String>,
        /// Populated on `api_retry` events.
        #[serde(default)]
        attempt: Option<u32>,
        #[serde(default)]
        max_retries: Option<u32>,
        #[serde(default)]
        retry_delay_ms: Option<u64>,
        #[serde(default)]
        error_status: Option<u16>,
        /// Populated on `prompt_suggestion` events — the SDK's
        /// predicted next user prompt.
        #[serde(default)]
        suggestion: Option<String>,
        /// Populated on `tool_progress` events. Seconds since the
        /// SDK started the tool call; mirrored for display only —
        /// the frontend's stalled-tool detector relies on
        /// `occurred_at` for freshness, not this field.
        #[serde(default)]
        elapsed_time_seconds: Option<f64>,
        /// Populated on `tool_progress` events. ISO 8601 timestamp
        /// stamped by the bridge at the moment the SDK's heartbeat
        /// arrived. The runtime stores this as
        /// `ToolCall::last_progress_at` so the stalled-tool pip
        /// can compare against wall time.
        #[serde(default)]
        occurred_at: Option<String>,
    },
}

pub(crate) fn permission_mode_to_str(mode: PermissionMode) -> &'static str {
    match mode {
        PermissionMode::Default => "default",
        PermissionMode::AcceptEdits => "acceptEdits",
        PermissionMode::Plan => "plan",
        PermissionMode::Bypass => "bypassPermissions",
        // The Claude Agent SDK exposes `'auto'` as a sixth
        // PermissionMode where a built-in model classifier decides
        // which tool calls to auto-approve vs escalate to
        // `canUseTool`. See sdk.d.ts PermissionMode union, v0.2.112+.
        PermissionMode::Auto => "auto",
    }
}

pub(crate) fn permission_decision_to_str(decision: PermissionDecision) -> &'static str {
    match decision {
        PermissionDecision::Allow => "allow",
        PermissionDecision::AllowAlways => "allow_always",
        PermissionDecision::Deny => "deny",
        PermissionDecision::DenyAlways => "deny_always",
    }
}

pub(crate) fn parse_decision(value: &str) -> PermissionDecision {
    match value {
        "allow_always" => PermissionDecision::AllowAlways,
        "deny" => PermissionDecision::Deny,
        "deny_always" => PermissionDecision::DenyAlways,
        _ => PermissionDecision::Allow,
    }
}

pub(crate) fn parse_compact_trigger(value: Option<&str>) -> zenui_provider_api::CompactTrigger {
    match value {
        Some("manual") => zenui_provider_api::CompactTrigger::Manual,
        _ => zenui_provider_api::CompactTrigger::Auto,
    }
}

/// Parse Claude SDK's `AskUserQuestion` tool input into zenui's cross-provider
/// question list. Claude's shape is
/// `{ questions: [{ question, header, options: [{label, description}], multiSelect }] }`,
/// per https://code.claude.com/docs/en/agent-sdk/user-input. Question ids are
/// synthesized as `q{i}` (matching the Claude-CLI adapter) and option ids are
/// `q{i}_opt{j}` via the shared `parse_options_from_value` helper. The bridge's
/// `answerQuestion` strips the `q` prefix to recover the original question
/// index and look up the text Claude expects as the `updatedInput.answers`
/// map key.
pub(crate) fn parse_claude_questions(raw: Option<&Value>) -> Vec<UserInputQuestion> {
    let Some(array) = raw.and_then(Value::as_array) else {
        return Vec::new();
    };
    array
        .iter()
        .enumerate()
        .map(|(i, q)| {
            let id = format!("q{i}");
            let options = zenui_provider_api::parse_options_from_value(q.get("options"), &id);
            UserInputQuestion {
                id,
                text: q
                    .get("question")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                header: q.get("header").and_then(Value::as_str).map(str::to_string),
                options,
                multi_select: q
                    .get("multiSelect")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                // Claude docs: no explicit allowFreeform flag; the client may always
                // accept a free-form answer by passing the user's typed text as the value.
                allow_freeform: true,
                is_secret: false,
            }
        })
        .collect()
}
