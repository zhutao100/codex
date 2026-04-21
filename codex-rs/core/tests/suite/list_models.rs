use anyhow::Result;
use codex_core::CodexAuth;
use codex_core::ThreadManager;
use codex_core::built_in_model_providers;
use codex_core::models_manager::manager::RefreshStrategy;
use codex_protocol::openai_models::ModelPreset;
use core_test_support::load_default_config_for_test;
use pretty_assertions::assert_eq;
use std::collections::HashSet;
use tempfile::tempdir;

fn assert_has_single_default(models: &[ModelPreset]) {
    let default_count = models.iter().filter(|model| model.is_default).count();
    assert_eq!(default_count, 1, "expected exactly one default model");

    let Some(first_picker_model) = models.iter().find(|model| model.show_in_picker) else {
        panic!("expected at least one picker model");
    };
    assert!(
        first_picker_model.is_default,
        "expected the first picker model to be the default",
    );
}

fn assert_models_unique_by_id(models: &[ModelPreset]) {
    let ids: HashSet<&str> = models.iter().map(|model| model.id.as_str()).collect();
    assert_eq!(ids.len(), models.len(), "expected model ids to be unique");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_models_filters_api_key_models_to_api_supported() -> Result<()> {
    let codex_home = tempdir()?;
    let config = load_default_config_for_test(&codex_home).await;
    let manager = ThreadManager::with_models_provider(
        CodexAuth::from_api_key("sk-test"),
        built_in_model_providers()["openai"].clone(),
    );
    let models = manager
        .list_models(&config, RefreshStrategy::OnlineIfUncached)
        .await;

    assert!(!models.is_empty());
    assert!(models.iter().all(|model| model.supported_in_api));
    assert_models_unique_by_id(&models);
    assert_has_single_default(&models);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_models_in_chatgpt_mode_includes_api_key_models() -> Result<()> {
    let codex_home = tempdir()?;
    let config = load_default_config_for_test(&codex_home).await;

    let api_key_manager = ThreadManager::with_models_provider(
        CodexAuth::from_api_key("sk-test"),
        built_in_model_providers()["openai"].clone(),
    );
    let api_key_models = api_key_manager
        .list_models(&config, RefreshStrategy::OnlineIfUncached)
        .await;

    let chatgpt_manager = ThreadManager::with_models_provider(
        CodexAuth::create_dummy_chatgpt_auth_for_testing(),
        built_in_model_providers()["openai"].clone(),
    );
    let chatgpt_models = chatgpt_manager
        .list_models(&config, RefreshStrategy::OnlineIfUncached)
        .await;

    assert!(!chatgpt_models.is_empty());
    assert_models_unique_by_id(&chatgpt_models);
    assert_has_single_default(&chatgpt_models);

    let chatgpt_ids: HashSet<&str> = chatgpt_models
        .iter()
        .map(|model| model.id.as_str())
        .collect();
    for api_key_model in api_key_models {
        assert!(
            chatgpt_ids.contains(api_key_model.id.as_str()),
            "expected ChatGPT models to include {}",
            api_key_model.id
        );
    }

    Ok(())
}
