use codex_protocol::config_types::Verbosity;
use codex_protocol::openai_models::ApplyPatchToolType;
use codex_protocol::openai_models::ConfigShellToolType;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::openai_models::ModelVisibility;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::openai_models::ReasoningEffortPreset;
use codex_protocol::openai_models::TruncationMode;
use codex_protocol::openai_models::TruncationPolicyConfig;
use codex_protocol::openai_models::default_input_modalities;

use crate::config::Config;
use crate::features::Feature;
use crate::truncate::approx_bytes_for_tokens;

pub const BASE_INSTRUCTIONS: &str = include_str!("../../prompt.md");
const OTHERS_INSTRUCTIONS: &str = include_str!("../../others_prompt.md");

pub(crate) const CONTEXT_WINDOW_164K: i64 = 163_840;

pub(crate) fn with_config_overrides(mut model: ModelInfo, config: &Config) -> ModelInfo {
    if let Some(supports_reasoning_summaries) = config.model_supports_reasoning_summaries {
        model.supports_reasoning_summaries = supports_reasoning_summaries;
    }
    if let Some(context_window) = config.model_context_window {
        model.context_window = Some(context_window);
    }
    if let Some(auto_compact_token_limit) = config.model_auto_compact_token_limit {
        model.auto_compact_token_limit = Some(auto_compact_token_limit);
    }
    if let Some(token_limit) = config.tool_output_token_limit {
        model.truncation_policy = match model.truncation_policy.mode {
            TruncationMode::Bytes => {
                let byte_limit =
                    i64::try_from(approx_bytes_for_tokens(token_limit)).unwrap_or(i64::MAX);
                TruncationPolicyConfig::bytes(byte_limit)
            }
            TruncationMode::Tokens => {
                let limit = i64::try_from(token_limit).unwrap_or(i64::MAX);
                TruncationPolicyConfig::tokens(limit)
            }
        };
    }

    if let Some(base_instructions) = &config.base_instructions {
        model.base_instructions = base_instructions.clone();
        model.model_messages = None;
    } else if !config.features.enabled(Feature::Personality) {
        model.model_messages = None;
    }

    model
}

/// Build a minimal fallback model descriptor for missing/unknown slugs.
pub(crate) fn model_info_from_slug(slug: &str) -> ModelInfo {
    ModelInfo {
        slug: slug.to_string(),
        display_name: slug.to_string(),
        description: None,
        default_reasoning_level: Some(ReasoningEffort::Medium),
        supported_reasoning_levels: supported_reasoning_level_low_medium_high(),
        shell_type: ConfigShellToolType::ShellCommand,
        visibility: ModelVisibility::None,
        supported_in_api: true,
        priority: 99,
        upgrade: None,
        base_instructions: OTHERS_INSTRUCTIONS.to_string(),
        model_messages: None,
        supports_reasoning_summaries: true,
        support_verbosity: true,
        default_verbosity: Some(Verbosity::Low),
        apply_patch_tool_type: Some(ApplyPatchToolType::Freeform),
        truncation_policy: TruncationPolicyConfig::bytes(10_000),
        supports_parallel_tool_calls: true,
        context_window: Some(CONTEXT_WINDOW_164K),
        auto_compact_token_limit: Some(CONTEXT_WINDOW_164K * 2 / 3),
        effective_context_window_percent: 95,
        experimental_supported_tools: Vec::new(),
        input_modalities: default_input_modalities(),
    }
}

fn supported_reasoning_level_low_medium_high() -> Vec<ReasoningEffortPreset> {
    vec![
        ReasoningEffortPreset {
            effort: ReasoningEffort::Low,
            description: "Fast responses with lighter reasoning".to_string(),
        },
        ReasoningEffortPreset {
            effort: ReasoningEffort::Medium,
            description: "Balances speed and reasoning depth for everyday tasks".to_string(),
        },
        ReasoningEffortPreset {
            effort: ReasoningEffort::High,
            description: "Greater reasoning depth for complex problems".to_string(),
        },
    ]
}
