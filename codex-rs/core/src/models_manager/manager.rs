use super::cache::ModelsCacheManager;
use crate::api_bridge::map_api_error;
use crate::api_bridge::resolve_request_auth;
use crate::auth::AuthManager;
use crate::auth::AuthMode;
use crate::config::Config;
use crate::default_client::build_reqwest_client;
use crate::error::CodexErr;
use crate::error::Result as CoreResult;
use crate::features::Feature;
use crate::model_provider_info::ModelProviderInfo;
use crate::models_manager::collaboration_mode_presets::builtin_collaboration_mode_presets;
use crate::models_manager::model_info;
use crate::models_manager::model_presets::builtin_model_presets;
use codex_api::ModelsClient;
use codex_api::ReqwestTransport;
use codex_protocol::config_types::CollaborationModeMask;
use codex_protocol::config_types::Personality;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::openai_models::ModelPreset;
use codex_protocol::openai_models::ModelsResponse;
use http::HeaderMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tokio::sync::TryLockError;
use tokio::time::timeout;
use tracing::error;
use tracing::warn;

const MODEL_CACHE_FILE: &str = "models_cache.json";
const DEFAULT_MODEL_CACHE_TTL: Duration = Duration::from_secs(300);
const MODELS_REFRESH_TIMEOUT: Duration = Duration::from_secs(5);

/// Strategy for refreshing available models.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshStrategy {
    /// Always fetch from the network, ignoring cache.
    Online,
    /// Only use cached data, never fetch from the network.
    Offline,
    /// Use cache if available and fresh, otherwise fetch from the network.
    OnlineIfUncached,
}

/// Coordinates remote model discovery plus cached metadata on disk.
#[derive(Debug)]
pub struct ModelsManager {
    local_models: Vec<ModelPreset>,
    remote_models: RwLock<Vec<ModelInfo>>,
    auth_manager: Arc<AuthManager>,
    etag: RwLock<Option<String>>,
    cache_manager: ModelsCacheManager,
    provider: ModelProviderInfo,
}

impl ModelsManager {
    /// Construct a manager scoped to the provided `AuthManager`.
    ///
    /// Uses `codex_home` to store cached model metadata and initializes with built-in presets.
    pub fn new(codex_home: PathBuf, auth_manager: Arc<AuthManager>) -> Self {
        let cache_path = codex_home.join(MODEL_CACHE_FILE);
        let cache_manager = ModelsCacheManager::new(cache_path, DEFAULT_MODEL_CACHE_TTL);
        Self {
            local_models: builtin_model_presets(auth_manager.auth_mode()),
            remote_models: RwLock::new(Self::load_remote_models_from_file().unwrap_or_default()),
            auth_manager,
            etag: RwLock::new(None),
            cache_manager,
            provider: ModelProviderInfo::create_openai_provider(),
        }
    }

    /// List all available models, refreshing according to the specified strategy.
    ///
    /// Returns model presets sorted by priority and filtered by auth mode and visibility.
    pub async fn list_models(
        &self,
        config: &Config,
        refresh_strategy: RefreshStrategy,
    ) -> Vec<ModelPreset> {
        if let Err(err) = self
            .refresh_available_models(config, refresh_strategy)
            .await
        {
            error!("failed to refresh available models: {err}");
        }
        let remote_models = self.get_remote_models_for_picker(config).await;
        self.build_available_models(Self::overlaid_candidates_for_picker(remote_models, config))
    }

    /// List collaboration mode presets.
    ///
    /// Returns a static set of presets seeded with the configured model.
    pub fn list_collaboration_modes(&self) -> Vec<CollaborationModeMask> {
        builtin_collaboration_mode_presets()
    }

    /// Attempt to list models without blocking, using the current cached state.
    ///
    /// Returns an error if the internal lock cannot be acquired.
    pub fn try_list_models(&self, config: &Config) -> Result<Vec<ModelPreset>, TryLockError> {
        let remote_models = self.try_get_remote_models_for_picker(config)?;
        Ok(
            self.build_available_models(Self::overlaid_candidates_for_picker(
                remote_models,
                config,
            )),
        )
    }

    // todo(aibrahim): should be visible to core only and sent on session_configured event
    /// Get the model identifier to use, refreshing according to the specified strategy.
    ///
    /// If `model` is provided, returns it directly. Otherwise selects the default based on
    /// auth mode and available models.
    pub async fn get_default_model(
        &self,
        model: &Option<String>,
        config: &Config,
        refresh_strategy: RefreshStrategy,
    ) -> String {
        if let Some(model) = model.as_ref() {
            return model.to_string();
        }
        if let Err(err) = self
            .refresh_available_models(config, refresh_strategy)
            .await
        {
            error!("failed to refresh available models: {err}");
        }
        let remote_models = self.get_remote_models_for_picker(config).await;
        let available = self
            .build_available_models(Self::overlaid_candidates_for_picker(remote_models, config));
        available
            .iter()
            .find(|model| model.is_default)
            .or_else(|| available.first())
            .map(|model| model.model.clone())
            .unwrap_or_default()
    }

    // todo(aibrahim): look if we can tighten it to pub(crate)
    /// Look up model metadata, applying remote overrides and config adjustments.
    pub async fn get_model_info(&self, model: &str, config: &Config) -> ModelInfo {
        let remote_models = self.remote_models.read().await;
        if let Some(model_info) =
            Self::find_model_info_from_candidates(model, remote_models.as_slice(), config)
        {
            return model_info;
        }
        drop(remote_models);

        if config.features.enabled(Feature::RemoteModels)
            && let Some(model_info) = self.get_model_info_from_cache(model, config).await
        {
            return model_info;
        }

        Self::construct_fallback_model_info(model, config)
    }

    pub async fn get_final_instruction_override(
        &self,
        model: &str,
        config: &Config,
    ) -> Option<String> {
        let overlay = config.model_overlay.as_ref()?;
        let remote_models = self.remote_models.read().await;
        let mut candidates = remote_models.clone();
        for entry in &overlay.models {
            if !candidates
                .iter()
                .any(|candidate| candidate.slug == entry.slug)
            {
                candidates.push(model_info::model_info_from_slug(&entry.slug));
            }
        }
        let matched_slug = Self::find_model_match(model, &candidates)
            .map(|candidate| candidate.slug)
            .unwrap_or_else(|| model.to_string());
        overlay.final_instruction_override_for_slug(&matched_slug)
    }

    pub(crate) fn effective_model_instructions(
        model_info: &ModelInfo,
        personality: Option<Personality>,
        final_instruction_override: Option<&str>,
    ) -> String {
        final_instruction_override
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| model_info.get_model_instructions(personality))
    }

    fn find_model_by_longest_prefix(model: &str, candidates: &[ModelInfo]) -> Option<ModelInfo> {
        let mut best: Option<ModelInfo> = None;
        for candidate in candidates {
            if !model.starts_with(&candidate.slug) {
                continue;
            }
            let is_better_match = if let Some(current) = best.as_ref() {
                candidate.slug.len() > current.slug.len()
            } else {
                true
            };
            if is_better_match {
                best = Some(candidate.clone());
            }
        }
        best
    }

    /// Retry metadata lookup for a single namespaced slug like `namespace/model-name`.
    ///
    /// This only strips one leading namespace segment and only when the namespace is ASCII
    /// alphanumeric/underscore (`\\w+`) to avoid broadly matching arbitrary aliases.
    fn find_model_by_namespaced_suffix(model: &str, candidates: &[ModelInfo]) -> Option<ModelInfo> {
        let (namespace, suffix) = model.split_once('/')?;
        if suffix.contains('/') {
            return None;
        }
        if !namespace
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            return None;
        }
        Self::find_model_by_longest_prefix(suffix, candidates)
    }

    fn construct_model_info_from_candidates(
        model: &str,
        candidates: &[ModelInfo],
        config: &Config,
    ) -> ModelInfo {
        Self::find_model_info_from_candidates(model, candidates, config)
            .unwrap_or_else(|| Self::construct_fallback_model_info(model, config))
    }

    fn find_model_info_from_candidates(
        model: &str,
        candidates: &[ModelInfo],
        config: &Config,
    ) -> Option<ModelInfo> {
        // First use the normal longest-prefix match. If that misses, allow a narrowly scoped
        // retry for namespaced slugs like `custom/gpt-5.3-codex`.
        let overlaid_candidates;
        let candidates = if let Some(overlay) = config.model_overlay.as_ref() {
            overlaid_candidates = overlay.apply_to_candidates(candidates);
            overlaid_candidates.as_slice()
        } else {
            candidates
        };
        let remote = Self::find_model_match(model, candidates);
        remote.map(|remote| {
            let model_info = ModelInfo {
                slug: model.to_string(),
                ..remote
            };
            model_info::with_config_overrides(model_info, config)
        })
    }

    fn construct_fallback_model_info(model: &str, config: &Config) -> ModelInfo {
        let mut model_info = model_info::model_info_from_slug(model);
        if let Some(overlay) = config.model_overlay.as_ref() {
            overlay.apply_to_fallback_model(&mut model_info);
        }
        model_info::with_config_overrides(model_info, config)
    }

    async fn get_model_info_from_cache(&self, model: &str, config: &Config) -> Option<ModelInfo> {
        let client_version = crate::models_manager::client_version_to_whole();
        let cache = self.cache_manager.load_compatible(&client_version).await?;
        Self::find_model_info_from_candidates(model, &cache.models, config)
    }

    fn find_model_match(model: &str, candidates: &[ModelInfo]) -> Option<ModelInfo> {
        Self::find_model_by_longest_prefix(model, candidates)
            .or_else(|| Self::find_model_by_namespaced_suffix(model, candidates))
    }

    /// Refresh models if the provided ETag differs from the cached ETag.
    ///
    /// Uses `Online` strategy to fetch latest models when ETags differ.
    pub(crate) async fn refresh_if_new_etag(&self, etag: String, config: &Config) {
        let current_etag = self.get_etag().await;
        if current_etag.clone().is_some() && current_etag.as_deref() == Some(etag.as_str()) {
            if let Err(err) = self.cache_manager.renew_cache_ttl().await {
                error!("failed to renew cache TTL: {err}");
            }
            return;
        }
        if let Err(err) = self
            .refresh_available_models(config, RefreshStrategy::Online)
            .await
        {
            error!("failed to refresh available models: {err}");
        }
    }

    /// Refresh available models according to the specified strategy.
    async fn refresh_available_models(
        &self,
        config: &Config,
        refresh_strategy: RefreshStrategy,
    ) -> CoreResult<()> {
        if !config.features.enabled(Feature::RemoteModels) {
            return Ok(());
        }
        if self.auth_manager.auth_mode() == Some(AuthMode::ApiKey) {
            if matches!(
                refresh_strategy,
                RefreshStrategy::Offline | RefreshStrategy::OnlineIfUncached
            ) {
                self.try_load_cache(true).await;
            }
            return Ok(());
        }

        match refresh_strategy {
            RefreshStrategy::Offline => {
                // Only try to load from cache, never fetch.
                self.try_load_cache(true).await;
                Ok(())
            }
            RefreshStrategy::OnlineIfUncached => {
                // Try a fresh cache first, fall back to online, then stale cache if offline.
                if self.try_load_cache(false).await {
                    return Ok(());
                }
                match self.fetch_and_update_models().await {
                    Ok(()) => Ok(()),
                    Err(err) => {
                        if self.try_load_cache(true).await {
                            warn!("failed to fetch models; using stale models cache: {err}");
                            Ok(())
                        } else {
                            Err(err)
                        }
                    }
                }
            }
            RefreshStrategy::Online => {
                // Always fetch from network.
                self.fetch_and_update_models().await
            }
        }
    }

    async fn fetch_and_update_models(&self) -> CoreResult<()> {
        let _timer =
            codex_otel::start_global_timer("codex.remote_models.fetch_update.duration_ms", &[]);
        let auth = self.auth_manager.auth().await;
        let request_auth = resolve_request_auth(auth, &self.provider)?;
        let api_provider = self.provider.to_api_provider(request_auth.auth_mode)?;
        let api_auth = request_auth.provider;
        let transport = ReqwestTransport::new(build_reqwest_client());
        let client = ModelsClient::new(transport, api_provider, api_auth);

        let client_version = crate::models_manager::client_version_to_whole();
        let (models, etag) = timeout(
            MODELS_REFRESH_TIMEOUT,
            client.list_models(&client_version, HeaderMap::new()),
        )
        .await
        .map_err(|_| CodexErr::Timeout)?
        .map_err(map_api_error)?;

        self.apply_remote_models(models.clone()).await;
        *self.etag.write().await = etag.clone();
        self.cache_manager
            .persist_cache(&models, etag, client_version)
            .await;
        Ok(())
    }

    async fn get_etag(&self) -> Option<String> {
        self.etag.read().await.clone()
    }

    /// Replace the cached remote models and rebuild the derived presets list.
    async fn apply_remote_models(&self, models: Vec<ModelInfo>) {
        let mut existing_models = Self::load_remote_models_from_file().unwrap_or_default();
        for model in models {
            if let Some(existing_index) = existing_models
                .iter()
                .position(|existing| existing.slug == model.slug)
            {
                existing_models[existing_index] = model;
            } else {
                existing_models.push(model);
            }
        }
        *self.remote_models.write().await = existing_models;
    }

    fn load_remote_models_from_file() -> Result<Vec<ModelInfo>, std::io::Error> {
        let file_contents = include_str!("../../models.json");
        let response: ModelsResponse = serde_json::from_str(file_contents)?;
        Ok(response.models)
    }

    /// Attempt to satisfy the refresh from the cache when it matches the provider.
    async fn try_load_cache(&self, allow_stale: bool) -> bool {
        let _timer =
            codex_otel::start_global_timer("codex.remote_models.load_cache.duration_ms", &[]);
        let client_version = crate::models_manager::client_version_to_whole();
        let cache = if allow_stale {
            self.cache_manager.load_compatible(&client_version).await
        } else {
            self.cache_manager.load_fresh(&client_version).await
        };
        let cache = match cache {
            Some(cache) => cache,
            None => return false,
        };
        let models = cache.models.clone();
        *self.etag.write().await = cache.etag.clone();
        self.apply_remote_models(models.clone()).await;
        true
    }

    /// Merge remote model metadata into picker-ready presets, preserving existing entries.
    fn build_available_models(&self, mut remote_models: Vec<ModelInfo>) -> Vec<ModelPreset> {
        remote_models.sort_by(|a, b| a.priority.cmp(&b.priority));

        let remote_presets: Vec<ModelPreset> = remote_models.into_iter().map(Into::into).collect();
        let existing_presets = self.local_models.clone();
        let mut merged_presets = ModelPreset::merge(remote_presets, existing_presets);
        let chatgpt_mode = matches!(self.auth_manager.auth_mode(), Some(AuthMode::Chatgpt));
        merged_presets = ModelPreset::filter_by_auth(merged_presets, chatgpt_mode);

        for preset in &mut merged_presets {
            preset.is_default = false;
        }
        if let Some(default) = merged_presets
            .iter_mut()
            .find(|preset| preset.show_in_picker)
        {
            default.is_default = true;
        } else if let Some(default) = merged_presets.first_mut() {
            default.is_default = true;
        }

        merged_presets
    }

    #[cfg(test)]
    async fn get_remote_models(&self, config: &Config) -> Vec<ModelInfo> {
        if config.features.enabled(Feature::RemoteModels) {
            self.remote_models.read().await.clone()
        } else {
            Vec::new()
        }
    }

    async fn get_remote_models_for_picker(&self, config: &Config) -> Vec<ModelInfo> {
        if config.features.enabled(Feature::RemoteModels) || config.model_overlay.is_some() {
            self.remote_models.read().await.clone()
        } else {
            Vec::new()
        }
    }

    fn try_get_remote_models_for_picker(
        &self,
        config: &Config,
    ) -> Result<Vec<ModelInfo>, TryLockError> {
        if config.features.enabled(Feature::RemoteModels) || config.model_overlay.is_some() {
            Ok(self.remote_models.try_read()?.clone())
        } else {
            Ok(Vec::new())
        }
    }

    fn overlaid_candidates_for_picker(
        remote_models: Vec<ModelInfo>,
        config: &Config,
    ) -> Vec<ModelInfo> {
        match config.model_overlay.as_ref() {
            Some(overlay) if config.features.enabled(Feature::RemoteModels) => {
                overlay.apply_to_candidates(&remote_models)
            }
            Some(overlay) => overlay.apply_mentioned_to_candidates(&remote_models),
            None => remote_models,
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    /// Construct a manager with a specific provider for testing.
    pub fn with_provider(
        codex_home: PathBuf,
        auth_manager: Arc<AuthManager>,
        provider: ModelProviderInfo,
    ) -> Self {
        let cache_path = codex_home.join(MODEL_CACHE_FILE);
        let cache_manager = ModelsCacheManager::new(cache_path, DEFAULT_MODEL_CACHE_TTL);
        Self {
            local_models: builtin_model_presets(auth_manager.auth_mode()),
            remote_models: RwLock::new(Self::load_remote_models_from_file().unwrap_or_default()),
            auth_manager,
            etag: RwLock::new(None),
            cache_manager,
            provider,
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    /// Get model identifier without consulting remote state or cache.
    pub fn get_model_offline(model: Option<&str>) -> String {
        if let Some(model) = model {
            return model.to_string();
        }
        let presets = builtin_model_presets(None);
        presets
            .iter()
            .find(|preset| preset.show_in_picker)
            .or_else(|| presets.first())
            .map(|preset| preset.model.clone())
            .unwrap_or_default()
    }

    #[cfg(any(test, feature = "test-support"))]
    /// Build `ModelInfo` without consulting remote state or cache.
    pub fn construct_model_info_offline(model: &str, config: &Config) -> ModelInfo {
        let candidates = Self::load_remote_models_from_file().unwrap_or_default();
        Self::construct_model_info_from_candidates(model, &candidates, config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CodexAuth;
    use crate::auth::AuthCredentialsStoreMode;
    use crate::config::ConfigBuilder;
    use crate::config::test_config;
    use crate::features::Feature;
    use crate::model_provider_info::WireApi;
    use crate::models_manager::overlay::ModelInfoPatch;
    use crate::models_manager::overlay::ModelInstructionsVariablesPatch;
    use crate::models_manager::overlay::ModelMessagesPatch;
    use crate::models_manager::overlay::ModelOverlay;
    use crate::models_manager::overlay::ModelOverlayEntry;
    use chrono::Utc;
    use codex_protocol::openai_models::ModelsResponse;
    use core_test_support::responses::mount_models_once;
    use pretty_assertions::assert_eq;
    use serde_json::json;
    use tempfile::tempdir;
    use wiremock::MockServer;

    fn remote_model(slug: &str, display: &str, priority: i32) -> ModelInfo {
        remote_model_with_visibility(slug, display, priority, "list")
    }

    fn remote_model_with_visibility(
        slug: &str,
        display: &str,
        priority: i32,
        visibility: &str,
    ) -> ModelInfo {
        serde_json::from_value(json!({
            "slug": slug,
            "display_name": display,
            "description": format!("{display} desc"),
            "default_reasoning_level": "medium",
            "supported_reasoning_levels": [{"effort": "low", "description": "low"}, {"effort": "medium", "description": "medium"}],
            "shell_type": "shell_command",
            "visibility": visibility,
            "minimal_client_version": [0, 1, 0],
            "supported_in_api": true,
            "priority": priority,
            "upgrade": null,
            "base_instructions": "base instructions",
            "supports_reasoning_summaries": false,
            "support_verbosity": false,
            "default_verbosity": null,
            "apply_patch_tool_type": null,
            "truncation_policy": {"mode": "bytes", "limit": 10_000},
            "supports_parallel_tool_calls": false,
            "context_window": 272_000,
            "experimental_supported_tools": [],
        }))
        .expect("valid model")
    }

    fn assert_models_contain(actual: &[ModelInfo], expected: &[ModelInfo]) {
        for model in expected {
            assert!(
                actual.iter().any(|candidate| candidate.slug == model.slug),
                "expected model {} in cached list",
                model.slug
            );
        }
    }

    fn provider_for(base_url: String) -> ModelProviderInfo {
        ModelProviderInfo {
            name: "mock".into(),
            base_url: Some(base_url),
            env_key: None,
            env_key_instructions: None,
            experimental_bearer_token: None,
            wire_api: WireApi::Responses,
            query_params: None,
            http_headers: None,
            env_http_headers: None,
            request_max_retries: Some(0),
            stream_max_retries: Some(0),
            stream_idle_timeout_ms: Some(5_000),
            requires_openai_auth: false,
            supports_websockets: false,
        }
    }

    #[tokio::test]
    async fn refresh_available_models_sorts_by_priority() {
        let server = MockServer::start().await;
        let remote_models = vec![
            remote_model("priority-low", "Low", 1),
            remote_model("priority-high", "High", 0),
        ];
        let models_mock = mount_models_once(
            &server,
            ModelsResponse {
                models: remote_models.clone(),
            },
        )
        .await;

        let codex_home = tempdir().expect("temp dir");
        let mut config = ConfigBuilder::default()
            .codex_home(codex_home.path().to_path_buf())
            .build()
            .await
            .expect("load default test config");
        config.features.enable(Feature::RemoteModels);
        let auth_manager =
            AuthManager::from_auth_for_testing(CodexAuth::create_dummy_chatgpt_auth_for_testing());
        let provider = provider_for(server.uri());
        let manager =
            ModelsManager::with_provider(codex_home.path().to_path_buf(), auth_manager, provider);

        manager
            .refresh_available_models(&config, RefreshStrategy::OnlineIfUncached)
            .await
            .expect("refresh succeeds");
        let cached_remote = manager.get_remote_models(&config).await;
        assert_models_contain(&cached_remote, &remote_models);

        let available = manager
            .list_models(&config, RefreshStrategy::OnlineIfUncached)
            .await;
        let high_idx = available
            .iter()
            .position(|model| model.model == "priority-high")
            .expect("priority-high should be listed");
        let low_idx = available
            .iter()
            .position(|model| model.model == "priority-low")
            .expect("priority-low should be listed");
        assert!(
            high_idx < low_idx,
            "higher priority should be listed before lower priority"
        );
        assert_eq!(
            models_mock.requests().len(),
            1,
            "expected a single /models request"
        );
    }

    #[tokio::test]
    async fn refresh_available_models_uses_cache_when_fresh() {
        let server = MockServer::start().await;
        let remote_models = vec![remote_model("cached", "Cached", 5)];
        let models_mock = mount_models_once(
            &server,
            ModelsResponse {
                models: remote_models.clone(),
            },
        )
        .await;

        let codex_home = tempdir().expect("temp dir");
        let mut config = ConfigBuilder::default()
            .codex_home(codex_home.path().to_path_buf())
            .build()
            .await
            .expect("load default test config");
        config.features.enable(Feature::RemoteModels);
        let auth_manager = Arc::new(AuthManager::new(
            codex_home.path().to_path_buf(),
            false,
            AuthCredentialsStoreMode::File,
        ));
        let provider = provider_for(server.uri());
        let manager =
            ModelsManager::with_provider(codex_home.path().to_path_buf(), auth_manager, provider);

        manager
            .refresh_available_models(&config, RefreshStrategy::OnlineIfUncached)
            .await
            .expect("first refresh succeeds");
        assert_models_contain(&manager.get_remote_models(&config).await, &remote_models);

        // Second call should read from cache and avoid the network.
        manager
            .refresh_available_models(&config, RefreshStrategy::OnlineIfUncached)
            .await
            .expect("cached refresh succeeds");
        assert_models_contain(&manager.get_remote_models(&config).await, &remote_models);
        assert_eq!(
            models_mock.requests().len(),
            1,
            "cache hit should avoid a second /models request"
        );
    }

    #[tokio::test]
    async fn refresh_available_models_refetches_when_cache_stale() {
        let server = MockServer::start().await;
        let initial_models = vec![remote_model("stale", "Stale", 1)];
        let initial_mock = mount_models_once(
            &server,
            ModelsResponse {
                models: initial_models.clone(),
            },
        )
        .await;

        let codex_home = tempdir().expect("temp dir");
        let mut config = ConfigBuilder::default()
            .codex_home(codex_home.path().to_path_buf())
            .build()
            .await
            .expect("load default test config");
        config.features.enable(Feature::RemoteModels);
        let auth_manager = Arc::new(AuthManager::new(
            codex_home.path().to_path_buf(),
            false,
            AuthCredentialsStoreMode::File,
        ));
        let provider = provider_for(server.uri());
        let manager =
            ModelsManager::with_provider(codex_home.path().to_path_buf(), auth_manager, provider);

        manager
            .refresh_available_models(&config, RefreshStrategy::OnlineIfUncached)
            .await
            .expect("initial refresh succeeds");

        // Rewrite cache with an old timestamp so it is treated as stale.
        manager
            .cache_manager
            .manipulate_cache_for_test(|fetched_at| {
                *fetched_at = Utc::now() - chrono::Duration::hours(1);
            })
            .await
            .expect("cache manipulation succeeds");

        let updated_models = vec![remote_model("fresh", "Fresh", 9)];
        server.reset().await;
        let refreshed_mock = mount_models_once(
            &server,
            ModelsResponse {
                models: updated_models.clone(),
            },
        )
        .await;

        manager
            .refresh_available_models(&config, RefreshStrategy::OnlineIfUncached)
            .await
            .expect("second refresh succeeds");
        assert_models_contain(&manager.get_remote_models(&config).await, &updated_models);
        assert_eq!(
            initial_mock.requests().len(),
            1,
            "initial refresh should only hit /models once"
        );
        assert_eq!(
            refreshed_mock.requests().len(),
            1,
            "stale cache refresh should fetch /models once"
        );
    }

    #[tokio::test]
    async fn get_model_info_uses_stale_cache_before_overlay_only_fallback() {
        let codex_home = tempdir().expect("temp dir");
        let auth_manager =
            AuthManager::from_auth_for_testing(CodexAuth::from_api_key("Test API Key"));
        let provider = provider_for("http://example.test".to_string());
        let manager =
            ModelsManager::with_provider(codex_home.path().to_path_buf(), auth_manager, provider);
        let mut config = test_config();
        config.features.enable(Feature::RemoteModels);
        config.model_overlay = Some(ModelOverlay {
            cross_model: ModelInfoPatch {
                effective_context_window_percent: Some(98),
                ..Default::default()
            },
            models: vec![ModelOverlayEntry {
                slug: "gpt-5.5".to_string(),
                model_provider: None,
                patch: ModelInfoPatch::default(),
                final_instruction_override: Some("custom instructions".to_string()),
            }],
            ..Default::default()
        });
        let cached_model = remote_model("gpt-5.5", "GPT-5.5", 0);
        manager
            .cache_manager
            .persist_cache(
                std::slice::from_ref(&cached_model),
                None,
                crate::models_manager::client_version_to_whole(),
            )
            .await;
        manager
            .cache_manager
            .manipulate_cache_for_test(|fetched_at| {
                *fetched_at = Utc::now() - chrono::Duration::hours(1);
            })
            .await
            .expect("cache manipulation succeeds");

        let model_info = manager.get_model_info("gpt-5.5", &config).await;
        let mut expected = cached_model;
        expected.effective_context_window_percent = 98;

        assert_eq!(model_info, expected);
    }

    #[tokio::test]
    async fn refresh_available_models_refetches_when_version_mismatch() {
        let server = MockServer::start().await;
        let initial_models = vec![remote_model("old", "Old", 1)];
        let initial_mock = mount_models_once(
            &server,
            ModelsResponse {
                models: initial_models.clone(),
            },
        )
        .await;

        let codex_home = tempdir().expect("temp dir");
        let mut config = ConfigBuilder::default()
            .codex_home(codex_home.path().to_path_buf())
            .build()
            .await
            .expect("load default test config");
        config.features.enable(Feature::RemoteModels);
        let auth_manager = Arc::new(AuthManager::new(
            codex_home.path().to_path_buf(),
            false,
            AuthCredentialsStoreMode::File,
        ));
        let provider = provider_for(server.uri());
        let manager =
            ModelsManager::with_provider(codex_home.path().to_path_buf(), auth_manager, provider);

        manager
            .refresh_available_models(&config, RefreshStrategy::OnlineIfUncached)
            .await
            .expect("initial refresh succeeds");

        manager
            .cache_manager
            .mutate_cache_for_test(|cache| {
                let client_version = crate::models_manager::client_version_to_whole();
                cache.client_version = Some(format!("{client_version}-mismatch"));
            })
            .await
            .expect("cache mutation succeeds");

        let updated_models = vec![remote_model("new", "New", 2)];
        server.reset().await;
        let refreshed_mock = mount_models_once(
            &server,
            ModelsResponse {
                models: updated_models.clone(),
            },
        )
        .await;

        manager
            .refresh_available_models(&config, RefreshStrategy::OnlineIfUncached)
            .await
            .expect("second refresh succeeds");
        assert_models_contain(&manager.get_remote_models(&config).await, &updated_models);
        assert_eq!(
            initial_mock.requests().len(),
            1,
            "initial refresh should only hit /models once"
        );
        assert_eq!(
            refreshed_mock.requests().len(),
            1,
            "version mismatch should fetch /models once"
        );
    }

    #[tokio::test]
    async fn refresh_available_models_drops_removed_remote_models() {
        let server = MockServer::start().await;
        let initial_models = vec![remote_model("remote-old", "Remote Old", 1)];
        let initial_mock = mount_models_once(
            &server,
            ModelsResponse {
                models: initial_models,
            },
        )
        .await;

        let codex_home = tempdir().expect("temp dir");
        let mut config = ConfigBuilder::default()
            .codex_home(codex_home.path().to_path_buf())
            .build()
            .await
            .expect("load default test config");
        config.features.enable(Feature::RemoteModels);
        let auth_manager =
            AuthManager::from_auth_for_testing(CodexAuth::create_dummy_chatgpt_auth_for_testing());
        let provider = provider_for(server.uri());
        let mut manager =
            ModelsManager::with_provider(codex_home.path().to_path_buf(), auth_manager, provider);
        manager.cache_manager.set_ttl(Duration::ZERO);

        manager
            .refresh_available_models(&config, RefreshStrategy::OnlineIfUncached)
            .await
            .expect("initial refresh succeeds");

        server.reset().await;
        let refreshed_models = vec![remote_model("remote-new", "Remote New", 1)];
        let refreshed_mock = mount_models_once(
            &server,
            ModelsResponse {
                models: refreshed_models,
            },
        )
        .await;

        manager
            .refresh_available_models(&config, RefreshStrategy::OnlineIfUncached)
            .await
            .expect("second refresh succeeds");

        let available = manager
            .try_list_models(&config)
            .expect("models should be available");
        assert!(
            available.iter().any(|preset| preset.model == "remote-new"),
            "new remote model should be listed"
        );
        assert!(
            !available.iter().any(|preset| preset.model == "remote-old"),
            "removed remote model should not be listed"
        );
        assert_eq!(
            initial_mock.requests().len(),
            1,
            "initial refresh should only hit /models once"
        );
        assert_eq!(
            refreshed_mock.requests().len(),
            1,
            "second refresh should only hit /models once"
        );
    }

    #[test]
    fn build_available_models_picks_default_after_hiding_hidden_models() {
        let codex_home = tempdir().expect("temp dir");
        let auth_manager =
            AuthManager::from_auth_for_testing(CodexAuth::from_api_key("Test API Key"));
        let provider = provider_for("http://example.test".to_string());
        let mut manager =
            ModelsManager::with_provider(codex_home.path().to_path_buf(), auth_manager, provider);
        manager.local_models = Vec::new();

        let hidden_model = remote_model_with_visibility("hidden", "Hidden", 0, "hide");
        let visible_model = remote_model_with_visibility("visible", "Visible", 1, "list");

        let expected_hidden = ModelPreset::from(hidden_model.clone());
        let mut expected_visible = ModelPreset::from(visible_model.clone());
        expected_visible.is_default = true;

        let available = manager.build_available_models(vec![hidden_model, visible_model]);

        assert_eq!(available, vec![expected_hidden, expected_visible]);
    }

    #[test]
    fn construct_model_info_offline_applies_custom_overlay_to_fallback_model() {
        let mut config = test_config();
        config.model_overlay = Some(ModelOverlay {
            models: vec![ModelOverlayEntry {
                slug: "private-model".to_string(),
                model_provider: None,
                patch: ModelInfoPatch {
                    context_window: Some(Some(1_048_576)),
                    auto_compact_token_limit: Some(Some(950_000)),
                    ..Default::default()
                },
                final_instruction_override: None,
            }],
            ..Default::default()
        });

        let model = ModelsManager::construct_model_info_offline("private-model", &config);

        assert_eq!(model.slug, "private-model");
        assert_eq!(model.display_name, "private-model");
        assert_eq!(model.context_window, Some(1_048_576));
        assert_eq!(model.auto_compact_token_limit, Some(950_000));
    }

    #[test]
    fn construct_model_info_offline_applies_existing_model_field_overlay() {
        let baseline = ModelsManager::construct_model_info_offline("gpt-5.4", &test_config());
        let mut config = test_config();
        config.model_overlay = Some(ModelOverlay {
            models: vec![ModelOverlayEntry {
                slug: "gpt-5.4".to_string(),
                model_provider: None,
                patch: ModelInfoPatch {
                    context_window: Some(Some(1_000_000)),
                    model_messages: Some(ModelMessagesPatch {
                        instructions_variables: Some(ModelInstructionsVariablesPatch {
                            personality_pragmatic: Some(
                                "Use a patched pragmatic voice.".to_string(),
                            ),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                final_instruction_override: None,
            }],
            ..Default::default()
        });

        let model = ModelsManager::construct_model_info_offline("gpt-5.4", &config);

        assert_eq!(model.context_window, Some(1_000_000));
        assert_eq!(model.display_name, baseline.display_name);
        assert_eq!(
            model
                .model_messages
                .as_ref()
                .and_then(|messages| messages.instructions_variables.as_ref())
                .and_then(|variables| variables.personality_pragmatic.as_deref()),
            Some("Use a patched pragmatic voice.")
        );
        assert_eq!(
            model
                .model_messages
                .as_ref()
                .and_then(|messages| messages.instructions_variables.as_ref())
                .and_then(|variables| variables.personality_friendly.as_deref()),
            baseline
                .model_messages
                .as_ref()
                .and_then(|messages| messages.instructions_variables.as_ref())
                .and_then(|variables| variables.personality_friendly.as_deref())
        );
    }

    #[test]
    fn config_context_window_override_wins_over_overlay() {
        let mut config = test_config();
        config.model_context_window = Some(256_000);
        config.model_overlay = Some(ModelOverlay {
            cross_model: ModelInfoPatch {
                context_window: Some(Some(1_000_000)),
                ..Default::default()
            },
            models: vec![ModelOverlayEntry {
                slug: "gpt-5.4".to_string(),
                model_provider: None,
                patch: ModelInfoPatch {
                    context_window: Some(Some(512_000)),
                    ..Default::default()
                },
                final_instruction_override: None,
            }],
            ..Default::default()
        });

        let model = ModelsManager::construct_model_info_offline("gpt-5.4", &config);

        assert_eq!(model.context_window, Some(256_000));
    }

    #[test]
    fn config_context_window_override_clamps_to_max_context_window() {
        let mut config = test_config();
        config.model_context_window = Some(500_000);
        let mut candidate = remote_model("wide-model", "Wide", 1);
        candidate.context_window = Some(272_000);
        candidate.max_context_window = Some(400_000);

        let model = ModelsManager::construct_model_info_from_candidates(
            "wide-model",
            &[candidate.clone()],
            &config,
        );
        let mut expected = candidate;
        expected.context_window = Some(400_000);

        assert_eq!(model, expected);
    }

    #[tokio::test]
    async fn final_instruction_override_uses_selected_candidate_not_partial_prefix() {
        let codex_home = tempdir().expect("temp dir");
        let auth_manager =
            AuthManager::from_auth_for_testing(CodexAuth::from_api_key("Test API Key"));
        let provider = provider_for("http://example.test".to_string());
        let manager =
            ModelsManager::with_provider(codex_home.path().to_path_buf(), auth_manager, provider);
        let mut config = test_config();
        config.model_overlay = Some(ModelOverlay {
            models: vec![ModelOverlayEntry {
                slug: "gpt-5.4".to_string(),
                model_provider: None,
                patch: ModelInfoPatch::default(),
                final_instruction_override: Some("patched gpt-5.4".to_string()),
            }],
            final_instruction_override: Some("global final".to_string()),
            ..Default::default()
        });

        let exact = manager
            .get_final_instruction_override("custom/gpt-5.4", &config)
            .await;
        let adjacent = manager
            .get_final_instruction_override("gpt-5.4-mini", &config)
            .await;

        assert_eq!(exact.as_deref(), Some("patched gpt-5.4"));
        assert_eq!(adjacent.as_deref(), Some("global final"));
    }

    #[test]
    fn effective_model_instructions_prefers_final_override() {
        let config = test_config();
        let model = ModelsManager::construct_model_info_offline("gpt-5.4", &config);

        let instructions = ModelsManager::effective_model_instructions(
            &model,
            config.personality,
            Some("final instructions"),
        );

        assert_eq!(instructions, "final instructions");
    }

    #[tokio::test]
    async fn overlay_custom_model_can_be_listed_when_remote_models_disabled() {
        let codex_home = tempdir().expect("temp dir");
        let auth_manager =
            AuthManager::from_auth_for_testing(CodexAuth::from_api_key("Test API Key"));
        let provider = provider_for("http://example.test".to_string());
        let manager =
            ModelsManager::with_provider(codex_home.path().to_path_buf(), auth_manager, provider);
        let mut config = test_config();
        config.features.disable(Feature::RemoteModels);
        config.model_overlay = Some(ModelOverlay {
            models: vec![ModelOverlayEntry {
                slug: "private-model".to_string(),
                model_provider: None,
                patch: ModelInfoPatch {
                    display_name: Some("Private Model".to_string()),
                    visibility: Some(codex_protocol::openai_models::ModelVisibility::List),
                    priority: Some(0),
                    ..Default::default()
                },
                final_instruction_override: None,
            }],
            ..Default::default()
        });

        let available = manager.list_models(&config, RefreshStrategy::Offline).await;
        let private = available
            .iter()
            .find(|preset| preset.model == "private-model")
            .expect("custom model should be listed");

        assert_eq!(private.display_name, "Private Model");
        assert!(private.show_in_picker);
    }

    #[tokio::test]
    async fn overlay_custom_models_are_not_persisted_to_cache() {
        let server = MockServer::start().await;
        let remote_models = vec![remote_model("server-model", "Server Model", 1)];
        let _models_mock = mount_models_once(
            &server,
            ModelsResponse {
                models: remote_models,
            },
        )
        .await;

        let codex_home = tempdir().expect("temp dir");
        let mut config = ConfigBuilder::default()
            .codex_home(codex_home.path().to_path_buf())
            .build()
            .await
            .expect("load default test config");
        config.features.enable(Feature::RemoteModels);
        config.model_overlay = Some(ModelOverlay {
            models: vec![ModelOverlayEntry {
                slug: "private-model".to_string(),
                model_provider: None,
                patch: ModelInfoPatch {
                    visibility: Some(codex_protocol::openai_models::ModelVisibility::List),
                    ..Default::default()
                },
                final_instruction_override: None,
            }],
            ..Default::default()
        });
        let auth_manager =
            AuthManager::from_auth_for_testing(CodexAuth::create_dummy_chatgpt_auth_for_testing());
        let provider = provider_for(server.uri());
        let manager =
            ModelsManager::with_provider(codex_home.path().to_path_buf(), auth_manager, provider);

        manager
            .refresh_available_models(&config, RefreshStrategy::OnlineIfUncached)
            .await
            .expect("refresh succeeds");
        let available = manager.list_models(&config, RefreshStrategy::Offline).await;
        assert!(
            available
                .iter()
                .any(|preset| preset.model == "private-model"),
            "overlay model should appear after refresh"
        );

        let cache_path = codex_home.path().join(MODEL_CACHE_FILE);
        let cache_json: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(cache_path).expect("cache should be written"),
        )
        .expect("cache should be JSON");
        let cached_models = cache_json
            .get("models")
            .and_then(serde_json::Value::as_array)
            .expect("cache should contain models");
        assert!(
            cached_models.iter().all(
                |model| model.get("slug").and_then(serde_json::Value::as_str)
                    != Some("private-model")
            ),
            "cache must not persist overlay-generated models"
        );
    }

    #[test]
    fn bundled_models_json_roundtrips() {
        let file_contents = include_str!("../../models.json");
        let response: ModelsResponse =
            serde_json::from_str(file_contents).expect("bundled models.json should deserialize");

        let serialized =
            serde_json::to_string(&response).expect("bundled models.json should serialize");
        let roundtripped: ModelsResponse =
            serde_json::from_str(&serialized).expect("serialized models.json should deserialize");

        assert_eq!(
            response, roundtripped,
            "bundled models.json should round trip through serde"
        );
        assert!(
            !response.models.is_empty(),
            "bundled models.json should contain at least one model"
        );
    }
}
