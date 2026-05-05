//! Wire-protocol data types. See lib.rs for the module layout.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "ts-bindings", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-bindings", ts(optional_fields))]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    Codex,
    #[default]
    Claude,
    #[serde(rename = "github_copilot")]
    GitHubCopilot,
    #[serde(rename = "claude_cli")]
    ClaudeCli,
    #[serde(rename = "github_copilot_cli")]
    GitHubCopilotCli,
    /// The `opencode` CLI (https://opencode.ai) — driven via its
    /// headless HTTP server (`opencode serve`) with an SSE event
    /// stream. The wire tag is the bare name `"opencode"`; the
    /// default snake-case rule would emit `"open_code"`.
    #[serde(rename = "opencode")]
    OpenCode,
}

impl ProviderKind {
    /// Every known provider variant. Keep in sync with the enum definition.
    pub const ALL: &[ProviderKind] = &[
        ProviderKind::Codex,
        ProviderKind::Claude,
        ProviderKind::GitHubCopilot,
        ProviderKind::ClaudeCli,
        ProviderKind::GitHubCopilotCli,
        ProviderKind::OpenCode,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Codex => "Codex",
            Self::Claude => "Claude",
            Self::GitHubCopilot => "GitHub Copilot",
            Self::ClaudeCli => "Claude (CLI)",
            Self::GitHubCopilotCli => "GitHub Copilot (CLI)",
            Self::OpenCode => "opencode",
        }
    }

    /// Short, stable string identifier for this variant. Matches the
    /// serde wire form (`#[serde(rename_all = "snake_case")]` plus the
    /// explicit renames on the enum). Prefer this over bespoke
    /// `match`-based codecs; keep one source of truth.
    pub fn as_tag(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
            Self::GitHubCopilot => "github_copilot",
            Self::ClaudeCli => "claude_cli",
            Self::GitHubCopilotCli => "github_copilot_cli",
            Self::OpenCode => "opencode",
        }
    }

    /// Inverse of [`Self::as_tag`]. Returns `None` for unknown tags so
    /// callers can decide how to handle drift (log + skip, error, etc.)
    /// rather than silently coercing to a wrong variant.
    pub fn from_tag(s: &str) -> Option<Self> {
        Some(match s {
            "codex" => Self::Codex,
            "claude" => Self::Claude,
            "github_copilot" => Self::GitHubCopilot,
            "claude_cli" => Self::ClaudeCli,
            "github_copilot_cli" => Self::GitHubCopilotCli,
            "opencode" => Self::OpenCode,
            _ => return None,
        })
    }
}

#[cfg(test)]
mod provider_kind_tests {
    use super::ProviderKind;

    #[test]
    fn as_tag_round_trips_for_every_variant() {
        for &kind in ProviderKind::ALL {
            let tag = kind.as_tag();
            assert_eq!(
                ProviderKind::from_tag(tag),
                Some(kind),
                "tag {tag:?} did not round-trip for {kind:?}",
            );
        }
    }

    #[test]
    fn as_tag_matches_serde_wire_form() {
        for &kind in ProviderKind::ALL {
            let json = serde_json::to_string(&kind).expect("serialize");
            // JSON strings are quoted; strip the quotes before comparing.
            let trimmed = json.trim_matches('"');
            assert_eq!(trimmed, kind.as_tag());
        }
    }

    #[test]
    fn from_tag_rejects_unknown() {
        assert_eq!(ProviderKind::from_tag("definitely-not-a-provider"), None);
        assert_eq!(ProviderKind::from_tag(""), None);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "ts-bindings", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-bindings", ts(optional_fields))]
#[serde(rename_all = "snake_case")]
pub enum ProviderStatusLevel {
    Ready,
    Warning,
    #[default]
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-bindings", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-bindings", ts(optional_fields))]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Ready,
    Running,
    Interrupted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-bindings", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-bindings", ts(optional_fields))]
#[serde(rename_all = "snake_case")]
pub enum TurnStatus {
    Running,
    Completed,
    Interrupted,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-bindings", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-bindings", ts(optional_fields))]
#[serde(rename_all = "snake_case")]
pub enum ToolCallStatus {
    Pending,
    Completed,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-bindings", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-bindings", ts(optional_fields))]
#[serde(rename_all = "snake_case")]
pub enum PermissionDecision {
    Allow,
    AllowAlways,
    Deny,
    DenyAlways,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-bindings", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-bindings", ts(optional_fields))]
#[serde(rename_all = "camelCase")]
pub struct UserInputOption {
    pub id: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-bindings", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-bindings", ts(optional_fields))]
#[serde(rename_all = "camelCase")]
pub struct UserInputQuestion {
    pub id: String,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header: Option<String>,
    #[serde(default)]
    pub options: Vec<UserInputOption>,
    #[serde(default)]
    pub multi_select: bool,
    #[serde(default)]
    pub allow_freeform: bool,
    #[serde(default)]
    pub is_secret: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-bindings", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-bindings", ts(optional_fields))]
#[serde(rename_all = "camelCase")]
pub struct UserInputAnswer {
    pub question_id: String,
    #[serde(default)]
    pub option_ids: Vec<String>,
    pub answer: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-bindings", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-bindings", ts(optional_fields))]
#[serde(rename_all = "snake_case")]
pub enum PermissionMode {
    Default,
    AcceptEdits,
    Plan,
    Bypass,
    /// Model-classifier approvals — the provider routes each tool call
    /// through an internal classifier that auto-approves low-risk calls
    /// and only falls through to `canUseTool` for tools it isn't
    /// confident about. Today only the Claude Agent SDK implements this
    /// (it maps to the SDK's `"auto"` `PermissionMode`); other adapters
    /// gate on `ProviderFeatures::supports_auto_permission_mode`.
    Auto,
}

impl Default for PermissionMode {
    fn default() -> Self {
        PermissionMode::AcceptEdits
    }
}

impl PermissionMode {
    /// Every known variant. Keep in sync with the enum definition.
    /// Used by `capabilities::capability_tools()` to derive the JSON
    /// Schema enum array from a single source so orchestration tool
    /// schemas can never drift from the wire type.
    pub const ALL: &[PermissionMode] = &[
        PermissionMode::Default,
        PermissionMode::AcceptEdits,
        PermissionMode::Plan,
        PermissionMode::Bypass,
        PermissionMode::Auto,
    ];

    /// Serde wire form. Matches `#[serde(rename_all = "snake_case")]`.
    pub fn as_tag(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::AcceptEdits => "accept_edits",
            Self::Plan => "plan",
            Self::Bypass => "bypass",
            Self::Auto => "auto",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-bindings", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-bindings", ts(optional_fields))]
#[serde(rename_all = "snake_case")]
pub enum ReasoningEffort {
    Minimal,
    Low,
    Medium,
    High,
    /// Claude Agent SDK's `EffortLevel::Xhigh` — deeper than `high`,
    /// currently only honoured by Opus 4.7+. The UI should gate this
    /// on `ProviderModel::supported_effort_levels` so older models
    /// don't expose an option they'll reject.
    Xhigh,
    /// Claude Agent SDK's `EffortLevel::Max` — maximum effort,
    /// limited to Opus 4.6/4.7+. Gated the same way as `Xhigh`.
    Max,
}

impl Default for ReasoningEffort {
    fn default() -> Self {
        ReasoningEffort::Medium
    }
}

impl ReasoningEffort {
    /// Every known variant. Keep in sync with the enum definition.
    /// Used by `capabilities::capability_tools()` to derive the JSON
    /// Schema enum array from a single source.
    pub const ALL: &[ReasoningEffort] = &[
        ReasoningEffort::Minimal,
        ReasoningEffort::Low,
        ReasoningEffort::Medium,
        ReasoningEffort::High,
        ReasoningEffort::Xhigh,
        ReasoningEffort::Max,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            ReasoningEffort::Minimal => "minimal",
            ReasoningEffort::Low => "low",
            ReasoningEffort::Medium => "medium",
            ReasoningEffort::High => "high",
            ReasoningEffort::Xhigh => "xhigh",
            ReasoningEffort::Max => "max",
        }
    }

    /// Alias for [`Self::as_str`] matching the `as_tag` naming on
    /// other wire enums (`ProviderKind::as_tag`, `PermissionMode::as_tag`).
    /// Prefer this in new code.
    #[inline]
    pub fn as_tag(self) -> &'static str {
        self.as_str()
    }
}

/// Per-turn dial orthogonal to [`ReasoningEffort`] controlling *when*
/// the provider should think, not *how much*. Currently only the
/// Claude Agent SDK adapter honours this — other adapters accept and
/// ignore the hint.
///
/// - `Always` maps to the SDK's `{ type: 'enabled', budgetTokens: N }`
///   where `N` is derived from [`ReasoningEffort`]. Produces reasoning
///   on every turn (deterministic).
/// - `Adaptive` maps to `{ type: 'adaptive' }` — the SDK / model
///   decides whether to think based on task complexity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-bindings", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-bindings", ts(optional_fields))]
#[serde(rename_all = "snake_case")]
pub enum ThinkingMode {
    /// Claude decides per-turn whether to think (SDK `adaptive`).
    Adaptive,
    /// Always think every turn with a concrete budget scaled from
    /// [`ReasoningEffort`] (SDK `enabled` + `budgetTokens`).
    Always,
}

impl Default for ThinkingMode {
    /// Restores deterministic reasoning — the pre-`11232b3` behaviour
    /// users expect.
    fn default() -> Self {
        ThinkingMode::Always
    }
}

impl ThinkingMode {
    pub fn as_str(self) -> &'static str {
        match self {
            ThinkingMode::Adaptive => "adaptive",
            ThinkingMode::Always => "always",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-bindings", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-bindings", ts(optional_fields))]
#[serde(rename_all = "snake_case")]
pub enum FileOperation {
    Write,
    Edit,
    Delete,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-bindings", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-bindings", ts(optional_fields))]
#[serde(rename_all = "snake_case")]
pub enum SubagentStatus {
    Running,
    Completed,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-bindings", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-bindings", ts(optional_fields))]
#[serde(rename_all = "snake_case")]
pub enum PlanStatus {
    Proposed,
    Accepted,
    Rejected,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-bindings", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-bindings", ts(optional_fields))]
#[serde(rename_all = "camelCase")]
pub struct PlanStep {
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-bindings", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-bindings", ts(optional_fields))]
#[serde(rename_all = "camelCase")]
pub struct PlanRecord {
    pub plan_id: String,
    pub title: String,
    pub steps: Vec<PlanStep>,
    pub raw: String,
    pub status: PlanStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-bindings", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-bindings", ts(optional_fields))]
#[serde(rename_all = "camelCase")]
pub struct FileChangeRecord {
    pub call_id: String,
    pub path: String,
    pub operation: FileOperation,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-bindings", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-bindings", ts(optional_fields))]
#[serde(rename_all = "camelCase")]
pub struct SubagentRecord {
    pub agent_id: String,
    pub parent_call_id: String,
    pub agent_type: String,
    pub prompt: String,
    /// Raw provider-level model this subagent is running on, when
    /// the provider differentiates subagent models from the main
    /// agent's. Populated lazily: set from the static agent catalog
    /// at spawn time (via `ProviderTurnEvent::SubagentStarted.model`)
    /// and upgraded to the observed value when the first assistant
    /// message lands (via `SubagentModelObserved`). `None` when the
    /// provider can't or doesn't distinguish.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub events: Vec<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub status: SubagentStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-bindings", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-bindings", ts(optional_fields))]
#[serde(rename_all = "camelCase")]
pub struct ToolCall {
    pub call_id: String,
    pub name: String,
    pub args: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub status: ToolCallStatus,
    /// When this tool call was issued from inside a sub-agent (the SDK's
    /// Task/Agent dispatch), this is the `call_id` of the parent Task
    /// tool_use that spawned the sub-agent. `None` means the call was
    /// issued directly by the main agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_call_id: Option<String>,
    /// Wall-clock timestamp (RFC 3339 / ISO 8601) of when the tool
    /// call started. Set by runtime-core on `ToolCallStarted`; the
    /// frontend reads this to render a live "Bash · 12s" elapsed
    /// counter while the call is still pending. Optional so older
    /// persisted turns deserialize cleanly; runtime-core stamps this
    /// on every fresh ToolCallStarted regardless of provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    /// Wall-clock timestamp of the most recent SDK heartbeat for
    /// this tool call (the provider's `tool_progress` event). Drives
    /// the per-tool stalled-tool pip on the frontend: when this
    /// timestamp grows older than the threshold (≈30s) while the
    /// tool is still pending, the UI shows "no progress · Ns" next
    /// to the elapsed counter and the session-wide stuck banner
    /// stays out of the way. `None` when the provider doesn't emit
    /// heartbeats — the UI then falls back to the session-wide
    /// stuck detector.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_progress_at: Option<String>,
}

/// One element of an assistant turn's ordered content stream.
///
/// Models a turn the way Anthropic does — as a sequence of blocks in
/// the order they arrived from the provider — so that interleaved
/// "text, tool, text, tool" responses render in stream order rather
/// than getting flattened into "all text first, then all tools at the
/// end". The legacy `output`, `reasoning`, and `tool_calls` fields on
/// `TurnRecord` remain populated for backwards compatibility.
///
/// `Text` and `Reasoning` carry their own text segment so a single
/// turn can hold multiple separate runs interrupted by tool calls.
/// `ToolCall` references the matching entry in `TurnRecord::tool_calls`
/// by `call_id` — that's where mutable status/output live; the block
/// itself only carries position.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-bindings", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-bindings", ts(optional_fields))]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    Reasoning {
        text: String,
    },
    ToolCall {
        #[serde(rename = "callId")]
        call_id: String,
    },
    /// Conversation-recap marker emitted by the Claude Agent SDK when
    /// it compresses older turns into a summary to free up context.
    /// `summary` is `None` between receiving the `compact_boundary`
    /// system message (which has the metrics) and the `PostCompact`
    /// hook firing (which carries the text). The runtime merges the
    /// two into a single block.
    Compact {
        trigger: CompactTrigger,
        #[serde(rename = "preTokens", default, skip_serializing_if = "Option::is_none")]
        pre_tokens: Option<u64>,
        #[serde(
            rename = "postTokens",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        post_tokens: Option<u64>,
        #[serde(
            rename = "durationMs",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        duration_ms: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        summary: Option<String>,
    },
    /// "Recalled from memory" marker — the SDK's memory-recall
    /// supervisor attached one or more memory files (or a synthesis
    /// paragraph) to the turn's context. Rendered as a subtle chip
    /// that expands into the referenced paths or synthesis body.
    MemoryRecall {
        mode: MemoryRecallMode,
        memories: Vec<MemoryRecallItem>,
    },
    /// Auto-generated short label (~30 chars, "git-commit-subject"
    /// style) the provider produced for a recent batch of tool
    /// calls. The frontend uses this to collapse the referenced
    /// `ToolCall` blocks under one labeled header — mobile-style
    /// density on tool-heavy turns.
    ///
    /// `call_ids` references the `ToolCall.call_id`s the label
    /// covers (in order). Currently emitted by Claude Code's SDK
    /// only.
    ToolUseSummary {
        summary: String,
        #[serde(rename = "callIds")]
        call_ids: Vec<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-bindings", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-bindings", ts(optional_fields))]
#[serde(rename_all = "snake_case")]
pub enum CompactTrigger {
    Auto,
    Manual,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-bindings", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-bindings", ts(optional_fields))]
#[serde(rename_all = "snake_case")]
pub enum MemoryRecallMode {
    /// Full file bodies surfaced by the parallel selector. `content`
    /// on each `MemoryRecallItem` is absent; renderers lazy-load from
    /// `path`.
    Select,
    /// Sonnet-authored paragraph distilled from many tiny memories.
    /// `content` holds the paragraph; `path` is a synthesis sentinel
    /// of the form `<synthesis:DIR>`.
    Synthesize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-bindings", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-bindings", ts(optional_fields))]
#[serde(rename_all = "snake_case")]
pub enum MemoryRecallScope {
    Personal,
    Team,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-bindings", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-bindings", ts(optional_fields))]
#[serde(rename_all = "camelCase")]
pub struct MemoryRecallItem {
    pub path: String,
    pub scope: MemoryRecallScope,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
}

/// Per-category breakdown of what's currently filling the model's
/// context window. Shape follows the Claude Agent SDK's response
/// loosely: a list of named categories with their token counts,
/// plus top-level totals. Intentionally flexible — providers
/// with different internal accounting can return whatever
/// categories they track, and the frontend just renders a stacked
/// bar from whatever it gets.
///
/// Returned by the `get_context_usage` adapter RPC (cross-provider,
/// default `Ok(None)` for adapters that don't implement it).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-bindings", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-bindings", ts(optional_fields))]
#[serde(rename_all = "camelCase")]
pub struct ContextBreakdown {
    pub total_tokens: u64,
    pub max_tokens: u64,
    pub categories: Vec<ContextCategory>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-bindings", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-bindings", ts(optional_fields))]
#[serde(rename_all = "camelCase")]
pub struct ContextCategory {
    pub name: String,
    pub tokens: u64,
    /// Provider-supplied hex color (e.g. Claude SDK's palette) so
    /// the frontend can render a consistent stacked bar. Optional
    /// so providers without a color convention can omit it and
    /// the UI falls back to a deterministic hash-based colour.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-bindings", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-bindings", ts(optional_fields))]
#[serde(rename_all = "camelCase")]
pub struct ProviderModel {
    pub value: String,
    pub label: String,
    /// Authoritative context window for this model in tokens. When
    /// present, UIs prefer this over the runtime-reported
    /// `TokenUsage.context_window`, which comes from the provider's
    /// SDK and can drift (e.g. Anthropic's 1M beta context auto-
    /// negotiation reports a window the user's tier may not actually
    /// support).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    /// Authoritative maximum output tokens for this model, when known.
    /// Used for warnings and UI ceilings. Purely advisory — adapters
    /// never enforce this locally.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u64>,
    /// Whether this model honours the SDK-native `effort` parameter
    /// (`low` / `medium` / `high` / `xhigh` / `max`). Mirrors the
    /// Claude Agent SDK's `ModelInfo.supportsEffort`. When false, the
    /// UI effort selector should be hidden (or downgraded to "default")
    /// for this model.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub supports_effort: bool,
    /// The effort levels this model accepts. Populated from the Claude
    /// Agent SDK's `ModelInfo.supportedEffortLevels`. Empty means
    /// "unknown — assume all levels" (back-compat when the adapter
    /// hasn't forwarded the list yet).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub supported_effort_levels: Vec<String>,
    /// Whether Claude decides its own thinking budget on this model
    /// (`thinking: { type: 'adaptive' }`). Mirrors the Claude Agent
    /// SDK's `ModelInfo.supportsAdaptiveThinking`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub supports_adaptive_thinking: bool,
    /// Whether this model supports the model-classifier `auto`
    /// permission mode. Mirrors the Claude Agent SDK's
    /// `ModelInfo.supportsAutoMode`. Complements the provider-level
    /// `ProviderFeatures::supports_auto_permission_mode` — the UI can
    /// gate on both (provider opts in, then the active model must also
    /// support it).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub supports_auto_mode: bool,
    /// Marks a model that the provider bills at zero cost (input and
    /// output). Populated today by the opencode adapter by reading the
    /// `cost` object on each model entry from
    /// `GET /config/providers`; other adapters leave this `false`.
    /// The frontend renders a small "Free" pill next to these entries
    /// in the model picker so users can spot them at a glance in long
    /// catalogs (opencode's flattened provider/model list routinely
    /// exceeds 30 entries).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_free: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-bindings", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-bindings", ts(optional_fields))]
#[serde(rename_all = "camelCase")]
pub struct ProviderStatus {
    pub kind: ProviderKind,
    pub label: String,
    pub installed: bool,
    pub authenticated: bool,
    pub version: Option<String>,
    pub status: ProviderStatusLevel,
    pub message: Option<String>,
    #[serde(default)]
    pub models: Vec<ProviderModel>,
    /// Runtime toggle — when `false`, the daemon refuses new turns
    /// for this provider and the frontend greys it out in Settings and
    /// hides it from the new-session picker. Adapters themselves
    /// always emit `true`; runtime-core overwrites with the persisted
    /// value from the `provider_enablement` table before broadcasting.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Per-provider feature capability flags. Frontend UIs gate
    /// affordances on these booleans so users don't see buttons that
    /// do nothing on their current provider. Defaults to all-false;
    /// each adapter opts in to the features it actually supports.
    /// Persisted `ProviderStatus` rows from before this field existed
    /// deserialize cleanly because both the field and the struct are
    /// `#[serde(default)]`.
    #[serde(default)]
    pub features: ProviderFeatures,
}

impl Default for ProviderStatus {
    fn default() -> Self {
        Self {
            kind: ProviderKind::default(),
            label: String::new(),
            installed: false,
            authenticated: false,
            version: None,
            status: ProviderStatusLevel::Error,
            message: None,
            models: Vec::new(),
            // Mirrors the `default_true` serde default: a freshly
            // constructed status is "enabled" so callers using struct-
            // update syntax (`..Default::default()`) don't accidentally
            // disable the provider.
            enabled: true,
            features: ProviderFeatures::default(),
        }
    }
}

fn default_true() -> bool {
    true
}

/// Cross-provider capability registry.
///
/// Every flag gates a user-visible surface (a UI affordance, a menu
/// item, a popover trigger). Internal event-emission details don't
/// earn a flag here — events are cheap; UIs are not. Adding a new
/// flag is a deliberate act: it admits a new piece of UI into the
/// "which provider supports this?" decision matrix the frontend has
/// to carry.
///
/// The default is all-false so adapters opt in. New fields MUST have
/// `#[serde(default)]` behavior (provided by the enclosing
/// `#[serde(default)]` on the struct) so adding a flag doesn't break
/// clients running against an older daemon.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-bindings", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-bindings", ts(optional_fields))]
#[serde(rename_all = "camelCase", default)]
pub struct ProviderFeatures {
    /// Emits `TurnStatusChanged` events during non-streaming phases
    /// (waiting on the API, compacting). Drives the working-indicator
    /// secondary label ("Working · compacting…").
    pub status_labels: bool,
    /// Emits periodic `tool_progress` heartbeat events for running
    /// tool calls, enabling "possibly stuck" detection. The basic
    /// elapsed-time counter ("Bash · 12s") is driven by the
    /// cross-provider `ToolCall::started_at` field and does NOT
    /// depend on this flag — runtime-core stamps `started_at` on
    /// every `ToolCallStarted` regardless of provider.
    pub tool_progress: bool,
    /// Emits `TurnRetrying` events when auto-retrying a transient API
    /// error. Drives the "Retrying (2/5)…" banner above the composer.
    pub api_retries: bool,
    /// Honours `reasoning_effort` via a native thinking-budget
    /// mechanism. The UI shows the effort selector only when true —
    /// providers without real effort support get no selector instead
    /// of a silently-dropped setting.
    pub thinking_effort: bool,
    /// Supports `get_context_usage()` — click-to-reveal breakdown of
    /// what's filling context (system / tools / messages / memory).
    pub context_breakdown: bool,
    /// Emits `PromptSuggested` events after each turn. Drives the
    /// ghost-text suggestion overlay in the composer.
    pub prompt_suggestions: bool,
    /// Emits session lifecycle diagnostics (start / end). Surfaced as
    /// `Info` events in the daemon log; purely observational today.
    pub session_lifecycle_events: bool,
    /// Honours `PermissionMode::Auto` — the provider has an internal
    /// classifier that decides when to auto-approve vs escalate to the
    /// `canUseTool` callback. The UI shows the "Auto" option in the
    /// mode selector only when true, so providers that would silently
    /// treat it as `Default` don't expose a non-functional choice.
    pub supports_auto_permission_mode: bool,
    /// Adapter implements `append_user_message` against a live
    /// in-flight turn — i.e. a peer's `flowstate_send` payload can be
    /// injected directly into the running provider Query and surface
    /// to the model on its next iteration, instead of waiting for the
    /// current turn to complete and the mailbox to drain.
    ///
    /// Today only the Claude Agent SDK bridge meets this contract
    /// (`shouldQuery: false` on the live `inputQueue`). Adapters
    /// without it fall back to the legacy mailbox-drain path, which
    /// only flushes on `TurnStatus::Completed` — that's the bug this
    /// flag exists to gate around for everyone else.
    pub live_message_injection: bool,
}

/// Central registry mapping every `ProviderKind` to the capability
/// flags its adapter supports. This is the single source of truth —
/// adapters call it from `health()` and the persistence layer calls
/// it when re-hydrating a cached `ProviderStatus`, so there is no
/// path by which two copies of "what does Claude support?" can drift
/// apart.
///
/// Features are **not persisted**. They're a pure function of the
/// provider kind and the daemon build, so caching them would let an
/// older row serve stale values to a newer daemon (e.g. a row written
/// before a new flag existed reads back with the flag defaulted to
/// `false`, even though the current code would return `true`). The
/// `provider_health_cache` writer strips this field before writing
/// and the reader repopulates it from this function.
///
/// Adding a new flag: flip it on here for the adapter that gained
/// the capability, ship the daemon, done. No migration, no cache
/// invalidation, no TTL wait.
pub fn features_for_kind(kind: ProviderKind) -> ProviderFeatures {
    match kind {
        // The Claude Agent SDK bridge — everything we've wired end-
        // to-end is on. Keep this block in lockstep with the bridge's
        // actual emissions; a flag here promises a working surface.
        ProviderKind::Claude => ProviderFeatures {
            // Commit 1: thinking config replaces maxThinkingTokens.
            thinking_effort: true,

            // Commit 2: turn-phase labels + API-retry banner. The
            // basic tool-elapsed counter is driven by the cross-
            // provider `ToolCall::started_at` field and ships
            // unconditionally; `tool_progress` is the SDK's per-tool
            // heartbeat that drives the *stalled-tool* pip (and
            // demotes the session-wide stuck banner to a fallback).
            status_labels: true,
            api_retries: true,
            tool_progress: true,

            // Commit 3: ghost-text prompt suggestions via the SDK's
            // `promptSuggestions: true` option + `prompt_suggestion`
            // messages.
            prompt_suggestions: true,

            // Commit 4: SessionStart / SessionEnd hooks surface as
            // `Info` events in the daemon log for diagnostics.
            // Purely observational; no UI surface today.
            session_lifecycle_events: true,

            // Mid-turn RPC: context-breakdown popover is live via the
            // pending-RPC dispatch in `CachedBridge.pending_rpcs` +
            // `run_turn`'s drain loop arm for
            // `BridgeResponse::RpcResponse`. Only functional during
            // an active turn; the UI already gates on `isRunning`
            // alongside this flag.
            context_breakdown: true,

            // The SDK's `'auto'` permission mode routes each tool
            // call through its own model classifier before falling
            // back to our `canUseTool` callback. The bridge forwards
            // `PermissionMode::Auto` straight through to the SDK and
            // leaves `canUseTool` untouched for classifier-escalated
            // calls.
            supports_auto_permission_mode: true,

            // The bridge's `appendUserMessage` pushes onto the live
            // `inputQueue` with `shouldQuery: false`, so a peer's
            // `flowstate_send` payload reaches the model on its next
            // iteration of an in-flight turn (e.g. between Monitor
            // ticks) instead of stalling in the mailbox until the
            // turn ends.
            live_message_injection: true,
        },

        // Codex CLI adapter has native `reasoning_effort` on its
        // turn API. None of the other cross-provider features (tool
        // heartbeats, compact summaries, file checkpoints, etc.) map
        // to anything the Codex protocol surfaces today.
        ProviderKind::Codex => ProviderFeatures {
            thinking_effort: true,
            ..ProviderFeatures::default()
        },

        // Claude CLI, GitHub Copilot (SaaS and CLI) don't expose any
        // of the flagged capabilities today — the UI hides the
        // corresponding affordances when selected.
        ProviderKind::ClaudeCli | ProviderKind::GitHubCopilot | ProviderKind::GitHubCopilotCli => {
            ProviderFeatures::default()
        }

        // Opencode accepts a `variant` on the prompt body which maps
        // onto flowstate's reasoning-effort scale (low/medium/high/
        // xhigh/max). The per-model list of supported variants is
        // surfaced through `ProviderModel.supported_effort_levels`
        // so the effort selector only renders for models that
        // actually expose variants in `/config/providers`.
        ProviderKind::OpenCode => ProviderFeatures {
            thinking_effort: true,
            ..ProviderFeatures::default()
        },
    }
}

/// Where a user-authored `SKILL.md` came from. Drives the "project" /
/// "global" badge in the slash-command popup. `DiskProject` is a skill
/// discovered under the session's cwd; `DiskGlobal` lives in a home
/// directory like `~/.claude/skills` and applies across every project.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-bindings", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-bindings", ts(optional_fields))]
#[serde(rename_all = "snake_case")]
pub enum SkillSource {
    DiskGlobal,
    DiskProject,
}

/// Discriminator for entries in a provider's [`CommandCatalog`].
///
/// - `Builtin` — a native slash command exposed by the provider runtime
///   (Claude's `/compact`, `/context`; Copilot's built-ins).
/// - `UserSkill` — a user-authored `SKILL.md` discovered on disk. Carries
///   its source so the popup can badge project-local vs global skills.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-bindings", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-bindings", ts(optional_fields))]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CommandKind {
    Builtin,
    UserSkill { source: SkillSource },
}

/// A single slash-command / skill entry in a provider's session command
/// catalog. The frontend renders one popup row per `ProviderCommand`.
///
/// - `id` is stable across sessions for the same command, shaped
///   `"{provider}:{kind}:{name}"`. Used as the React key and for reducer
///   id-equality short-circuits that avoid re-renders when a catalog
///   refresh returns the same set.
/// - `user_invocable` lets providers expose internal commands in the
///   catalog without offering them to users. The Copilot SDK's
///   `customize-cloud-agent` is the canonical example. The frontend
///   filters `!user_invocable` entries before rendering.
/// - `arg_hint` is the provider's suggested argument placeholder, e.g.
///   `"[path]"`, rendered muted inline after the command name.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-bindings", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-bindings", ts(optional_fields))]
#[serde(rename_all = "camelCase")]
pub struct ProviderCommand {
    pub id: String,
    pub name: String,
    pub description: String,
    #[serde(flatten)]
    pub kind: CommandKind,
    #[serde(default = "default_true")]
    pub user_invocable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arg_hint: Option<String>,
}

/// A sub-agent exposed by a provider (currently Claude SDK's
/// `supportedAgents`). Rendered in the slash-command popup under an
/// "agent" badge; selecting one inserts the agent invocation into the
/// composer. Still fully wire-carried when unused so future UIs can
/// surface them without an adapter change.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-bindings", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-bindings", ts(optional_fields))]
#[serde(rename_all = "camelCase")]
pub struct ProviderAgent {
    pub id: String,
    pub name: String,
    pub description: String,
}

/// An MCP server the provider is aware of. Carried on the wire for
/// future UI (session header chip, Settings tab). Not rendered in the
/// slash popup in v1.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-bindings", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-bindings", ts(optional_fields))]
#[serde(rename_all = "camelCase")]
pub struct McpServerInfo {
    pub id: String,
    pub name: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

/// Everything a provider can enumerate for a given session at a point
/// in time: slash commands (builtin + user-authored disk skills),
/// sub-agents, and MCP servers. Produced by
/// [`ProviderAdapter::session_command_catalog`] and broadcast to the
/// frontend as [`RuntimeEvent::SessionCommandCatalogUpdated`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-bindings", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-bindings", ts(optional_fields))]
#[serde(rename_all = "camelCase")]
pub struct CommandCatalog {
    pub commands: Vec<ProviderCommand>,
    #[serde(default)]
    pub agents: Vec<ProviderAgent>,
    #[serde(default)]
    pub mcp_servers: Vec<McpServerInfo>,
}

/// Multimodal user input for a single turn. Adapters that only
/// support text use `input.text` and silently drop `images` after
/// logging a one-line `warn!`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-bindings", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-bindings", ts(optional_fields))]
pub struct UserInput {
    pub text: String,
    #[serde(default)]
    pub images: Vec<ImageAttachment>,
}

impl UserInput {
    pub fn from_text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            images: Vec::new(),
        }
    }
}

/// Raw image payload carried across the trait boundary while a turn
/// is in flight. The Claude SDK bridge needs the bytes to build
/// multimodal content blocks; the runtime also persists them to disk
/// before calling the adapter so they survive across restarts.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-bindings", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-bindings", ts(optional_fields))]
#[serde(rename_all = "snake_case")]
pub struct ImageAttachment {
    /// MIME type, e.g. `"image/png"`.
    pub media_type: String,
    /// Standard base64 (no `data:` prefix).
    pub data_base64: String,
    /// Display name, e.g. `"image.png"`. Not forwarded to the model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// Lightweight reference to a persisted image. Sent to the frontend
/// on session load in place of the raw bytes, so opening a thread
/// with lots of attachments stays cheap.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-bindings", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-bindings", ts(optional_fields))]
#[serde(rename_all = "camelCase")]
pub struct AttachmentRef {
    /// UUID — also the filename (sans extension) on disk.
    pub id: String,
    pub media_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub size_bytes: u64,
}

/// On-demand attachment payload, returned by the `get_attachment`
/// client message. Carries the full bytes; fetched lazily when the
/// user clicks a persisted chip.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-bindings", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-bindings", ts(optional_fields))]
#[serde(rename_all = "camelCase")]
pub struct AttachmentData {
    pub media_type: String,
    pub data_base64: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// Per-agent token breakdown for a single turn. Populated by providers
/// that can attribute tokens to individual agents running inside the
/// turn — specifically the main (parent) agent vs. each Task/Agent
/// sub-agent the SDK dispatched.
///
/// The sum of `input`, `output`, `cache_read`, and `cache_write` across
/// every `AgentUsage` in `TokenUsage.agents` MUST equal the turn-level
/// aggregate fields. The dashboard relies on that invariant when it
/// allocates cost proportionally across agents.
///
/// `agent_id` is `None` for the main-agent bucket; sub-agents carry
/// the SDK's `parent_tool_use_id` (the `Task`/`Agent` tool call id
/// that spawned them). `agent_type` is `None` for main and the subagent
/// catalog type (`"general-purpose"`, `"Explore"`, `"Plan"`, …) for
/// sub-agents. `model` is the model the provider actually ran this
/// agent on (subagents can differ from the main agent's model).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-bindings", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-bindings", ts(optional_fields))]
#[serde(rename_all = "camelCase")]
pub struct AgentUsage {
    /// SDK tool-use id of the Task/Agent that spawned this sub-agent.
    /// `None` identifies the main (parent) agent bucket.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    /// Sub-agent catalog type, e.g. `"Explore"`, `"general-purpose"`.
    /// `None` for the main agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<String>,
    /// Model the provider actually ran this agent on, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_read_tokens: u64,
    #[serde(default)]
    pub cache_write_tokens: u64,
}

/// Per-turn token accounting. All fields are optional so providers
/// can emit whatever their underlying engine reports — a minimal
/// provider only needs `input_tokens` + `output_tokens`. Cache
/// fields describe prompt caching cost savings; providers without
/// prompt caching leave them `None`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-bindings", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-bindings", ts(optional_fields))]
#[serde(rename_all = "camelCase")]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Tokens written to the provider's prompt cache this turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_tokens: Option<u64>,
    /// Tokens read from the provider's prompt cache this turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_tokens: Option<u64>,
    /// Model's max context window in tokens, when the provider
    /// knows it. UIs use this as the denominator for "N of M" fills.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    /// Approximate tokens currently occupying the model's context
    /// window — i.e. the *snapshot* size at the most recent API
    /// call, not the SDK-aggregate sum across the whole turn's tool
    /// loop. Providers should compute this as
    /// `last_call.input + last_call.cache_read + last_call.cache_write
    ///  + running_output`. UIs prefer this over summing the aggregate
    /// `input_tokens + cache_read_tokens + cache_write_tokens +
    /// output_tokens` because aggregate values multi-count the cached
    /// system prompt across each tool-loop iteration (the "51M / 1M"
    /// inflation bug). Absent on providers that don't expose per-call
    /// usage; consumers fall back to the aggregate sum in that case.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub live_context_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_cost_usd: Option<f64>,
    /// Whether the provider actually attempted to compute a cost for
    /// this turn. `Some(true)` means `total_cost_usd` is the
    /// provider's own number; `Some(false)` means the provider
    /// explicitly did not return a cost (older CLI versions, API-key
    /// sessions on plans where cost isn't surfaced, future quirks);
    /// `None` means the provider gave us no signal either way and
    /// consumers should treat it as unknown. The dashboard renders
    /// "(unknown)" when this is `Some(false)` instead of silently
    /// showing $0.00 from the `unwrap_or(0.0)` fallback in the store.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub has_cost: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Per-agent token breakdown within this turn. When present, the
    /// sum of every entry's token fields equals the turn-level
    /// `input_tokens` / `output_tokens` / `cache_*` totals above, so
    /// dashboards can allocate `total_cost_usd` across agents
    /// proportionally without double-counting. Absent for providers
    /// that can't attribute per-agent tokens, in which case consumers
    /// treat the whole turn as one "main agent" bucket.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agents: Option<Vec<AgentUsage>>,
}

/// Current status of a rate-limit bucket. Generic across providers.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "ts-bindings", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-bindings", ts(optional_fields))]
#[serde(rename_all = "snake_case")]
pub enum RateLimitStatus {
    Allowed,
    AllowedWarning,
    Rejected,
}

/// Provider-reported usage against a rate-limit bucket. The shared
/// shape is intentionally generic — each provider owns its own
/// bucket taxonomy and human-readable labels, and maps its native
/// rate-limit concepts onto this struct inside its own adapter.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-bindings", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-bindings", ts(optional_fields))]
#[serde(rename_all = "camelCase")]
pub struct RateLimitInfo {
    /// Stable provider-defined id for this bucket. Used as the map
    /// key on the client side so updates replace prior values for
    /// the same bucket. Example values: "five_hour",
    /// "requests_per_minute", "monthly_tokens".
    pub bucket: String,
    /// Human-readable label decided by the provider. Shown to the
    /// user as-is in the rate-limit UI, so providers should pick
    /// concise phrasing like "5-hour limit" or "Weekly · Opus".
    pub label: String,
    pub status: RateLimitStatus,
    /// Fraction 0.0 - 1.0 of the bucket that's currently used.
    pub utilization: f64,
    /// Unix milliseconds when the bucket resets. Absent for
    /// buckets that don't reset on a schedule.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<i64>,
    /// True when the provider is currently drawing from overage
    /// credit rather than the primary bucket allowance.
    #[serde(default)]
    pub is_using_overage: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-bindings", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-bindings", ts(optional_fields))]
#[serde(rename_all = "camelCase")]
pub struct TurnRecord {
    pub turn_id: String,
    pub input: String,
    pub output: String,
    pub status: TurnStatus,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub file_changes: Vec<FileChangeRecord>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub subagents: Vec<SubagentRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<PlanRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<PermissionMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<ReasoningEffort>,
    /// Ordered content stream — text, reasoning, and tool calls in the
    /// exact order the provider emitted them. Canonical view for UIs
    /// that want to render interleaved content correctly.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocks: Vec<ContentBlock>,
    /// References to images the user pasted on this turn. Lightweight
    /// metadata only — the full bytes live on disk and are fetched
    /// lazily via `get_attachment` when the user clicks a chip.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub input_attachments: Vec<AttachmentRef>,
    /// Token usage and cost reported by the provider when the turn
    /// finished. Absent on interrupted/failed turns and on providers
    /// that don't surface usage data yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<TokenUsage>,
}

// Display-only fields (`name`, `sort_order` here; `title`,
// `last_turn_preview` on `SessionSummary`) deliberately do NOT
// exist: they're app concerns, persisted by consuming apps in
// their own stores. See
// `rs-agent-sdk/crates/core/persistence/CLAUDE.md` for the
// boundary rule.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-bindings", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-bindings", ts(optional_fields))]
#[serde(rename_all = "camelCase")]
pub struct ProjectRecord {
    pub project_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-bindings", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-bindings", ts(optional_fields))]
#[serde(rename_all = "camelCase")]
pub struct SessionSummary {
    pub session_id: String,
    pub provider: ProviderKind,
    pub status: SessionStatus,
    pub created_at: String,
    pub updated_at: String,
    pub turn_count: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-bindings", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-bindings", ts(optional_fields))]
#[serde(rename_all = "camelCase")]
pub struct ProviderSessionState {
    pub native_thread_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-bindings", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-bindings", ts(optional_fields))]
#[serde(rename_all = "camelCase")]
pub struct SessionDetail {
    pub summary: SessionSummary,
    pub turns: Vec<TurnRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_state: Option<ProviderSessionState>,
    /// Transient working directory resolved by RuntimeCore before adapter calls.
    /// Not persisted in the database.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-bindings", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-bindings", ts(optional_fields))]
#[serde(rename_all = "camelCase")]
pub struct AppSnapshot {
    pub generated_at: String,
    pub sessions: Vec<SessionDetail>,
    #[serde(default)]
    pub projects: Vec<ProjectRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-bindings", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-bindings", ts(optional_fields))]
#[serde(rename_all = "camelCase")]
pub struct BootstrapPayload {
    pub app_name: String,
    pub generated_at: String,
    /// WebSocket endpoint for the event stream. `Some(url)` under the
    /// HTTP transport; `None` for in-proc transports like Tauri where
    /// events are already streamed through the host channel and no
    /// separate WS dial is needed. Before Phase 4.10 this was an
    /// empty string under Tauri, which frontend code could not
    /// distinguish from an HTTP misconfiguration.
    pub ws_url: Option<String>,
    pub providers: Vec<ProviderStatus>,
    pub snapshot: AppSnapshot,
    /// Current checkpoint-enablement snapshot — the global default +
    /// every project override. Shipped on bootstrap so the frontend
    /// renders settings UI without a second round-trip, and refreshed
    /// via `RuntimeEvent::CheckpointEnablementChanged`.
    pub checkpoint_settings: crate::messages::CheckpointSettings,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-bindings", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-bindings", ts(optional_fields))]
#[serde(rename_all = "camelCase")]
pub struct HealthPayload {
    pub status: String,
    pub generated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts-bindings", derive(ts_rs::TS))]
#[cfg_attr(feature = "ts-bindings", ts(optional_fields))]
#[serde(rename_all = "camelCase")]
pub struct ProviderTurnOutput {
    pub output: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_state: Option<ProviderSessionState>,
}
