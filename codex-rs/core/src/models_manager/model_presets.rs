use crate::auth::AuthMode;
use codex_protocol::openai_models::ModelPreset;
use codex_protocol::openai_models::ModelsResponse;
use once_cell::sync::Lazy;

pub const HIDE_GPT5_1_MIGRATION_PROMPT_CONFIG: &str = "hide_gpt5_1_migration_prompt";
pub const HIDE_GPT_5_1_CODEX_MAX_MIGRATION_PROMPT_CONFIG: &str =
    "hide_gpt-5.1-codex-max_migration_prompt";

static PRESETS: Lazy<Vec<ModelPreset>> = Lazy::new(|| {
    let mut response: ModelsResponse = serde_json::from_str(include_str!("../../models.json"))
        .expect("bundled models.json should deserialize");
    response.models.sort_by_key(|model| model.priority);
    let mut presets: Vec<ModelPreset> = response.models.into_iter().map(Into::into).collect();
    if let Some(default) = presets.iter_mut().find(|preset| preset.show_in_picker) {
        default.is_default = true;
    }
    presets
});

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
    use pretty_assertions::assert_eq;

    #[test]
    fn only_one_default_model_is_configured() {
        let default_models: Vec<_> = PRESETS
            .iter()
            .filter(|preset| preset.is_default)
            .map(|preset| preset.model.as_str())
            .collect();
        assert_eq!(default_models, vec!["gpt-6.1-sol"]);
    }

    #[test]
    fn presets_preserve_bundled_model_metadata() {
        let response: ModelsResponse = serde_json::from_str(include_str!("../../models.json"))
            .expect("bundled models.json should deserialize");
        let mut expected: Vec<ModelPreset> = response.models.into_iter().map(Into::into).collect();
        expected
            .iter_mut()
            .find(|preset| preset.model == "gpt-6.1-sol")
            .expect("default model should be bundled")
            .is_default = true;
        assert_eq!(*PRESETS, expected);
    }

    #[test]
    fn removed_server_models_are_not_configured() {
        let removed_models = [
            "gpt-5.4",
            "gpt-5.4-mini",
            "gpt-5.3-codex",
            "gpt-5.3-codex-spark",
            "gpt-5.2",
            "gpt-5.2-codex",
            "gpt-5.1-codex-max",
            "gpt-5.1-codex",
            "gpt-5.1-codex-mini",
            "gpt-5.1",
            "gpt-5-codex",
            "gpt-5-codex-mini",
            "gpt-5",
            "bengalfox",
            "boomslang",
        ];

        for model in removed_models {
            assert!(
                PRESETS.iter().all(|preset| preset.model != model),
                "{model} should not be configured as a local preset"
            );
        }
    }
}
