//! Fallback Copilot model catalog used when the bridge hasn't yet
//! returned a live `listModels()` response. Centralised here so any
//! quarterly model-list refresh is a one-file edit.
//!
//! Extracted from `lib.rs` in the phase 3 god-file split.

use zenui_provider_api::ProviderModel;

pub(crate) fn copilot_models() -> Vec<ProviderModel> {
    // Fallback capability values used only when the live
    // `listModels()` call fails or returns an empty list. Live
    // responses beat these via `fetch_models` (which now carries
    // `context_window` / `max_output_tokens` straight through from
    // the Copilot SDK's ModelCapabilities.limits). Numbers follow
    // each vendor's public model cards.
    vec![
        ProviderModel {
            value: "gpt-4.1".to_string(),
            label: "GPT-4.1".to_string(),
            context_window: Some(1_047_576),
            max_output_tokens: Some(32_768),
            ..ProviderModel::default()
        },
        ProviderModel {
            value: "gpt-4o".to_string(),
            label: "GPT-4o".to_string(),
            context_window: Some(128_000),
            max_output_tokens: Some(16_384),
            ..ProviderModel::default()
        },
        ProviderModel {
            value: "gpt-5".to_string(),
            label: "GPT-5".to_string(),
            context_window: Some(400_000),
            max_output_tokens: Some(128_000),
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
