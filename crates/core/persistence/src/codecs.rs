//! String ↔ enum codecs for SQLite-stored columns plus small schema
//! helpers (`ext_for_media_type`, `synthesize_blocks`,
//! `reasoning_effort_from_str`).
//!
//! Extracted from `lib.rs` in the phase 3 god-file split. Pure
//! functions; no DB access. Keeping them in one module makes it
//! obvious what the on-disk string forms are for every enum that
//! crosses the persistence boundary, and lets a future schema-
//! breaking rename be a single-file edit.

use zenui_provider_api::{
    ContentBlock, PermissionMode, ProviderKind, ReasoningEffort, SessionStatus, ToolCall,
    TurnStatus,
};

pub(crate) fn ext_for_media_type(media_type: &str) -> &'static str {
    match media_type {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        _ => "bin",
    }
}

pub(crate) fn provider_kind_from_str(value: &str) -> ProviderKind {
    match ProviderKind::from_tag(value) {
        Some(kind) => kind,
        None => {
            tracing::warn!(
                tag = value,
                "unknown provider tag in persistence; defaulting to Codex",
            );
            ProviderKind::Codex
        }
    }
}

pub(crate) fn session_status_to_str(status: SessionStatus) -> &'static str {
    match status {
        SessionStatus::Ready => "ready",
        SessionStatus::Running => "running",
        SessionStatus::Interrupted => "interrupted",
    }
}

pub(crate) fn session_status_from_str(value: &str) -> SessionStatus {
    match value {
        "running" => SessionStatus::Running,
        "interrupted" => SessionStatus::Interrupted,
        _ => SessionStatus::Ready,
    }
}

pub(crate) fn turn_status_to_str(status: TurnStatus) -> &'static str {
    match status {
        TurnStatus::Running => "running",
        TurnStatus::Completed => "completed",
        TurnStatus::Interrupted => "interrupted",
        TurnStatus::Failed => "failed",
    }
}

pub(crate) fn turn_status_from_str(value: &str) -> TurnStatus {
    match value {
        "running" => TurnStatus::Running,
        "interrupted" => TurnStatus::Interrupted,
        "failed" => TurnStatus::Failed,
        _ => TurnStatus::Completed,
    }
}

pub(crate) fn permission_mode_to_str(mode: PermissionMode) -> String {
    match mode {
        PermissionMode::Default => "default",
        PermissionMode::AcceptEdits => "accept_edits",
        PermissionMode::Plan => "plan",
        PermissionMode::Bypass => "bypass",
        PermissionMode::Auto => "auto",
    }
    .to_string()
}

pub(crate) fn permission_mode_from_str(value: &str) -> PermissionMode {
    match value {
        "default" => PermissionMode::Default,
        "plan" => PermissionMode::Plan,
        "bypass" => PermissionMode::Bypass,
        "auto" => PermissionMode::Auto,
        _ => PermissionMode::AcceptEdits,
    }
}

/// Reconstruct an ordered block list for a historical turn that was
/// persisted before `blocks_json` existed. Layout matches the old UI:
/// reasoning fold-open first, then the text body, then any tool calls.
/// Not perfect, but stable and consistent across reloads.
pub(crate) fn synthesize_blocks(
    reasoning: Option<&str>,
    output: &str,
    tool_calls: &[ToolCall],
) -> Vec<ContentBlock> {
    let mut blocks: Vec<ContentBlock> = Vec::new();
    if let Some(text) = reasoning {
        if !text.is_empty() {
            blocks.push(ContentBlock::Reasoning {
                text: text.to_string(),
            });
        }
    }
    if !output.is_empty() {
        blocks.push(ContentBlock::Text {
            text: output.to_string(),
        });
    }
    for tc in tool_calls {
        blocks.push(ContentBlock::ToolCall {
            call_id: tc.call_id.clone(),
        });
    }
    blocks
}

pub(crate) fn reasoning_effort_from_str(value: &str) -> Option<ReasoningEffort> {
    match value {
        "minimal" => Some(ReasoningEffort::Minimal),
        "low" => Some(ReasoningEffort::Low),
        "medium" => Some(ReasoningEffort::Medium),
        "high" => Some(ReasoningEffort::High),
        "xhigh" => Some(ReasoningEffort::Xhigh),
        "max" => Some(ReasoningEffort::Max),
        _ => None,
    }
}
