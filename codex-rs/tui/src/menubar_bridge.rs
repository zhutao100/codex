use codex_core::AuthManager;
use codex_core::ThreadManager;
use codex_core::config::Config;
use codex_core::protocol::EventMsg;
use std::sync::Arc;
use toml::Value as TomlValue;

#[cfg(target_os = "macos")]
mod imp {
    use super::*;
    use codex_codexd::producer::CodexdProducerClient;
    use codex_codexd::producer::RuntimeMetadata;
    use codex_codexd::protocol::HubNotification;
    use serde_json::json;
    use std::collections::HashMap;
    use std::collections::HashSet;
    use std::path::PathBuf;

    pub struct MenuBarBridge {
        producer: CodexdProducerClient,
        active_turns: HashMap<String, String>,
        turn_key_by_turn_id: HashMap<String, String>,
        turn_start_order: Vec<String>,
        known_turn_keys: HashSet<String>,
        current_model: Option<String>,
        current_model_provider: Option<String>,
        current_thinking_level: Option<String>,
    }

    impl MenuBarBridge {
        pub async fn start(
            _codex_linux_sandbox_exe: Option<PathBuf>,
            config: Arc<Config>,
            _auth_manager: Arc<AuthManager>,
            _thread_manager: Arc<ThreadManager>,
            _cli_overrides: Vec<(String, TomlValue)>,
        ) -> Option<Self> {
            let producer = CodexdProducerClient::spawn(
                config.codex_home.as_path(),
                RuntimeMetadata {
                    runtime_id: format!("pid:{}", std::process::id()),
                    pid: Some(std::process::id()),
                    session_source: Some("cli".to_string()),
                    cwd: Some(config.cwd.to_string_lossy().into_owned()),
                    display_name: Some("codex-tui".to_string()),
                },
            );

            Some(Self {
                producer,
                active_turns: HashMap::new(),
                turn_key_by_turn_id: HashMap::new(),
                turn_start_order: Vec::new(),
                known_turn_keys: HashSet::new(),
                current_model: None,
                current_model_provider: None,
                current_thinking_level: None,
            })
        }

        pub fn publish_event(
            &mut self,
            event: &EventMsg,
            event_turn_id: &str,
            active_thread_id: Option<String>,
        ) {
            let mut notifications = Vec::new();
            match event {
                EventMsg::TurnStarted(_) => {
                    if let Some(turn_id) = normalize_turn_id(event_turn_id)
                        && let Some(thread_id) = active_thread_id
                    {
                        notifications.extend(self.ensure_turn_started(thread_id, turn_id));
                    }
                }
                EventMsg::SessionConfigured(event) => {
                    self.current_model = Some(event.model.clone());
                    self.current_model_provider = Some(event.model_provider_id.clone());
                    self.current_thinking_level =
                        event.reasoning_effort.as_ref().map(ToString::to_string);
                }
                EventMsg::ItemStarted(item) => {
                    notifications.extend(
                        self.ensure_turn_started(item.thread_id.to_string(), item.turn_id.clone()),
                    );
                    notifications.push(HubNotification {
                        method: "item/started".to_string(),
                        params: Some(json!({
                            "threadId": item.thread_id,
                            "turnId": item.turn_id,
                            "item": item.item,
                        })),
                    });
                }
                EventMsg::ItemCompleted(item) => {
                    notifications.extend(
                        self.ensure_turn_started(item.thread_id.to_string(), item.turn_id.clone()),
                    );
                    notifications.push(HubNotification {
                        method: "item/completed".to_string(),
                        params: Some(json!({
                            "threadId": item.thread_id,
                            "turnId": item.turn_id,
                            "item": item.item,
                        })),
                    });
                }
                EventMsg::ProgressTrace(trace) => {
                    notifications.extend(
                        self.ensure_turn_started(
                            trace.thread_id.to_string(),
                            trace.turn_id.clone(),
                        ),
                    );
                    notifications.push(HubNotification {
                        method: "turn/progressTrace".to_string(),
                        params: Some(json!({
                            "threadId": trace.thread_id,
                            "turnId": trace.turn_id,
                            "category": trace.category,
                            "state": trace.state,
                            "label": trace.label,
                        })),
                    });
                }
                EventMsg::AgentMessageContentDelta(event) => {
                    notifications.extend(
                        self.ensure_turn_started(event.thread_id.clone(), event.turn_id.clone()),
                    );
                }
                EventMsg::PlanDelta(event) => {
                    notifications.extend(
                        self.ensure_turn_started(event.thread_id.clone(), event.turn_id.clone()),
                    );
                }
                EventMsg::ReasoningContentDelta(event) => {
                    notifications.extend(
                        self.ensure_turn_started(event.thread_id.clone(), event.turn_id.clone()),
                    );
                }
                EventMsg::ReasoningRawContentDelta(event) => {
                    notifications.extend(
                        self.ensure_turn_started(event.thread_id.clone(), event.turn_id.clone()),
                    );
                }
                EventMsg::TokenCount(event) => {
                    if let Some(info) = &event.info {
                        let turn_id = normalize_turn_id(event_turn_id);
                        let thread_id = turn_id
                            .as_deref()
                            .and_then(|id| self.resolve_thread_id_for_turn(id))
                            .or(active_thread_id.clone());
                        notifications
                            .push(Self::token_usage_notification(info, thread_id, turn_id));
                    }
                }
                EventMsg::TurnComplete(_) | EventMsg::TurnAborted(_) => {
                    notifications.extend(self.complete_turn(normalize_turn_id(event_turn_id)));
                }
                EventMsg::Error(error) => {
                    notifications.push(HubNotification {
                        method: "error".to_string(),
                        params: Some(json!({
                            "error": {
                                "message": error.message,
                            },
                            "willRetry": false,
                        })),
                    });
                }
                _ => {}
            }

            for notification in notifications {
                let producer = self.producer.clone();
                tokio::spawn(async move {
                    producer.publish_hub_notification(notification).await;
                });
            }
        }

        fn ensure_turn_started(
            &mut self,
            thread_id: String,
            turn_id: String,
        ) -> Vec<HubNotification> {
            let key = format!("{thread_id}:{turn_id}");
            if self.known_turn_keys.contains(&key) {
                return Vec::new();
            }

            if let Some(existing_key) = self.turn_key_by_turn_id.get(&turn_id)
                && existing_key != &key
            {
                return Vec::new();
            }

            self.known_turn_keys.insert(key.clone());
            self.active_turns.insert(key.clone(), thread_id.clone());
            self.turn_key_by_turn_id
                .insert(turn_id.clone(), key.clone());
            self.turn_start_order.push(key);

            vec![HubNotification {
                method: "turn/started".to_string(),
                params: Some(json!({
                    "threadId": thread_id,
                    "turn": {
                        "id": turn_id,
                        "status": "inProgress",
                        "model": self.current_model.clone(),
                        "modelProvider": self.current_model_provider.clone(),
                        "thinkingLevel": self.current_thinking_level.clone(),
                    }
                })),
            }]
        }

        fn resolve_thread_id_for_turn(&self, turn_id: &str) -> Option<String> {
            let key = self.turn_key_by_turn_id.get(turn_id)?;
            self.active_turns.get(key).cloned()
        }

        fn token_usage_notification(
            info: &codex_core::protocol::TokenUsageInfo,
            thread_id: Option<String>,
            turn_id: Option<String>,
        ) -> HubNotification {
            let mut params = serde_json::Map::new();
            if let Some(thread_id) = thread_id {
                params.insert("threadId".to_string(), json!(thread_id));
            }
            if let Some(turn_id) = turn_id {
                params.insert("turnId".to_string(), json!(turn_id));
            }
            params.insert(
                "tokenUsage".to_string(),
                json!({
                    "total": {
                        "totalTokens": info.total_token_usage.total_tokens,
                        "inputTokens": info.total_token_usage.input_tokens,
                        "cachedInputTokens": info.total_token_usage.cached_input_tokens,
                        "outputTokens": info.total_token_usage.output_tokens,
                        "reasoningOutputTokens": info.total_token_usage.reasoning_output_tokens,
                    },
                    "last": {
                        "totalTokens": info.last_token_usage.total_tokens,
                        "inputTokens": info.last_token_usage.input_tokens,
                        "cachedInputTokens": info.last_token_usage.cached_input_tokens,
                        "outputTokens": info.last_token_usage.output_tokens,
                        "reasoningOutputTokens": info.last_token_usage.reasoning_output_tokens,
                    },
                    "modelContextWindow": info.model_context_window,
                }),
            );
            HubNotification {
                method: "thread/tokenUsage/updated".to_string(),
                params: Some(serde_json::Value::Object(params)),
            }
        }

        fn complete_turn(&mut self, turn_id: Option<String>) -> Vec<HubNotification> {
            if let Some(turn_id) = turn_id
                && let Some(key) = self.turn_key_by_turn_id.remove(&turn_id)
                && let Some(thread_id) = self.active_turns.remove(&key)
            {
                self.turn_start_order.retain(|existing| existing != &key);
                return vec![HubNotification {
                    method: "turn/completed".to_string(),
                    params: Some(json!({
                        "threadId": thread_id,
                        "turn": {
                            "id": turn_id,
                            "status": "completed",
                        }
                    })),
                }];
            }

            while let Some(key) = self.turn_start_order.pop() {
                let Some(thread_id) = self.active_turns.remove(&key) else {
                    continue;
                };
                let Some((_, turn_id)) = key.split_once(':') else {
                    continue;
                };
                self.turn_key_by_turn_id.remove(turn_id);
                return vec![HubNotification {
                    method: "turn/completed".to_string(),
                    params: Some(json!({
                        "threadId": thread_id,
                        "turn": {
                            "id": turn_id,
                            "status": "completed",
                        }
                    })),
                }];
            }

            Vec::new()
        }

        pub async fn shutdown(self) {
            self.producer.shutdown().await;
        }
    }
}

#[cfg(target_os = "macos")]
fn normalize_turn_id(turn_id: &str) -> Option<String> {
    let trimmed = turn_id.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.to_string())
}

#[cfg(not(target_os = "macos"))]
mod imp {
    use super::*;
    use std::path::PathBuf;

    pub struct MenuBarBridge;

    impl MenuBarBridge {
        pub async fn start(
            _codex_linux_sandbox_exe: Option<PathBuf>,
            _config: Arc<Config>,
            _auth_manager: Arc<AuthManager>,
            _thread_manager: Arc<ThreadManager>,
            _cli_overrides: Vec<(String, TomlValue)>,
        ) -> Option<Self> {
            None
        }

        pub fn publish_event(
            &mut self,
            _event: &EventMsg,
            _event_turn_id: &str,
            _active_thread_id: Option<String>,
        ) {
        }

        pub async fn shutdown(self) {}
    }
}

pub use imp::MenuBarBridge;
