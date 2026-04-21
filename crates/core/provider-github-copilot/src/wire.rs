//! TS-bridge wire protocol for the GitHub Copilot SDK adapter:
//! request/response envelopes, capability sub-shapes, and the tiny
//! helper functions that translate zenui's permission enums to/from
//! the string forms the bridge uses.
//!
//! Extracted from `lib.rs` in the phase 3 god-file split. Process
//! handle + idle-watchdog plumbing live in `process.rs`; the
//! fallback model catalog in `config.rs`.

use serde::{Deserialize, Serialize};

use zenui_provider_api::{PermissionDecision, PermissionMode, ProviderModel};

/// Result of asking the user a question: either they picked / typed, or
/// dismissed the dialog. Carried over the writer-task channel so the bridge
/// knows whether to send AnswerUserInput or CancelUserInput.
pub(crate) enum UserInputOutcome {
    Answered { answer: String, was_freeform: bool },
    Cancelled,
}

/// Bridge process wrapper for GitHub Copilot SDK

/// the Copilot SDK's `SessionSkillsListResult.skills[]` entry.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BridgeSkill {
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) description: String,
    #[serde(default)]
    #[allow(dead_code)]
    pub(crate) source: String,
    #[serde(default)]
    pub(crate) user_invocable: bool,
    #[serde(default)]
    #[allow(dead_code)]
    pub(crate) enabled: bool,
}

/// Wire shape of one sub-agent inside a `capabilities` response.
/// Mirrors `SessionAgentListResult.agents[]`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BridgeCopilotAgent {
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) description: String,
    #[serde(default)]
    #[allow(dead_code)]
    pub(crate) display_name: Option<String>,
}

/// Wire shape of one MCP server inside a `capabilities` response.
/// Mirrors `SessionMcpListResult.servers[]`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BridgeCopilotMcp {
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) status: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    pub(crate) source: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    pub(crate) error: Option<String>,
}

/// ZenUI Bridge Protocol Messages (Rust → TS)
#[derive(Debug, Serialize)]
#[serde(tag = "type")]
pub(crate) enum BridgeRequest {
    #[serde(rename = "create_session")]
    CreateSession {
        cwd: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        /// When `Some`, the bridge calls `client.resumeSession(id, …)`
        /// and only falls back to a fresh create if the SDK rejects the
        /// resume (session expired, deleted, or the upstream Copilot CLI
        /// doesn't recognise it). Sourced from
        /// `session.provider_state.native_thread_id`, which we stamp
        /// after the first successful turn.
        #[serde(skip_serializing_if = "Option::is_none")]
        resume_session_id: Option<String>,
        /// Flowstate-side session id. Propagated to the bridge so it
        /// can bake the correct `--session-id` into the flowstate
        /// MCP server registration (`SessionConfig.mcpServers.flowstate`),
        /// which in turn makes `RuntimeCall` dispatches originate
        /// from this exact session. Absent on older bridges / when
        /// the Tauri app hasn't mounted the loopback HTTP transport.
        #[serde(skip_serializing_if = "Option::is_none")]
        flowstate_session_id: Option<String>,
    },
    #[serde(rename = "send_prompt")]
    SendPrompt {
        prompt: String,
        permission_mode: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        reasoning_effort: Option<String>,
    },
    #[serde(rename = "answer_permission")]
    AnswerPermission {
        request_id: String,
        decision: String,
    },
    #[serde(rename = "answer_user_input")]
    AnswerUserInput {
        request_id: String,
        answer: String,
        was_freeform: bool,
    },
    #[serde(rename = "cancel_user_input")]
    CancelUserInput { request_id: String },
    #[serde(rename = "list_models")]
    ListModels,
    /// Enumerate the session's Copilot skills, sub-agents, and MCP
    /// servers by calling `session.rpc.{skills,agent,mcp}.list()` in
    /// the bridge. Requires a live session — callers must go through
    /// `ensure_session_process` first. Fired from
    /// [`ProviderAdapter::session_command_catalog`] on popup open.
    #[serde(rename = "list_capabilities")]
    ListCapabilities,
    #[serde(rename = "interrupt")]
    Interrupt,
}

/// ZenUI Bridge Protocol Messages (TS → Rust)
#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
pub(crate) enum BridgeResponse {
    #[serde(rename = "ready")]
    Ready,
    #[serde(rename = "session_created")]
    SessionCreated { session_id: String },
    #[serde(rename = "models")]
    Models { models: Vec<ProviderModel> },
    /// Response to `BridgeRequest::ListCapabilities`. Skills come with
    /// the SDK's `userInvocable` flag preserved — the frontend filters
    /// `!user_invocable` entries out of the popup in
    /// `mergeCommandsWithCatalog`, so the wire stays rich enough for
    /// future surfaces (e.g. a Settings pane) to inspect the complete
    /// set.
    #[serde(rename = "capabilities")]
    Capabilities {
        #[serde(default)]
        skills: Vec<BridgeSkill>,
        #[serde(default)]
        agents: Vec<BridgeCopilotAgent>,
        #[serde(default)]
        mcp_servers: Vec<BridgeCopilotMcp>,
    },
    #[serde(rename = "response")]
    Response { output: String },
    #[serde(rename = "interrupted")]
    #[allow(dead_code)]
    Interrupted,
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
        args: Option<serde_json::Value>,
        #[serde(default)]
        output: Option<String>,
        #[serde(default)]
        error: Option<String>,
        #[serde(default)]
        message: Option<String>,
        // Round-trip / plan-mode fields
        #[serde(default)]
        request_id: Option<String>,
        #[serde(default)]
        tool_name: Option<String>,
        #[serde(default)]
        input: Option<serde_json::Value>,
        #[serde(default)]
        suggested: Option<String>,
        #[serde(default)]
        question: Option<String>,
        #[serde(default)]
        choices: Option<Vec<String>>,
        #[serde(default)]
        allow_freeform: Option<bool>,
        #[serde(default)]
        plan_id: Option<String>,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        steps: Option<serde_json::Value>,
        #[serde(default)]
        raw: Option<String>,
        #[serde(default)]
        usage: Option<serde_json::Value>,
        #[serde(default)]
        rate_limit_info: Option<serde_json::Value>,
    },
}

/// GitHub Copilot Provider Adapter

pub(crate) fn permission_mode_to_str(mode: PermissionMode) -> &'static str {
    match mode {
        PermissionMode::Default => "default",
        PermissionMode::AcceptEdits => "accept_edits",
        PermissionMode::Plan => "plan",
        PermissionMode::Bypass => "bypass",
        // Copilot has no model-classifier permission mode; the UI
        // gates the "Auto" option on `supports_auto_permission_mode`
        // so this arm is defensive. Fall back to the neutral default.
        PermissionMode::Auto => "default",
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
