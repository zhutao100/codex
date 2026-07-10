use std::sync::Arc;
use std::time::Duration;

use crate::config::Config;
use crate::models_manager::manager::ModelsManager;
use crate::models_manager::manager::RefreshStrategy;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

const MODELS_REFRESH_INTERVAL: Duration = Duration::from_secs(3 * 60);

#[derive(Debug)]
pub struct ModelsRefreshWorker {
    shutdown: CancellationToken,
    _task: JoinHandle<()>,
}

impl ModelsRefreshWorker {
    pub fn shutdown(&self) {
        self.shutdown.cancel();
    }
}

impl Drop for ModelsRefreshWorker {
    fn drop(&mut self) {
        self.shutdown();
    }
}

pub fn spawn(models_manager: &Arc<ModelsManager>, config: Arc<Config>) -> ModelsRefreshWorker {
    spawn_with_interval(models_manager, config, MODELS_REFRESH_INTERVAL)
}

fn spawn_with_interval(
    models_manager: &Arc<ModelsManager>,
    config: Arc<Config>,
    refresh_interval: Duration,
) -> ModelsRefreshWorker {
    let models_manager = Arc::downgrade(models_manager);
    let shutdown = CancellationToken::new();
    let worker_shutdown = shutdown.clone();
    let task = tokio::spawn(async move {
        loop {
            if worker_shutdown.is_cancelled() {
                break;
            }
            let Some(models_manager) = models_manager.upgrade() else {
                break;
            };
            models_manager
                .list_models(&config, RefreshStrategy::Online)
                .await;
            drop(models_manager);

            tokio::select! {
                _ = worker_shutdown.cancelled() => break,
                _ = tokio::time::sleep(refresh_interval) => {}
            }
        }
    });

    ModelsRefreshWorker {
        shutdown,
        _task: task,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::AuthManager;
    use crate::CodexAuth;
    use crate::ModelProviderInfo;
    use crate::WireApi;
    use crate::config::ConfigBuilder;
    use crate::features::Feature;
    use crate::models_manager::cache::ModelsCache;
    use anyhow::Result;
    use codex_protocol::openai_models::ModelsResponse;
    use pretty_assertions::assert_eq;
    use std::sync::Condvar;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use tempfile::tempdir;
    use tokio::sync::Notify;
    use tokio::time::timeout;
    use wiremock::Mock;
    use wiremock::MockServer;
    use wiremock::Respond;
    use wiremock::ResponseTemplate;
    use wiremock::matchers::method;
    use wiremock::matchers::path;

    const CACHE_FILE: &str = "models_cache.json";
    const FIRST_ETAG: &str = "\"first\"";
    const SECOND_ETAG: &str = "\"second\"";

    #[derive(Debug)]
    struct SecondResponseGate {
        response_count: AtomicUsize,
        second_response_started: Notify,
        released: Mutex<bool>,
        release: Condvar,
    }

    impl SecondResponseGate {
        async fn wait_until_started(&self) {
            timeout(Duration::from_secs(1), async {
                loop {
                    let started = self.second_response_started.notified();
                    if self.response_count.load(Ordering::SeqCst) >= 2 {
                        return;
                    }
                    started.await;
                }
            })
            .await
            .expect("expected second model response to start");
        }

        fn release(&self) {
            let mut released = self.released.lock().expect("response gate lock");
            *released = true;
            self.release.notify_one();
        }
    }

    #[derive(Debug)]
    struct SequencedModelsResponder {
        gate: Arc<SecondResponseGate>,
    }

    impl Respond for SequencedModelsResponder {
        fn respond(&self, _: &wiremock::Request) -> ResponseTemplate {
            let response_index = self.gate.response_count.fetch_add(1, Ordering::SeqCst);
            let response = ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_json(ModelsResponse { models: Vec::new() });
            match response_index {
                0 => response.insert_header("etag", FIRST_ETAG),
                _ => {
                    self.gate.second_response_started.notify_one();
                    let mut released = self.gate.released.lock().expect("response gate lock");
                    while !*released {
                        released = self
                            .gate
                            .release
                            .wait(released)
                            .expect("response gate wait");
                    }
                    response.insert_header("etag", SECOND_ETAG)
                }
            }
        }
    }

    #[tokio::test]
    async fn refreshes_periodically_finishes_in_flight_and_stops_when_dropped() -> Result<()> {
        let server = MockServer::start().await;
        let gate = Arc::new(SecondResponseGate {
            response_count: AtomicUsize::new(0),
            second_response_started: Notify::new(),
            released: Mutex::new(false),
            release: Condvar::new(),
        });
        Mock::given(method("GET"))
            .and(path("/models"))
            .respond_with(SequencedModelsResponder {
                gate: Arc::clone(&gate),
            })
            .mount(&server)
            .await;

        let codex_home = tempdir()?;
        let cache_path = codex_home.path().join(CACHE_FILE);
        tokio::fs::write(&cache_path, "").await?;

        let mut config = ConfigBuilder::default()
            .codex_home(codex_home.path().to_path_buf())
            .build()
            .await?;
        config.features.enable(Feature::RemoteModels);

        let manager = Arc::new(ModelsManager::with_provider(
            codex_home.path().to_path_buf(),
            AuthManager::from_auth_for_testing(CodexAuth::create_dummy_chatgpt_auth_for_testing()),
            provider_for(server.uri()),
        ));

        let worker = spawn_with_interval(&manager, Arc::new(config), Duration::from_millis(10));

        wait_for_cache_etag(&cache_path, FIRST_ETAG).await;
        gate.wait_until_started().await;
        drop(worker);
        gate.release();
        let cache_after_second_refresh = wait_for_cache_etag(&cache_path, SECOND_ETAG).await;
        tokio::time::sleep(Duration::from_millis(30)).await;

        assert_eq!(
            cache_after_second_refresh.etag.as_deref(),
            Some(SECOND_ETAG)
        );
        assert_eq!(gate.response_count.load(Ordering::SeqCst), 2);
        Ok(())
    }

    async fn wait_for_cache_etag(path: &std::path::Path, expected_etag: &str) -> ModelsCache {
        timeout(Duration::from_secs(1), async {
            loop {
                if let Ok(contents) = tokio::fs::read(path).await
                    && let Ok(cache) = serde_json::from_slice::<ModelsCache>(&contents)
                    && cache.etag.as_deref() == Some(expected_etag)
                {
                    return cache;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("expected models cache update")
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
}
