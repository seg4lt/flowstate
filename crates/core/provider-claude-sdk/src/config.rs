//! Claude SDK configuration surface: metadata helpers that pull
//! custom prompt/compact instructions off a session, plus the
//! hardcoded model fallback list used when the bridge hasn't yet
//! returned a dynamic catalog.
//!
//! Extracted from `lib.rs` in the phase 3 god-file split.

use serde_json::Value;

use zenui_provider_api::{ProviderModel, ProviderSessionState};

/// Pull `compactCustomInstructions` out of a session's
/// `provider_state.metadata` blob. Returns `None` for absent /
/// non-string / empty / whitespace-only values — those should all
/// collapse to "no append, use the default preset" rather than
/// "append an empty string", so the SDK gets a clean Options shape.
///
/// Key name is camelCase because `ProviderSessionState` serializes
/// with `#[serde(rename_all = "camelCase")]`; the metadata blob
/// inside is opaque JSON but we follow the same convention so the
/// flowstate user_config readers (TS) and writers (Rust) agree.
pub(crate) fn read_compact_custom_instructions(
    state: Option<&ProviderSessionState>,
) -> Option<String> {
    let metadata = state.and_then(|s| s.metadata.as_ref())?;
    let text = metadata
        .get("compactCustomInstructions")
        .and_then(Value::as_str)?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

pub(crate) fn claude_models() -> Vec<ProviderModel> {
    // Context/output ceilings follow Anthropic's public model-card
    // defaults (200k context across the 4.x family; output ceilings
    // vary by tier). The 1M beta context for Sonnet 4.5 is not
    // surfaced here because it requires an explicit opt-in header —
    // reporting 1M universally would be misleading.
    // Capability booleans (`supports_effort`, `supports_adaptive_thinking`,
    // `supports_auto_mode`) deliberately left at their struct defaults
    // (false / empty) in this static table — the SDK's own `supportedModels()`
    // response in `fetch_models` carries authoritative per-model flags and
    // overlays them via `..m`. This table is only a context-window fallback
    // for when the bridge path is unavailable.
    vec![
        ProviderModel {
            value: "claude-opus-4-6".to_string(),
            label: "Claude Opus 4.6".to_string(),
            context_window: Some(200_000),
            max_output_tokens: Some(32_000),
            ..ProviderModel::default()
        },
        ProviderModel {
            value: "claude-sonnet-4-6".to_string(),
            label: "Claude Sonnet 4.6".to_string(),
            context_window: Some(200_000),
            max_output_tokens: Some(64_000),
            ..ProviderModel::default()
        },
        ProviderModel {
            value: "claude-haiku-4-5".to_string(),
            label: "Claude Haiku 4.5".to_string(),
            context_window: Some(200_000),
            max_output_tokens: Some(64_000),
            ..ProviderModel::default()
        },
        ProviderModel {
            value: "claude-opus-4-5".to_string(),
            label: "Claude Opus 4.5".to_string(),
            context_window: Some(200_000),
            max_output_tokens: Some(32_000),
            ..ProviderModel::default()
        },
        ProviderModel {
            value: "claude-sonnet-4-5".to_string(),
            label: "Claude Sonnet 4.5".to_string(),
            context_window: Some(200_000),
            max_output_tokens: Some(64_000),
            ..ProviderModel::default()
        },
    ]
}
