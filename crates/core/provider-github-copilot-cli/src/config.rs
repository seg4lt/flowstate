//! Copilot CLI catalog parsers + fallback model list.
//!
//! Extracted from `lib.rs` in the phase 3 god-file split.

use zenui_provider_api::ProviderModel;

#[derive(Debug, serde::Deserialize)]
pub(crate) struct SkillsList {
    #[serde(default)]
    pub(crate) skills: Vec<CliSkill>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CliSkill {
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) description: String,
    #[serde(default)]
    pub(crate) user_invocable: bool,
}

#[derive(Debug, serde::Deserialize)]
pub(crate) struct AgentsList {
    #[serde(default)]
    pub(crate) agents: Vec<CliAgent>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CliAgent {
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) description: String,
}

#[derive(Debug, serde::Deserialize)]
pub(crate) struct McpList {
    #[serde(default)]
    pub(crate) servers: Vec<CliMcpServer>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CliMcpServer {
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) status: Option<String>,
}

// ── Utilities ─────────────────────────────────────────────────────────────────

pub(crate) fn copilot_cli_models() -> Vec<ProviderModel> {
    // Fallback capability values for when the CLI's `listModels`
    // output doesn't carry them. Live responses beat these when
    // present (see the parser above).
    vec![
        ProviderModel {
            value: "gpt-4o".to_string(),
            label: "GPT-4o".to_string(),
            context_window: Some(128_000),
            max_output_tokens: Some(16_384),
            ..ProviderModel::default()
        },
        ProviderModel {
            value: "gpt-4.1".to_string(),
            label: "GPT-4.1".to_string(),
            context_window: Some(1_047_576),
            max_output_tokens: Some(32_768),
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
        ProviderModel {
            value: "claude-sonnet-4-6".to_string(),
            label: "Claude Sonnet 4.6".to_string(),
            context_window: Some(200_000),
            max_output_tokens: Some(64_000),
            ..ProviderModel::default()
        },
        ProviderModel {
            value: "o3".to_string(),
            label: "o3".to_string(),
            context_window: Some(200_000),
            max_output_tokens: Some(100_000),
            ..ProviderModel::default()
        },
        ProviderModel {
            value: "o4-mini".to_string(),
            label: "o4-mini".to_string(),
            context_window: Some(200_000),
            max_output_tokens: Some(100_000),
            ..ProviderModel::default()
        },
        ProviderModel {
            value: "gemini-2.5-pro".to_string(),
            label: "Gemini 2.5 Pro".to_string(),
            context_window: Some(1_048_576),
            max_output_tokens: Some(65_536),
            ..ProviderModel::default()
        },
    ]
}

/// Parse markdown bullet/numbered list into PlanStep items.
pub(crate) fn parse_plan_steps(raw: &str) -> Vec<zenui_provider_api::PlanStep> {
    raw.lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            let content = trimmed
                .strip_prefix("- ")
                .or_else(|| trimmed.strip_prefix("* "))
                .or_else(|| {
                    // numbered: "1. ", "12. ", etc.
                    let mut chars = trimmed.chars();
                    let digits: String = chars.by_ref().take_while(|c| c.is_ascii_digit()).collect();
                    if !digits.is_empty() && chars.next() == Some('.') {
                        Some(trimmed[digits.len() + 1..].trim())
                    } else {
                        None
                    }
                });
            content.map(|c| zenui_provider_api::PlanStep {
                title: c.to_string(),
                detail: None,
            })
        })
        .collect()
}
