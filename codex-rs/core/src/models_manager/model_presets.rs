use crate::auth::AuthMode;
use codex_protocol::openai_models::ModelPreset;
use codex_protocol::openai_models::ModelUpgrade;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::openai_models::ReasoningEffortPreset;
use codex_protocol::openai_models::default_input_modalities;
use indoc::indoc;
use once_cell::sync::Lazy;
use std::collections::HashMap;

pub const HIDE_GPT5_1_MIGRATION_PROMPT_CONFIG: &str = "hide_gpt5_1_migration_prompt";
pub const HIDE_GPT_5_1_CODEX_MAX_MIGRATION_PROMPT_CONFIG: &str =
    "hide_gpt-5.1-codex-max_migration_prompt";

static PRESETS: Lazy<Vec<ModelPreset>> = Lazy::new(|| {
    vec![
        ModelPreset {
            id: "gpt-5.4".to_string(),
            model: "gpt-5.4".to_string(),
            display_name: "gpt-5.4".to_string(),
            description: "Latest frontier agentic coding model.".to_string(),
            default_reasoning_effort: ReasoningEffort::Medium,
            supported_reasoning_efforts: vec![
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
                ReasoningEffortPreset {
                    effort: ReasoningEffort::XHigh,
                    description: "Extra high reasoning depth for complex problems".to_string(),
                },
            ],
            supports_personality: true,
            is_default: true,
            upgrade: None,
            show_in_picker: true,
            supported_in_api: true,
            input_modalities: default_input_modalities(),
        },
        ModelPreset {
            id: "gpt-5.4-mini".to_string(),
            model: "gpt-5.4-mini".to_string(),
            display_name: "GPT-5.4-Mini".to_string(),
            description: "Smaller frontier agentic coding model.".to_string(),
            default_reasoning_effort: ReasoningEffort::Medium,
            supported_reasoning_efforts: vec![
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
                ReasoningEffortPreset {
                    effort: ReasoningEffort::XHigh,
                    description: "Extra high reasoning depth for complex problems".to_string(),
                },
            ],
            supports_personality: true,
            is_default: false,
            upgrade: None,
            show_in_picker: true,
            supported_in_api: true,
            input_modalities: default_input_modalities(),
        },
        ModelPreset {
            id: "gpt-5.3-codex".to_string(),
            model: "gpt-5.3-codex".to_string(),
            display_name: "gpt-5.3-codex".to_string(),
            description: "Frontier Codex-optimized agentic coding model.".to_string(),
            default_reasoning_effort: ReasoningEffort::Medium,
            supported_reasoning_efforts: vec![
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
                ReasoningEffortPreset {
                    effort: ReasoningEffort::XHigh,
                    description: "Extra high reasoning depth for complex problems".to_string(),
                },
            ],
            supports_personality: true,
            is_default: false,
            upgrade: None,
            show_in_picker: true,
            supported_in_api: true,
            input_modalities: default_input_modalities(),
        },
        ModelPreset {
            id: "gpt-5.3-codex-spark".to_string(),
            model: "gpt-5.3-codex-spark".to_string(),
            display_name: "GPT-5.3-Codex-Spark".to_string(),
            description: "Ultra-fast coding model.".to_string(),
            default_reasoning_effort: ReasoningEffort::High,
            supported_reasoning_efforts: vec![
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
                ReasoningEffortPreset {
                    effort: ReasoningEffort::XHigh,
                    description: "Extra high reasoning depth for complex problems".to_string(),
                },
            ],
            supports_personality: true,
            is_default: false,
            upgrade: None,
            show_in_picker: true,
            supported_in_api: false,
            input_modalities: default_input_modalities(),
        },
        ModelPreset {
            id: "gpt-5.2".to_string(),
            model: "gpt-5.2".to_string(),
            display_name: "gpt-5.2".to_string(),
            description: "Optimized for professional work and long-running agents".to_string(),
            default_reasoning_effort: ReasoningEffort::Medium,
            supported_reasoning_efforts: vec![
                ReasoningEffortPreset {
                    effort: ReasoningEffort::Low,
                    description: "Balances speed with some reasoning; useful for straightforward queries and short explanations".to_string(),
                },
                ReasoningEffortPreset {
                    effort: ReasoningEffort::Medium,
                    description: "Provides a solid balance of reasoning depth and latency for general-purpose tasks".to_string(),
                },
                ReasoningEffortPreset {
                    effort: ReasoningEffort::High,
                    description: "Maximizes reasoning depth for complex or ambiguous problems".to_string(),
                },
                ReasoningEffortPreset {
                    effort: ReasoningEffort::XHigh,
                    description: "Extra high reasoning for complex problems".to_string(),
                },
            ],
            supports_personality: false,
            is_default: false,
            upgrade: None,
            show_in_picker: true,
            supported_in_api: true,
            input_modalities: default_input_modalities(),
        },
        ModelPreset {
            id: "codex-auto-review".to_string(),
            model: "codex-auto-review".to_string(),
            display_name: "Codex Auto Review".to_string(),
            description: "Automatic approval review model for Codex.".to_string(),
            default_reasoning_effort: ReasoningEffort::Medium,
            supported_reasoning_efforts: vec![
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
                ReasoningEffortPreset {
                    effort: ReasoningEffort::XHigh,
                    description: "Extra high reasoning depth for complex problems".to_string(),
                },
            ],
            supports_personality: true,
            is_default: false,
            upgrade: None,
            show_in_picker: false,
            supported_in_api: true,
            input_modalities: default_input_modalities(),
        },
        ModelPreset {
            id: "bengalfox".to_string(),
            model: "bengalfox".to_string(),
            display_name: "bengalfox".to_string(),
            description: "bengalfox".to_string(),
            default_reasoning_effort: ReasoningEffort::Medium,
            supported_reasoning_efforts: vec![
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
                ReasoningEffortPreset {
                    effort: ReasoningEffort::XHigh,
                    description: "Extra high reasoning depth for complex problems".to_string(),
                },
            ],
            supports_personality: true,
            is_default: false,
            upgrade: None,
            show_in_picker: false,
            supported_in_api: true,
            input_modalities: default_input_modalities(),
        },
        ModelPreset {
            id: "boomslang".to_string(),
            model: "boomslang".to_string(),
            display_name: "boomslang".to_string(),
            description: "boomslang".to_string(),
            default_reasoning_effort: ReasoningEffort::Medium,
            supported_reasoning_efforts: vec![
                ReasoningEffortPreset {
                    effort: ReasoningEffort::Low,
                    description: "Balances speed with some reasoning; useful for straightforward queries and short explanations".to_string(),
                },
                ReasoningEffortPreset {
                    effort: ReasoningEffort::Medium,
                    description: "Provides a solid balance of reasoning depth and latency for general-purpose tasks".to_string(),
                },
                ReasoningEffortPreset {
                    effort: ReasoningEffort::High,
                    description: "Maximizes reasoning depth for complex or ambiguous problems".to_string(),
                },
                ReasoningEffortPreset {
                    effort: ReasoningEffort::XHigh,
                    description: "Extra high reasoning depth for complex problems".to_string(),
                },
            ],
            supports_personality: false,
            is_default: false,
            upgrade: None,
            show_in_picker: false,
            supported_in_api: true,
            input_modalities: default_input_modalities(),
        },
        // Deprecated models (removed from server).
	        ModelPreset {
	            id: "gpt-5.2-codex".to_string(),
	            model: "gpt-5.2-codex".to_string(),
	            display_name: "gpt-5.2-codex".to_string(),
            description: "Latest frontier agentic coding model.".to_string(),
            default_reasoning_effort: ReasoningEffort::Medium,
            supported_reasoning_efforts: vec![
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
                ReasoningEffortPreset {
                    effort: ReasoningEffort::XHigh,
                    description: "Extra high reasoning depth for complex problems".to_string(),
                },
            ],
	            supports_personality: true,
	            is_default: false,
	            upgrade: Some(gpt_54_upgrade(
	                "gpt-5.4",
	                HIDE_GPT5_1_MIGRATION_PROMPT_CONFIG,
	            )),
	            show_in_picker: false,
	            supported_in_api: false,
	            input_modalities: default_input_modalities(),
	        },
	        ModelPreset {
            id: "gpt-5.1-codex-max".to_string(),
            model: "gpt-5.1-codex-max".to_string(),
            display_name: "gpt-5.1-codex-max".to_string(),
            description: "Codex-optimized flagship for deep and fast reasoning.".to_string(),
            default_reasoning_effort: ReasoningEffort::Medium,
            supported_reasoning_efforts: vec![
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
                ReasoningEffortPreset {
                    effort: ReasoningEffort::XHigh,
                    description: "Extra high reasoning depth for complex problems".to_string(),
                },
            ],
	            supports_personality: false,
	            is_default: false,
	            upgrade: Some(gpt_54_upgrade(
	                "gpt-5.4",
	                HIDE_GPT_5_1_CODEX_MAX_MIGRATION_PROMPT_CONFIG,
	            )),
	            show_in_picker: false,
	            supported_in_api: false,
	            input_modalities: default_input_modalities(),
	        },
	        ModelPreset {
            id: "gpt-5.1-codex-mini".to_string(),
            model: "gpt-5.1-codex-mini".to_string(),
            display_name: "gpt-5.1-codex-mini".to_string(),
            description: "Optimized for codex. Cheaper, faster, but less capable.".to_string(),
            default_reasoning_effort: ReasoningEffort::Medium,
            supported_reasoning_efforts: vec![
                ReasoningEffortPreset {
                    effort: ReasoningEffort::Medium,
                    description: "Dynamically adjusts reasoning based on the task".to_string(),
                },
                ReasoningEffortPreset {
                    effort: ReasoningEffort::High,
                    description: "Maximizes reasoning depth for complex or ambiguous problems"
                        .to_string(),
                },
            ],
	            supports_personality: false,
	            is_default: false,
	            upgrade: Some(gpt_54_upgrade(
	                "gpt-5.4-mini",
	                HIDE_GPT5_1_MIGRATION_PROMPT_CONFIG,
	            )),
	            show_in_picker: false,
	            supported_in_api: false,
	            input_modalities: default_input_modalities(),
	        },
	        ModelPreset {
            id: "gpt-5.1-codex".to_string(),
            model: "gpt-5.1-codex".to_string(),
            display_name: "gpt-5.1-codex".to_string(),
            description: "Optimized for codex.".to_string(),
            default_reasoning_effort: ReasoningEffort::Medium,
            supported_reasoning_efforts: vec![
                ReasoningEffortPreset {
                    effort: ReasoningEffort::Low,
                    description: "Fastest responses with limited reasoning".to_string(),
                },
                ReasoningEffortPreset {
                    effort: ReasoningEffort::Medium,
                    description: "Dynamically adjusts reasoning based on the task".to_string(),
                },
                ReasoningEffortPreset {
                    effort: ReasoningEffort::High,
                    description: "Maximizes reasoning depth for complex or ambiguous problems"
                        .to_string(),
                },
            ],
	            supports_personality: false,
	            is_default: false,
	            upgrade: Some(gpt_54_upgrade(
	                "gpt-5.4",
	                HIDE_GPT5_1_MIGRATION_PROMPT_CONFIG,
	            )),
	            show_in_picker: false,
	            supported_in_api: false,
	            input_modalities: default_input_modalities(),
	        },
        ModelPreset {
            id: "gpt-5".to_string(),
            model: "gpt-5".to_string(),
            display_name: "gpt-5".to_string(),
            description: "Broad world knowledge with strong general reasoning.".to_string(),
            default_reasoning_effort: ReasoningEffort::Medium,
            supported_reasoning_efforts: vec![
                ReasoningEffortPreset {
                    effort: ReasoningEffort::Minimal,
                    description: "Fastest responses with little reasoning".to_string(),
                },
                ReasoningEffortPreset {
                    effort: ReasoningEffort::Low,
                    description: "Balances speed with some reasoning; useful for straightforward queries and short explanations".to_string(),
                },
                ReasoningEffortPreset {
                    effort: ReasoningEffort::Medium,
                    description: "Provides a solid balance of reasoning depth and latency for general-purpose tasks".to_string(),
                },
                ReasoningEffortPreset {
                    effort: ReasoningEffort::High,
                    description: "Maximizes reasoning depth for complex or ambiguous problems".to_string(),
                },
            ],
	            supports_personality: false,
	            is_default: false,
	            upgrade: Some(gpt_54_upgrade(
	                "gpt-5.4",
	                HIDE_GPT5_1_MIGRATION_PROMPT_CONFIG,
	            )),
	            show_in_picker: false,
	            supported_in_api: false,
	            input_modalities: default_input_modalities(),
	        },
        ModelPreset {
            id: "gpt-5.1".to_string(),
            model: "gpt-5.1".to_string(),
            display_name: "gpt-5.1".to_string(),
            description: "Broad world knowledge with strong general reasoning.".to_string(),
            default_reasoning_effort: ReasoningEffort::Medium,
            supported_reasoning_efforts: vec![
                ReasoningEffortPreset {
                    effort: ReasoningEffort::Low,
                    description: "Balances speed with some reasoning; useful for straightforward queries and short explanations".to_string(),
                },
                ReasoningEffortPreset {
                    effort: ReasoningEffort::Medium,
                    description: "Provides a solid balance of reasoning depth and latency for general-purpose tasks".to_string(),
                },
                ReasoningEffortPreset {
                    effort: ReasoningEffort::High,
                    description: "Maximizes reasoning depth for complex or ambiguous problems".to_string(),
                },
            ],
	            supports_personality: false,
	            is_default: false,
	            upgrade: Some(gpt_54_upgrade(
	                "gpt-5.4",
	                HIDE_GPT5_1_MIGRATION_PROMPT_CONFIG,
	            )),
	            show_in_picker: false,
	            supported_in_api: false,
	            input_modalities: default_input_modalities(),
	        },
        ModelPreset {
            id: "gpt-5-codex".to_string(),
            model: "gpt-5-codex".to_string(),
            display_name: "gpt-5-codex".to_string(),
            description: "Optimized for codex.".to_string(),
            default_reasoning_effort: ReasoningEffort::Medium,
            supported_reasoning_efforts: vec![
                ReasoningEffortPreset {
                    effort: ReasoningEffort::Low,
                    description: "Fastest responses with limited reasoning".to_string(),
                },
                ReasoningEffortPreset {
                    effort: ReasoningEffort::Medium,
                    description: "Dynamically adjusts reasoning based on the task".to_string(),
                },
                ReasoningEffortPreset {
                    effort: ReasoningEffort::High,
                    description: "Maximizes reasoning depth for complex or ambiguous problems".to_string(),
                },
            ],
	            supports_personality: false,
	            is_default: false,
	            upgrade: Some(gpt_54_upgrade(
	                "gpt-5.4",
	                HIDE_GPT5_1_MIGRATION_PROMPT_CONFIG,
	            )),
	            show_in_picker: false,
	            supported_in_api: false,
	            input_modalities: default_input_modalities(),
	        },
        ModelPreset {
            id: "gpt-5-codex-mini".to_string(),
            model: "gpt-5-codex-mini".to_string(),
            display_name: "gpt-5-codex-mini".to_string(),
            description: "Optimized for codex. Cheaper, faster, but less capable.".to_string(),
            default_reasoning_effort: ReasoningEffort::Medium,
            supported_reasoning_efforts: vec![
                ReasoningEffortPreset {
                    effort: ReasoningEffort::Medium,
                    description: "Dynamically adjusts reasoning based on the task".to_string(),
                },
                ReasoningEffortPreset {
                    effort: ReasoningEffort::High,
                    description: "Maximizes reasoning depth for complex or ambiguous problems".to_string(),
                },
            ],
	            supports_personality: false,
	            is_default: false,
	            upgrade: Some(gpt_54_upgrade(
	                "gpt-5.4-mini",
	                HIDE_GPT5_1_MIGRATION_PROMPT_CONFIG,
	            )),
	            show_in_picker: false,
	            supported_in_api: false,
	            input_modalities: default_input_modalities(),
	        },
    ]
});

fn gpt_54_upgrade(target_model: &str, migration_config_key: &str) -> ModelUpgrade {
    ModelUpgrade {
        id: target_model.to_string(),
        reasoning_effort_mapping: Some(HashMap::from([
            (ReasoningEffort::None, ReasoningEffort::Low),
            (ReasoningEffort::Minimal, ReasoningEffort::Low),
            (ReasoningEffort::Low, ReasoningEffort::Low),
            (ReasoningEffort::Medium, ReasoningEffort::Medium),
            (ReasoningEffort::High, ReasoningEffort::High),
            (ReasoningEffort::XHigh, ReasoningEffort::XHigh),
        ])),
        migration_config_key: migration_config_key.to_string(),
        model_link: None,
        upgrade_copy: Some(format!(
            "Codex is now powered by {target_model}, our latest frontier agentic coding model."
        )),
        migration_markdown: Some(
            indoc! {r#"
	                **Codex just got an upgrade. Introducing {model_to}.**

	                Codex is now powered by {model_to}, our latest frontier agentic coding model.

	                You can continue using {model_from} if you prefer.
	            "#}
            .to_string(),
        ),
    }
}

pub(super) fn builtin_model_presets(_auth_mode: Option<AuthMode>) -> Vec<ModelPreset> {
    PRESETS.iter().cloned().collect()
}

#[cfg(any(test, feature = "test-support"))]
pub fn all_model_presets() -> &'static Vec<ModelPreset> {
    &PRESETS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_one_default_model_is_configured() {
        let default_models = PRESETS.iter().filter(|preset| preset.is_default).count();
        assert!(default_models == 1);
    }
}
