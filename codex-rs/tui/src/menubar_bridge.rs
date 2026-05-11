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
    use codex_core::protocol::NetworkAccess;
    use codex_core::protocol::RuntimeContextScope;
    use codex_core::protocol::RuntimeContextSnapshot;
    use codex_core::protocol::SandboxPolicy;
    use codex_core::protocol::SessionSource;
    use codex_core::protocol::SubAgentSource;
    use codex_core::protocol::TokenUsageInfo;
    use serde_json::json;
    use std::collections::HashMap;
    use std::collections::HashSet;
    use std::path::PathBuf;

    #[derive(Debug, Clone)]
    struct ActiveTurnState {
        thread_id: String,
        turn_id: String,
    }

    pub struct MenuBarBridge {
        producer: CodexdProducerClient,
        active_turns: HashMap<String, ActiveTurnState>,
        turn_start_order: Vec<String>,
        known_turn_keys: HashSet<String>,
        active_runtime_context: Option<RuntimeContextSnapshot>,
        current_model: Option<String>,
        current_model_provider: Option<String>,
        current_thinking_level: Option<String>,
        current_cwd: Option<String>,
        current_approval: Option<String>,
        current_sandbox: Option<String>,
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
                turn_start_order: Vec::new(),
                known_turn_keys: HashSet::new(),
                active_runtime_context: None,
                current_model: None,
                current_model_provider: None,
                current_thinking_level: None,
                current_cwd: Some(config.cwd.to_string_lossy().into_owned()),
                current_approval: Some(config.approval_policy.value().to_string()),
                current_sandbox: Some(sandbox_status_label(config.sandbox_policy.get())),
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
                EventMsg::TurnStarted(event) => {
                    if let Some(turn_id) = normalize_turn_id(event_turn_id)
                        && let Some(thread_id) = self
                            .active_runtime_context
                            .as_ref()
                            .map(|snapshot| snapshot.session_id.to_string())
                            .or(active_thread_id)
                    {
                        notifications.extend(self.ensure_turn_started(
                            thread_id,
                            turn_id,
                            event.model_context_window,
                        ));
                    }
                }
                EventMsg::SessionConfigured(event) => {
                    self.current_model = Some(event.model.clone());
                    self.current_model_provider = Some(event.model_provider_id.clone());
                    self.current_thinking_level =
                        event.reasoning_effort.as_ref().map(ToString::to_string);
                    self.current_cwd = Some(event.cwd.to_string_lossy().into_owned());
                    self.current_approval = Some(event.approval_policy.to_string());
                    self.current_sandbox = Some(sandbox_status_label(&event.sandbox_policy));
                }
                EventMsg::ItemStarted(item) => {
                    notifications.extend(self.ensure_turn_started(
                        item.thread_id.to_string(),
                        item.turn_id.clone(),
                        None,
                    ));
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
                    notifications.extend(self.ensure_turn_started(
                        item.thread_id.to_string(),
                        item.turn_id.clone(),
                        None,
                    ));
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
                    notifications.extend(self.ensure_turn_started(
                        trace.thread_id.to_string(),
                        trace.turn_id.clone(),
                        None,
                    ));
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
                    notifications.extend(self.ensure_turn_started(
                        event.thread_id.clone(),
                        event.turn_id.clone(),
                        None,
                    ));
                }
                EventMsg::PlanDelta(event) => {
                    notifications.extend(self.ensure_turn_started(
                        event.thread_id.clone(),
                        event.turn_id.clone(),
                        None,
                    ));
                }
                EventMsg::ReasoningContentDelta(event) => {
                    notifications.extend(self.ensure_turn_started(
                        event.thread_id.clone(),
                        event.turn_id.clone(),
                        None,
                    ));
                }
                EventMsg::ReasoningRawContentDelta(event) => {
                    notifications.extend(self.ensure_turn_started(
                        event.thread_id.clone(),
                        event.turn_id.clone(),
                        None,
                    ));
                }
                EventMsg::TokenCount(event) => {
                    if let Some(info) = &event.info {
                        let turn_id = normalize_turn_id(event_turn_id);
                        let turn_key = turn_id
                            .as_deref()
                            .and_then(|id| {
                                active_thread_id.as_deref().and_then(|thread_id| {
                                    let key = turn_key(thread_id, id);
                                    self.active_turns.contains_key(&key).then_some(key)
                                })
                            })
                            .or_else(|| {
                                turn_id
                                    .as_deref()
                                    .and_then(|id| self.resolve_turn_key_for_turn(id))
                            });
                        let thread_id = turn_key
                            .as_deref()
                            .and_then(|key| self.active_turns.get(key))
                            .map(|turn| turn.thread_id.clone())
                            .or(active_thread_id);
                        notifications.push(Self::token_usage_notification(
                            info, thread_id, turn_id, turn_key,
                        ));
                    }
                }
                EventMsg::RuntimeContextActivated(event) => {
                    self.active_runtime_context = Some(event.snapshot.clone());
                    notifications.extend(self.runtime_context_update_notifications(
                        &event.snapshot,
                        normalize_turn_id(event_turn_id),
                    ));
                }
                EventMsg::RuntimeContextUpdated(event) => {
                    self.active_runtime_context = Some(event.snapshot.clone());
                    notifications.extend(self.runtime_context_update_notifications(
                        &event.snapshot,
                        normalize_turn_id(event_turn_id),
                    ));
                }
                EventMsg::RuntimeContextDeactivated(event) => {
                    if self
                        .active_runtime_context
                        .as_ref()
                        .is_some_and(|snapshot| snapshot.scope_id == event.scope_id)
                    {
                        self.active_runtime_context = None;
                    }
                }
                EventMsg::TurnComplete(_) | EventMsg::TurnAborted(_) | EventMsg::TurnPaused(_) => {
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
                // Preserve lifecycle ordering. Spawning each notification independently
                // can let a completion reach codexd before its matching start.
                self.producer.try_publish_hub_notification(notification);
            }
        }

        fn ensure_turn_started(
            &mut self,
            thread_id: String,
            turn_id: String,
            model_context_window: Option<i64>,
        ) -> Vec<HubNotification> {
            let key = format!("{thread_id}:{turn_id}");
            if self.known_turn_keys.contains(&key) {
                return Vec::new();
            }

            self.known_turn_keys.insert(key.clone());
            self.active_turns.insert(
                key.clone(),
                ActiveTurnState {
                    thread_id: thread_id.clone(),
                    turn_id: turn_id.clone(),
                },
            );
            self.turn_start_order.push(key.clone());

            let turn = self.turn_started_payload(&key, &thread_id, &turn_id, model_context_window);

            vec![HubNotification {
                method: "turn/started".to_string(),
                params: Some(json!({
                    "threadId": thread_id,
                    "turn": turn,
                })),
            }]
        }

        fn resolve_turn_key_for_turn(&self, turn_id: &str) -> Option<String> {
            self.turn_start_order.iter().rev().find_map(|key| {
                self.active_turns
                    .get(key)
                    .filter(|turn| turn.turn_id == turn_id)
                    .map(|_| key.clone())
            })
        }

        fn latest_turn_for_thread(&self, thread_id: &str) -> Option<(String, String)> {
            self.turn_start_order.iter().rev().find_map(|key| {
                self.active_turns
                    .get(key)
                    .filter(|turn| turn.thread_id == thread_id)
                    .map(|turn| (key.clone(), turn.turn_id.clone()))
            })
        }

        fn turn_started_payload(
            &self,
            key: &str,
            thread_id: &str,
            turn_id: &str,
            model_context_window: Option<i64>,
        ) -> serde_json::Value {
            if let Some(snapshot) = self
                .active_runtime_context
                .as_ref()
                .filter(|snapshot| snapshot.session_id.to_string() == thread_id)
            {
                let mut turn = runtime_context_params(snapshot);
                turn.insert("id".to_string(), json!(turn_id));
                turn.insert("key".to_string(), json!(key));
                turn.insert("status".to_string(), json!("inProgress"));
                if let Some(model_context_window) =
                    model_context_window.or(snapshot.model_context_window)
                {
                    turn.insert(
                        "modelContextWindow".to_string(),
                        json!(model_context_window),
                    );
                }
                if let Some(info) = snapshot.token_info.as_ref() {
                    turn.insert("tokenUsage".to_string(), token_usage_value(info));
                    if let Some(context_remaining_percent) = context_remaining_percent(info) {
                        turn.insert(
                            "contextRemainingPercent".to_string(),
                            json!(context_remaining_percent),
                        );
                    }
                }
                return serde_json::Value::Object(turn);
            }

            json!({
                "id": turn_id,
                "key": key,
                "status": "inProgress",
                "scope": "primary",
                "taskKind": "user",
                "sessionSource": "cli",
                "model": self.current_model.clone(),
                "modelProvider": self.current_model_provider.clone(),
                "thinkingLevel": self.current_thinking_level.clone(),
                "cwd": self.current_cwd.clone(),
                "approval": self.current_approval.clone(),
                "sandbox": self.current_sandbox.clone(),
                "modelContextWindow": model_context_window,
            })
        }

        fn token_usage_notification(
            info: &codex_core::protocol::TokenUsageInfo,
            thread_id: Option<String>,
            turn_id: Option<String>,
            turn_key: Option<String>,
        ) -> HubNotification {
            let mut params = serde_json::Map::new();
            if let Some(thread_id) = thread_id {
                params.insert("threadId".to_string(), json!(thread_id));
            }
            if let Some(turn_id) = turn_id {
                params.insert("turnId".to_string(), json!(turn_id));
            }
            if let Some(turn_key) = turn_key {
                params.insert("turnKey".to_string(), json!(turn_key));
            }
            if let Some(context_remaining_percent) = context_remaining_percent(info) {
                params.insert(
                    "contextRemainingPercent".to_string(),
                    json!(context_remaining_percent),
                );
            }
            params.insert("tokenUsage".to_string(), token_usage_value(info));
            HubNotification {
                method: "thread/tokenUsage/updated".to_string(),
                params: Some(serde_json::Value::Object(params)),
            }
        }

        fn runtime_context_update_notifications(
            &self,
            snapshot: &RuntimeContextSnapshot,
            event_turn_id: Option<String>,
        ) -> Vec<HubNotification> {
            let thread_id = snapshot.session_id.to_string();
            let resolved_turn = event_turn_id
                .and_then(|turn_id| {
                    self.active_turns
                        .contains_key(&turn_key(&thread_id, &turn_id))
                        .then(|| (turn_key(&thread_id, &turn_id), turn_id))
                })
                .or_else(|| self.latest_turn_for_thread(&thread_id));
            let Some((turn_key, turn_id)) = resolved_turn else {
                return Vec::new();
            };

            let mut params = runtime_context_params(snapshot);
            params.insert("threadId".to_string(), json!(thread_id));
            params.insert("turnId".to_string(), json!(turn_id));
            params.insert("turnKey".to_string(), json!(turn_key));
            if let Some(info) = snapshot.token_info.as_ref() {
                params.insert("tokenUsage".to_string(), token_usage_value(info));
                if let Some(context_remaining_percent) = context_remaining_percent(info) {
                    params.insert(
                        "contextRemainingPercent".to_string(),
                        json!(context_remaining_percent),
                    );
                }
            }

            let mut notifications = vec![HubNotification {
                method: "turn/contextUpdated".to_string(),
                params: Some(serde_json::Value::Object(params)),
            }];
            if let Some(info) = snapshot.token_info.as_ref() {
                notifications.push(Self::token_usage_notification(
                    info,
                    Some(thread_id),
                    Some(turn_id),
                    Some(turn_key),
                ));
            }
            notifications
        }

        fn complete_turn(&mut self, turn_id: Option<String>) -> Vec<HubNotification> {
            if let Some(turn_id) = turn_id
                && let Some(key) = self.resolve_turn_key_for_turn(&turn_id)
                && let Some(turn) = self.active_turns.remove(&key)
            {
                self.turn_start_order.retain(|existing| existing != &key);
                return vec![HubNotification {
                    method: "turn/completed".to_string(),
                    params: Some(json!({
                        "threadId": turn.thread_id,
                        "turn": {
                            "id": turn_id,
                            "key": key,
                            "status": "completed",
                        }
                    })),
                }];
            }

            while let Some(key) = self.turn_start_order.pop() {
                let Some(turn) = self.active_turns.remove(&key) else {
                    continue;
                };
                return vec![HubNotification {
                    method: "turn/completed".to_string(),
                    params: Some(json!({
                        "threadId": turn.thread_id,
                        "turn": {
                            "id": turn.turn_id,
                            "key": key,
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

    fn runtime_context_params(
        snapshot: &RuntimeContextSnapshot,
    ) -> serde_json::Map<String, serde_json::Value> {
        let mut params = serde_json::Map::new();
        params.insert("scope".to_string(), json!(scope_label(&snapshot.scope)));
        if let Some(task_kind) = snapshot.task_kind.as_ref() {
            params.insert("taskKind".to_string(), json!(task_kind));
        }
        params.insert(
            "sessionSource".to_string(),
            json!(snapshot.session_source.to_string()),
        );
        if let Some(sub_agent_source) = sub_agent_source_label(&snapshot.session_source) {
            params.insert("subAgentSource".to_string(), json!(sub_agent_source));
        }
        if let Some(parent_session_id) = snapshot.parent_session_id.as_ref() {
            params.insert(
                "parentThreadId".to_string(),
                json!(parent_session_id.to_string()),
            );
        }
        if let Some(parent_turn_id) = snapshot.parent_turn_id.as_ref() {
            params.insert("parentTurnId".to_string(), json!(parent_turn_id));
        }
        if let Some(thread_name) = snapshot.thread_name.as_ref() {
            params.insert("threadName".to_string(), json!(thread_name));
        }
        params.insert(
            "cwd".to_string(),
            json!(snapshot.cwd.to_string_lossy().into_owned()),
        );
        params.insert("model".to_string(), json!(snapshot.model.as_str()));
        params.insert(
            "modelProvider".to_string(),
            json!(snapshot.model_provider_id.as_str()),
        );
        params.insert(
            "approval".to_string(),
            json!(snapshot.approval_policy.to_string()),
        );
        params.insert(
            "sandbox".to_string(),
            json!(sandbox_status_label(&snapshot.sandbox_policy)),
        );
        if let Some(reasoning_effort) = snapshot.reasoning_effort.as_ref() {
            params.insert(
                "thinkingLevel".to_string(),
                json!(reasoning_effort.to_string()),
            );
        }
        if let Some(model_context_window) = snapshot.model_context_window {
            params.insert(
                "modelContextWindow".to_string(),
                json!(model_context_window),
            );
        }
        params
    }

    fn token_usage_value(info: &TokenUsageInfo) -> serde_json::Value {
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
        })
    }

    fn context_remaining_percent(info: &TokenUsageInfo) -> Option<i64> {
        info.model_context_window.map(|window| {
            info.last_token_usage
                .percent_of_context_window_remaining(window)
        })
    }

    fn turn_key(thread_id: &str, turn_id: &str) -> String {
        format!("{thread_id}:{turn_id}")
    }

    fn scope_label(scope: &RuntimeContextScope) -> &'static str {
        match scope {
            RuntimeContextScope::Primary => "primary",
            RuntimeContextScope::Delegate => "delegate",
        }
    }

    fn sub_agent_source_label(session_source: &SessionSource) -> Option<String> {
        match session_source {
            SessionSource::SubAgent(source) => Some(match source {
                SubAgentSource::Review => "review".to_string(),
                SubAgentSource::Compact => "compact".to_string(),
                SubAgentSource::ThreadSpawn {
                    parent_thread_id,
                    depth,
                } => format!("thread_spawn:{parent_thread_id}:{depth}"),
                SubAgentSource::Other(source) => source.clone(),
            }),
            SessionSource::Cli
            | SessionSource::VSCode
            | SessionSource::Exec
            | SessionSource::Mcp
            | SessionSource::Unknown => None,
        }
    }

    fn sandbox_status_label(policy: &SandboxPolicy) -> String {
        match policy {
            SandboxPolicy::DangerFullAccess => "danger-full-access".to_string(),
            SandboxPolicy::ReadOnly { .. } => "read-only".to_string(),
            SandboxPolicy::WorkspaceWrite { .. } => "workspace-write".to_string(),
            SandboxPolicy::ExternalSandbox { network_access } => {
                if matches!(network_access, NetworkAccess::Enabled) {
                    "external-sandbox (network access enabled)".to_string()
                } else {
                    "external-sandbox".to_string()
                }
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use codex_core::protocol::RuntimeContextScope;
        use codex_core::protocol::TokenUsage;
        use codex_protocol::ThreadId;

        fn test_bridge() -> MenuBarBridge {
            let producer = CodexdProducerClient::spawn_with_socket_path(
                PathBuf::from(format!("/tmp/{}", "x".repeat(200))),
                RuntimeMetadata {
                    runtime_id: "test-runtime".to_string(),
                    pid: None,
                    session_source: Some("cli".to_string()),
                    cwd: None,
                    display_name: Some("test".to_string()),
                },
            );

            MenuBarBridge {
                producer,
                active_turns: HashMap::new(),
                turn_start_order: Vec::new(),
                known_turn_keys: HashSet::new(),
                active_runtime_context: None,
                current_model: Some("parent-model".to_string()),
                current_model_provider: Some("parent-provider".to_string()),
                current_thinking_level: Some("low".to_string()),
                current_cwd: Some("/tmp/parent".to_string()),
                current_approval: Some("never".to_string()),
                current_sandbox: Some("read-only".to_string()),
            }
        }

        fn token_info(total_tokens: i64, context_window: i64) -> TokenUsageInfo {
            let usage = TokenUsage {
                total_tokens,
                ..TokenUsage::default()
            };
            TokenUsageInfo {
                total_token_usage: usage.clone(),
                last_token_usage: usage,
                model_context_window: Some(context_window),
            }
        }

        fn runtime_context() -> RuntimeContextSnapshot {
            RuntimeContextSnapshot {
                scope_id: "delegate-scope".to_string(),
                scope: RuntimeContextScope::Delegate,
                task_kind: Some("review".to_string()),
                session_source: SessionSource::SubAgent(SubAgentSource::Review),
                session_id: ThreadId::new(),
                parent_session_id: Some(ThreadId::new()),
                parent_turn_id: Some("parent-turn".to_string()),
                thread_name: Some("Delegate".to_string()),
                rollout_path: None,
                cwd: PathBuf::from("/tmp/delegate"),
                model: "delegate-model".to_string(),
                model_provider_id: "delegate-provider".to_string(),
                approval_policy: codex_core::protocol::AskForApproval::Never,
                sandbox_policy: SandboxPolicy::new_read_only_policy(),
                reasoning_effort: Some(codex_protocol::openai_models::ReasoningEffort::High),
                service_tier: None,
                model_context_window: Some(100_000),
                agents_summary: Some("delegate agents".to_string()),
                token_info: Some(token_info(90_000, 100_000)),
            }
        }

        #[tokio::test]
        async fn duplicate_turn_ids_are_keyed_by_thread() {
            let mut bridge = test_bridge();

            let first = bridge.ensure_turn_started(
                "thread-a".to_string(),
                "turn-1".to_string(),
                Some(200_000),
            );
            let second = bridge.ensure_turn_started(
                "thread-b".to_string(),
                "turn-1".to_string(),
                Some(100_000),
            );

            assert_eq!(first.len(), 1);
            assert_eq!(second.len(), 1);
            assert_eq!(bridge.active_turns.len(), 2);
            assert_eq!(
                first[0].params.as_ref().unwrap()["turn"]["key"],
                json!("thread-a:turn-1")
            );
            assert_eq!(
                second[0].params.as_ref().unwrap()["turn"]["key"],
                json!("thread-b:turn-1")
            );

            let completed = bridge.complete_turn(Some("turn-1".to_string()));
            assert_eq!(
                completed[0].params.as_ref().unwrap()["threadId"],
                "thread-b"
            );
            let completed = bridge.complete_turn(Some("turn-1".to_string()));
            assert_eq!(
                completed[0].params.as_ref().unwrap()["threadId"],
                "thread-a"
            );

            bridge.shutdown().await;
        }

        #[tokio::test]
        async fn runtime_context_enriches_started_and_update_notifications() {
            let mut bridge = test_bridge();
            let snapshot = runtime_context();
            let thread_id = snapshot.session_id.to_string();
            bridge.active_runtime_context = Some(snapshot.clone());

            let started = bridge.ensure_turn_started(thread_id.clone(), "turn-7".to_string(), None);
            let turn = &started[0].params.as_ref().unwrap()["turn"];
            assert_eq!(turn["key"], json!(format!("{thread_id}:turn-7")));
            assert_eq!(turn["scope"], json!("delegate"));
            assert_eq!(turn["taskKind"], json!("review"));
            assert_eq!(turn["sessionSource"], json!("subagent_review"));
            assert_eq!(turn["subAgentSource"], json!("review"));
            assert_eq!(turn["model"], json!("delegate-model"));
            assert_eq!(turn["modelProvider"], json!("delegate-provider"));
            assert_eq!(turn["modelContextWindow"], json!(100_000));
            assert!(turn["tokenUsage"].is_object());

            let updates =
                bridge.runtime_context_update_notifications(&snapshot, Some("turn-7".to_string()));
            assert_eq!(updates.len(), 2);
            assert_eq!(updates[0].method, "turn/contextUpdated");
            assert_eq!(updates[0].params.as_ref().unwrap()["turnKey"], turn["key"]);
            assert_eq!(updates[1].method, "thread/tokenUsage/updated");
            assert_eq!(updates[1].params.as_ref().unwrap()["turnKey"], turn["key"]);

            bridge.shutdown().await;
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
