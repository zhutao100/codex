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
    use codex_core::protocol::FileChange;
    use codex_core::protocol::NetworkAccess;
    use codex_core::protocol::RuntimeContextScope;
    use codex_core::protocol::RuntimeContextSnapshot;
    use codex_core::protocol::SandboxPolicy;
    use codex_core::protocol::SessionSource;
    use codex_core::protocol::SubAgentSource;
    use codex_core::protocol::TokenUsageInfo;
    use codex_protocol::plan_tool::StepStatus;
    use codex_protocol::plan_tool::UpdatePlanArgs;
    use serde_json::json;
    use std::collections::HashMap;
    use std::collections::HashSet;
    use std::path::PathBuf;

    #[derive(Debug, Clone)]
    struct ActiveTurnState {
        thread_id: String,
        turn_id: String,
    }

    #[derive(Debug, Clone, Copy)]
    enum FileChangeLifecycle {
        Started,
        Completed { success: bool },
    }

    impl FileChangeLifecycle {
        fn method(self) -> &'static str {
            match self {
                Self::Started => "item/started",
                Self::Completed { .. } => "item/completed",
            }
        }

        fn status(self) -> &'static str {
            match self {
                Self::Started => "inProgress",
                Self::Completed { success: true } => "completed",
                Self::Completed { success: false } => "failed",
            }
        }

        fn is_start(self) -> bool {
            matches!(self, Self::Started)
        }
    }

    pub struct MenuBarBridge {
        producer: CodexdProducerClient,
        active_turns: HashMap<String, ActiveTurnState>,
        turn_start_order: Vec<String>,
        known_turn_keys: HashSet<String>,
        file_change_started: HashSet<String>,
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
                file_change_started: HashSet::new(),
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
                    if let Some((thread_id, turn_id)) =
                        self.resolve_event_turn(event_turn_id, active_thread_id.as_deref())
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
                EventMsg::PlanUpdate(update) => {
                    notifications.extend(self.plan_update_notifications(
                        update,
                        event_turn_id,
                        active_thread_id.as_deref(),
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
                        let resolved_turn =
                            self.resolve_event_turn(event_turn_id, active_thread_id.as_deref());
                        let turn_id = resolved_turn
                            .as_ref()
                            .map(|(_, turn_id)| turn_id.clone())
                            .or_else(|| normalize_turn_id(event_turn_id));
                        let turn_key = turn_id
                            .as_deref()
                            .and_then(|id| {
                                resolved_turn.as_ref().and_then(|(thread_id, _)| {
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
                            .or_else(|| resolved_turn.map(|(thread_id, _)| thread_id))
                            .or(active_thread_id);
                        notifications.push(Self::token_usage_notification(
                            info, thread_id, turn_id, turn_key,
                        ));
                    }
                }
                EventMsg::PatchApplyBegin(event) => {
                    notifications.extend(
                        self.file_change_started_notifications(
                            event.call_id.as_str(),
                            normalize_turn_id(event.turn_id.as_str())
                                .or_else(|| normalize_turn_id(event_turn_id)),
                            active_thread_id.as_deref(),
                            &event.changes,
                        ),
                    );
                }
                EventMsg::PatchApplyEnd(event) => {
                    notifications.extend(
                        self.file_change_completed_notifications(
                            event.call_id.as_str(),
                            normalize_turn_id(event.turn_id.as_str())
                                .or_else(|| normalize_turn_id(event_turn_id)),
                            active_thread_id.as_deref(),
                            &event.changes,
                            event.success,
                        ),
                    );
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
                    let session_id = event.session_id.to_string();
                    notifications.extend(self.complete_turns_for_thread(&session_id));
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

        fn plan_update_notifications(
            &mut self,
            update: &UpdatePlanArgs,
            event_turn_id: &str,
            active_thread_id: Option<&str>,
        ) -> Vec<HubNotification> {
            let Some((thread_id, turn_id)) =
                self.resolve_event_turn(event_turn_id, active_thread_id)
            else {
                return Vec::new();
            };

            let mut notifications =
                self.ensure_turn_started(thread_id.clone(), turn_id.clone(), None);
            let plan = update
                .plan
                .iter()
                .map(|step| {
                    json!({
                        "step": step.step.clone(),
                        "status": plan_step_status_label(&step.status),
                    })
                })
                .collect::<Vec<_>>();

            notifications.push(HubNotification {
                method: "turn/plan/updated".to_string(),
                params: Some(json!({
                    "threadId": thread_id,
                    "turnId": turn_id,
                    "explanation": update.explanation.clone(),
                    "plan": plan,
                })),
            });
            notifications
        }

        fn file_change_started_notifications(
            &mut self,
            item_id: &str,
            turn_id: Option<String>,
            active_thread_id: Option<&str>,
            changes: &HashMap<PathBuf, FileChange>,
        ) -> Vec<HubNotification> {
            self.file_change_notifications(
                FileChangeLifecycle::Started,
                item_id,
                turn_id,
                active_thread_id,
                changes,
            )
        }

        fn file_change_completed_notifications(
            &mut self,
            item_id: &str,
            turn_id: Option<String>,
            active_thread_id: Option<&str>,
            changes: &HashMap<PathBuf, FileChange>,
            success: bool,
        ) -> Vec<HubNotification> {
            self.file_change_notifications(
                FileChangeLifecycle::Completed { success },
                item_id,
                turn_id,
                active_thread_id,
                changes,
            )
        }

        fn file_change_notifications(
            &mut self,
            lifecycle: FileChangeLifecycle,
            item_id: &str,
            turn_id: Option<String>,
            active_thread_id: Option<&str>,
            changes: &HashMap<PathBuf, FileChange>,
        ) -> Vec<HubNotification> {
            let Some(turn_id) = turn_id else {
                return Vec::new();
            };
            let Some(thread_id) = self.resolve_thread_id(&turn_id, active_thread_id) else {
                return Vec::new();
            };

            let change_key = file_change_key(&thread_id, &turn_id, item_id);
            if lifecycle.is_start() && !self.file_change_started.insert(change_key.clone()) {
                return Vec::new();
            }
            if !lifecycle.is_start() {
                self.file_change_started.remove(&change_key);
            }

            let mut notifications =
                self.ensure_turn_started(thread_id.clone(), turn_id.clone(), None);
            notifications.push(HubNotification {
                method: lifecycle.method().to_string(),
                params: Some(json!({
                    "threadId": thread_id,
                    "turnId": turn_id,
                    "item": {
                        "type": "fileChange",
                        "id": item_id,
                        "changes": file_update_changes(changes),
                        "status": lifecycle.status(),
                    },
                })),
            });
            notifications
        }

        fn resolve_turn_key_for_turn(&self, turn_id: &str) -> Option<String> {
            self.turn_start_order.iter().rev().find_map(|key| {
                self.active_turns
                    .get(key)
                    .filter(|turn| turn.turn_id == turn_id)
                    .map(|_| key.clone())
            })
        }

        fn resolve_thread_id(
            &self,
            turn_id: &str,
            active_thread_id: Option<&str>,
        ) -> Option<String> {
            self.active_runtime_context
                .as_ref()
                .map(|snapshot| snapshot.session_id.to_string())
                .or_else(|| active_thread_id.map(ToString::to_string))
                .or_else(|| {
                    self.resolve_turn_key_for_turn(turn_id)
                        .and_then(|key| self.active_turns.get(&key))
                        .map(|turn| turn.thread_id.clone())
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

        fn resolve_event_turn(
            &self,
            event_turn_id: &str,
            active_thread_id: Option<&str>,
        ) -> Option<(String, String)> {
            if let Some(snapshot) = self.active_runtime_context.as_ref() {
                let thread_id = snapshot.session_id.to_string();
                if let Some((_, turn_id)) = self.latest_turn_for_thread(&thread_id) {
                    return Some((thread_id, turn_id));
                }
                return normalize_turn_id(event_turn_id).map(|turn_id| (thread_id, turn_id));
            }

            let turn_id = normalize_turn_id(event_turn_id)?;
            let thread_id = self.resolve_thread_id(&turn_id, active_thread_id)?;
            Some((thread_id, turn_id))
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
            &mut self,
            snapshot: &RuntimeContextSnapshot,
            event_turn_id: Option<String>,
        ) -> Vec<HubNotification> {
            let thread_id = snapshot.session_id.to_string();
            let mut notifications = Vec::new();
            let resolved_turn = event_turn_id
                .as_ref()
                .and_then(|turn_id| {
                    self.active_turns
                        .contains_key(&turn_key(&thread_id, turn_id))
                        .then(|| (turn_key(&thread_id, turn_id), turn_id.clone()))
                })
                .or_else(|| self.latest_turn_for_thread(&thread_id));
            let (turn_key, turn_id) = if let Some(resolved_turn) = resolved_turn {
                resolved_turn
            } else if let Some(turn_id) = event_turn_id {
                let key = turn_key(&thread_id, &turn_id);
                notifications.extend(self.ensure_turn_started(
                    thread_id.clone(),
                    turn_id.clone(),
                    snapshot.model_context_window,
                ));
                if !self.active_turns.contains_key(&key) {
                    return notifications;
                }
                (key, turn_id)
            } else {
                return notifications;
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

            notifications.push(HubNotification {
                method: "turn/contextUpdated".to_string(),
                params: Some(serde_json::Value::Object(params)),
            });
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
            if let Some(turn_id) = turn_id {
                let Some(key) = self.resolve_turn_key_for_turn(&turn_id) else {
                    return Vec::new();
                };
                let Some(turn) = self.active_turns.remove(&key) else {
                    return Vec::new();
                };
                self.turn_start_order.retain(|existing| existing != &key);
                self.prune_file_changes_for_turn(&key);
                return vec![Self::turn_completed_notification(key, turn)];
            }

            while let Some(key) = self.turn_start_order.pop() {
                let Some(turn) = self.active_turns.remove(&key) else {
                    continue;
                };
                self.prune_file_changes_for_turn(&key);
                return vec![Self::turn_completed_notification(key, turn)];
            }

            Vec::new()
        }

        fn complete_turns_for_thread(&mut self, thread_id: &str) -> Vec<HubNotification> {
            let keys = self
                .turn_start_order
                .iter()
                .filter(|key| {
                    self.active_turns
                        .get(*key)
                        .is_some_and(|turn| turn.thread_id == thread_id)
                })
                .cloned()
                .collect::<Vec<_>>();

            if keys.is_empty() {
                return Vec::new();
            }

            self.turn_start_order
                .retain(|existing| !keys.contains(existing));

            keys.into_iter()
                .filter_map(|key| {
                    self.prune_file_changes_for_turn(&key);
                    self.active_turns
                        .remove(&key)
                        .map(|turn| Self::turn_completed_notification(key, turn))
                })
                .collect()
        }

        fn prune_file_changes_for_turn(&mut self, turn_key: &str) {
            let prefix = format!("{turn_key}:");
            self.file_change_started
                .retain(|existing| !existing.starts_with(&prefix));
        }

        fn turn_completed_notification(key: String, turn: ActiveTurnState) -> HubNotification {
            HubNotification {
                method: "turn/completed".to_string(),
                params: Some(json!({
                    "threadId": turn.thread_id,
                    "turn": {
                        "id": turn.turn_id,
                        "key": key,
                        "status": "completed",
                    }
                })),
            }
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

    fn file_change_key(thread_id: &str, turn_id: &str, item_id: &str) -> String {
        format!("{}:{item_id}", turn_key(thread_id, turn_id))
    }

    fn plan_step_status_label(status: &StepStatus) -> &'static str {
        match status {
            StepStatus::Pending => "pending",
            StepStatus::InProgress => "inProgress",
            StepStatus::Completed => "completed",
        }
    }

    fn file_update_changes(changes: &HashMap<PathBuf, FileChange>) -> Vec<serde_json::Value> {
        let mut entries = changes.iter().collect::<Vec<_>>();
        entries.sort_by(|(lhs, _), (rhs, _)| lhs.cmp(rhs));
        entries
            .into_iter()
            .map(|(path, change)| {
                json!({
                    "path": path.to_string_lossy(),
                    "kind": patch_change_kind_value(change),
                    "diff": file_change_diff(change),
                })
            })
            .collect()
    }

    fn patch_change_kind_value(change: &FileChange) -> serde_json::Value {
        match change {
            FileChange::Add { .. } => json!({ "type": "add" }),
            FileChange::Delete { .. } => json!({ "type": "delete" }),
            FileChange::Update { move_path, .. } => json!({
                "type": "update",
                "movePath": move_path,
            }),
        }
    }

    fn file_change_diff(change: &FileChange) -> String {
        match change {
            FileChange::Add { content } | FileChange::Delete { content } => content.clone(),
            FileChange::Update {
                unified_diff,
                move_path,
            } => {
                if let Some(path) = move_path {
                    format!("{unified_diff}\n\nMoved to: {}", path.display())
                } else {
                    unified_diff.clone()
                }
            }
        }
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
        use pretty_assertions::assert_eq;

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
                file_change_started: HashSet::new(),
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

        #[tokio::test]
        async fn runtime_context_activation_starts_delegate_turn_before_turn_started_event() {
            let mut bridge = test_bridge();
            let snapshot = runtime_context();
            let thread_id = snapshot.session_id.to_string();
            bridge.active_runtime_context = Some(snapshot.clone());

            let notifications = bridge.runtime_context_update_notifications(
                &snapshot,
                Some("post-turn-review-0".to_string()),
            );

            assert_eq!(notifications.len(), 3);
            assert_eq!(notifications[0].method, "turn/started");
            assert_eq!(notifications[1].method, "turn/contextUpdated");
            assert_eq!(notifications[2].method, "thread/tokenUsage/updated");
            let params = notifications[0]
                .params
                .as_ref()
                .expect("turn started params");
            assert_eq!(params["threadId"], json!(thread_id));
            assert_eq!(params["turn"]["id"], json!("post-turn-review-0"));
            assert_eq!(
                params["turn"]["key"],
                json!(format!("{thread_id}:post-turn-review-0"))
            );
            assert_eq!(params["turn"]["taskKind"], json!("review"));
            assert!(params["turn"]["tokenUsage"].is_object());

            let duplicate_started =
                bridge.ensure_turn_started(thread_id, "post-turn-review-0".to_string(), None);
            assert!(duplicate_started.is_empty());

            bridge.shutdown().await;
        }

        #[tokio::test]
        async fn runtime_context_publish_event_prestarts_delegate_turn_before_turn_started_event() {
            let mut bridge = test_bridge();
            let snapshot = runtime_context();
            let thread_id = snapshot.session_id.to_string();
            let expected_key = format!("{thread_id}:post-turn-review-0");

            bridge.publish_event(
                &EventMsg::RuntimeContextActivated(
                    codex_protocol::protocol::RuntimeContextActivatedEvent {
                        snapshot: snapshot.clone(),
                    },
                ),
                "post-turn-review-0",
                None,
            );

            assert_eq!(bridge.active_turns.len(), 1);
            assert!(bridge.active_turns.contains_key(&expected_key));
            assert_eq!(bridge.turn_start_order, vec![expected_key.clone()]);

            let mut updated_snapshot = snapshot;
            updated_snapshot.token_info = Some(token_info(12_000, 100_000));
            bridge.publish_event(
                &EventMsg::RuntimeContextUpdated(
                    codex_protocol::protocol::RuntimeContextUpdatedEvent {
                        snapshot: updated_snapshot,
                    },
                ),
                "post-turn-review-0",
                None,
            );

            assert_eq!(bridge.active_turns.len(), 1);
            assert_eq!(bridge.turn_start_order, vec![expected_key]);

            bridge.shutdown().await;
        }

        #[tokio::test]
        async fn forwarded_parent_delegate_events_reuse_runtime_context_turn() {
            let mut bridge = test_bridge();
            let mut snapshot = runtime_context();
            snapshot.task_kind = Some("post_turn_completion_review".to_string());
            snapshot.token_info = None;
            let thread_id = snapshot.session_id.to_string();
            let runtime_turn_key = format!("{thread_id}:0");
            bridge.active_runtime_context = Some(snapshot.clone());

            let started =
                bridge.runtime_context_update_notifications(&snapshot, Some("0".to_string()));
            assert_eq!(started.len(), 2);
            assert_eq!(started[0].method, "turn/started");
            assert_eq!(
                started[0].params.as_ref().unwrap()["turn"]["key"],
                json!(runtime_turn_key)
            );

            bridge.publish_event(
                &EventMsg::TurnStarted(codex_core::protocol::TurnStartedEvent {
                    model_context_window: Some(100_000),
                    collaboration_mode_kind: Default::default(),
                }),
                "post-turn-review-0",
                None,
            );
            assert_eq!(bridge.active_turns.len(), 1);
            assert!(bridge.active_turns.contains_key(&runtime_turn_key));

            bridge.publish_event(
                &EventMsg::PlanUpdate(UpdatePlanArgs {
                    explanation: Some("review checklist".to_string()),
                    plan: vec![codex_protocol::plan_tool::PlanItemArg {
                        step: "Inspect completed turn".to_string(),
                        status: StepStatus::InProgress,
                    }],
                }),
                "post-turn-review-0",
                None,
            );
            assert_eq!(bridge.active_turns.len(), 1);
            assert!(bridge.active_turns.contains_key(&runtime_turn_key));

            snapshot.token_info = Some(token_info(12_000, 100_000));
            bridge.active_runtime_context = Some(snapshot.clone());
            let updates =
                bridge.runtime_context_update_notifications(&snapshot, Some("0".to_string()));
            assert_eq!(updates.len(), 2);
            assert_eq!(updates[0].method, "turn/contextUpdated");
            assert_eq!(updates[1].method, "thread/tokenUsage/updated");
            assert_eq!(
                updates[1].params.as_ref().unwrap()["turnKey"],
                json!(runtime_turn_key)
            );

            let completed = bridge.complete_turns_for_thread(&thread_id);
            assert_eq!(completed.len(), 1);
            assert_eq!(
                completed[0].params.as_ref().unwrap()["turn"]["key"],
                json!(runtime_turn_key)
            );

            bridge.shutdown().await;
        }

        #[tokio::test]
        async fn runtime_context_token_update_refreshes_prestarted_delegate_turn() {
            let mut bridge = test_bridge();
            let mut snapshot = runtime_context();
            let thread_id = snapshot.session_id.to_string();
            bridge.active_runtime_context = Some(snapshot.clone());
            let started = bridge.runtime_context_update_notifications(
                &snapshot,
                Some("post-turn-review-0".to_string()),
            );
            assert_eq!(started.len(), 3);

            snapshot.token_info = Some(token_info(12_000, 100_000));
            bridge.active_runtime_context = Some(snapshot.clone());
            let updates = bridge.runtime_context_update_notifications(
                &snapshot,
                Some("post-turn-review-0".to_string()),
            );

            assert_eq!(updates.len(), 2);
            assert_eq!(updates[0].method, "turn/contextUpdated");
            assert_eq!(updates[1].method, "thread/tokenUsage/updated");
            let expected_key = json!(format!("{thread_id}:post-turn-review-0"));
            assert_eq!(updates[0].params.as_ref().unwrap()["turnKey"], expected_key);
            assert_eq!(updates[1].params.as_ref().unwrap()["turnKey"], expected_key);
            assert_eq!(
                updates[1].params.as_ref().unwrap()["tokenUsage"]["last"]["totalTokens"],
                json!(12_000)
            );

            bridge.shutdown().await;
        }

        #[tokio::test]
        async fn runtime_context_deactivation_completes_delegate_turns() {
            let mut bridge = test_bridge();
            let snapshot = runtime_context();
            let thread_id = snapshot.session_id.to_string();
            bridge.active_runtime_context = Some(snapshot);

            let started = bridge.ensure_turn_started(
                thread_id.clone(),
                "post-turn-review-0".to_string(),
                None,
            );
            assert_eq!(started.len(), 1);
            assert_eq!(bridge.active_turns.len(), 1);

            let completed = bridge.complete_turns_for_thread(&thread_id);
            assert_eq!(completed.len(), 1);
            assert_eq!(completed[0].method, "turn/completed");
            assert_eq!(
                completed[0].params.as_ref().unwrap()["threadId"],
                json!(thread_id)
            );
            assert_eq!(
                completed[0].params.as_ref().unwrap()["turn"]["id"],
                json!("post-turn-review-0")
            );
            assert!(bridge.active_turns.is_empty());
            assert!(
                bridge
                    .complete_turn(Some("post-turn-review-0".to_string()))
                    .is_empty()
            );

            bridge.shutdown().await;
        }

        #[tokio::test]
        async fn duplicate_delegate_completion_does_not_complete_latest_primary_turn() {
            let mut bridge = test_bridge();
            let primary = bridge.ensure_turn_started(
                "primary-thread".to_string(),
                "primary-turn".to_string(),
                None,
            );
            assert_eq!(primary.len(), 1);

            let snapshot = runtime_context();
            let delegate_thread_id = snapshot.session_id.to_string();
            bridge.active_runtime_context = Some(snapshot);
            let delegate = bridge.ensure_turn_started(
                delegate_thread_id.clone(),
                "post-turn-review-0".to_string(),
                None,
            );
            assert_eq!(delegate.len(), 1);
            assert_eq!(bridge.active_turns.len(), 2);

            let completed = bridge.complete_turns_for_thread(&delegate_thread_id);
            assert_eq!(completed.len(), 1);
            assert_eq!(
                completed[0].params.as_ref().unwrap()["turn"]["id"],
                json!("post-turn-review-0")
            );

            assert!(
                bridge
                    .complete_turn(Some("post-turn-review-0".to_string()))
                    .is_empty()
            );
            assert_eq!(bridge.active_turns.len(), 1);
            assert!(
                bridge
                    .active_turns
                    .contains_key("primary-thread:primary-turn")
            );

            bridge.shutdown().await;
        }

        #[tokio::test]
        async fn plan_update_emits_turn_plan_notification() {
            let mut bridge = test_bridge();
            let update = UpdatePlanArgs {
                explanation: Some("adjust plan".to_string()),
                plan: vec![
                    codex_protocol::plan_tool::PlanItemArg {
                        step: "inspect bridge".to_string(),
                        status: StepStatus::Completed,
                    },
                    codex_protocol::plan_tool::PlanItemArg {
                        step: "wire event".to_string(),
                        status: StepStatus::InProgress,
                    },
                ],
            };

            let notifications =
                bridge.plan_update_notifications(&update, "turn-1", Some("thread-1"));

            assert_eq!(notifications.len(), 2);
            assert_eq!(notifications[0].method, "turn/started");
            assert_eq!(notifications[1].method, "turn/plan/updated");
            let params = notifications[1].params.as_ref().expect("plan params");
            assert_eq!(params["threadId"], json!("thread-1"));
            assert_eq!(params["turnId"], json!("turn-1"));
            assert_eq!(params["explanation"], json!("adjust plan"));
            assert_eq!(
                params["plan"],
                json!([
                    { "step": "inspect bridge", "status": "completed" },
                    { "step": "wire event", "status": "inProgress" },
                ])
            );

            bridge.shutdown().await;
        }

        #[tokio::test]
        async fn patch_apply_events_emit_file_change_items() {
            let mut bridge = test_bridge();
            let changes = HashMap::from([(
                PathBuf::from("src/main.rs"),
                FileChange::Update {
                    unified_diff: "@@ -1 +1 @@".to_string(),
                    move_path: None,
                },
            )]);

            let started = bridge.file_change_started_notifications(
                "call-1",
                Some("turn-1".to_string()),
                Some("thread-1"),
                &changes,
            );
            assert_eq!(started.len(), 2);
            assert_eq!(started[0].method, "turn/started");
            assert_eq!(started[1].method, "item/started");
            let started_item = &started[1].params.as_ref().expect("started params")["item"];
            assert_eq!(started_item["type"], json!("fileChange"));
            assert_eq!(started_item["status"], json!("inProgress"));
            assert_eq!(started_item["changes"][0]["path"], json!("src/main.rs"));
            assert_eq!(started_item["changes"][0]["kind"]["type"], json!("update"));

            let duplicate_started = bridge.file_change_started_notifications(
                "call-1",
                Some("turn-1".to_string()),
                Some("thread-1"),
                &changes,
            );
            assert!(duplicate_started.is_empty());

            let completed = bridge.file_change_completed_notifications(
                "call-1",
                Some("turn-1".to_string()),
                Some("thread-1"),
                &changes,
                true,
            );
            assert_eq!(completed.len(), 1);
            assert_eq!(completed[0].method, "item/completed");
            let completed_item = &completed[0].params.as_ref().expect("completed params")["item"];
            assert_eq!(completed_item["type"], json!("fileChange"));
            assert_eq!(completed_item["status"], json!("completed"));
            assert!(bridge.file_change_started.is_empty());

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
