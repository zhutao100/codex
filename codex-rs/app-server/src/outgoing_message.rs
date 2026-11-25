use std::collections::HashMap;
use std::sync::atomic::AtomicI64;
use std::sync::atomic::Ordering;

use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::Result;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::ServerRequest;
use codex_app_server_protocol::ServerRequestPayload;
use codex_codexd::producer::CodexdProducerClient;
use codex_codexd::protocol::HubNotification;
use serde::Serialize;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tracing::warn;

use crate::error_code::INTERNAL_ERROR_CODE;

#[cfg(test)]
use codex_protocol::account::PlanType;

/// Stable identifier for a transport connection.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct ConnectionId(pub(crate) u64);

/// Stable identifier for a client request scoped to a transport connection.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct ConnectionRequestId {
    pub(crate) connection_id: ConnectionId,
    pub(crate) request_id: RequestId,
}

#[derive(Debug, Clone)]
pub(crate) enum OutgoingEnvelope {
    ToConnection {
        connection_id: ConnectionId,
        message: OutgoingMessage,
    },
    Broadcast {
        message: OutgoingMessage,
    },
}

/// Sends messages to the client and manages request callbacks.
#[derive(Clone)]
pub(crate) struct OutgoingMessageSender {
    sender: mpsc::Sender<OutgoingEnvelope>,
    requests: Arc<RequestCallbacks>,
    codexd_producer: Option<CodexdProducerClient>,
    delegate_route: Option<(String, String)>,
}

#[derive(Default)]
struct RequestCallbacks {
    next_id: AtomicI64,
    pending: Mutex<HashMap<RequestId, oneshot::Sender<Result>>>,
}

impl OutgoingMessageSender {
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn new(sender: mpsc::Sender<OutgoingEnvelope>) -> Self {
        Self::new_with_codexd(sender, None)
    }

    pub(crate) fn new_with_codexd(
        sender: mpsc::Sender<OutgoingEnvelope>,
        codexd_producer: Option<CodexdProducerClient>,
    ) -> Self {
        Self {
            sender,
            requests: Arc::default(),
            codexd_producer,
            delegate_route: None,
        }
    }

    pub(crate) async fn send_request(
        &self,
        request: ServerRequestPayload,
    ) -> oneshot::Receiver<Result> {
        let (_id, rx) = self.send_request_with_id(request).await;
        rx
    }

    pub(crate) async fn send_request_with_id(
        &self,
        request: ServerRequestPayload,
    ) -> (RequestId, oneshot::Receiver<Result>) {
        let id = RequestId::Integer(self.requests.next_id.fetch_add(1, Ordering::Relaxed));
        let outgoing_message_id = id.clone();
        let (tx_approve, rx_approve) = oneshot::channel();
        {
            let mut request_id_to_callback = self.requests.pending.lock().await;
            request_id_to_callback.insert(id, tx_approve);
        }

        let outgoing_message =
            OutgoingMessage::Request(request.request_with_id(outgoing_message_id.clone()));
        if let Err(err) = self
            .sender
            .send(OutgoingEnvelope::Broadcast {
                message: outgoing_message,
            })
            .await
        {
            warn!("failed to send request {outgoing_message_id:?} to client: {err:?}");
            let mut request_id_to_callback = self.requests.pending.lock().await;
            request_id_to_callback.remove(&outgoing_message_id);
        }
        (outgoing_message_id, rx_approve)
    }

    pub(crate) async fn notify_client_response(&self, id: RequestId, result: Result) {
        let entry = {
            let mut request_id_to_callback = self.requests.pending.lock().await;
            request_id_to_callback.remove_entry(&id)
        };

        match entry {
            Some((id, sender)) => {
                if let Err(err) = sender.send(result) {
                    warn!("could not notify callback for {id:?} due to: {err:?}");
                }
            }
            None => {
                warn!("could not find callback for {id:?}");
            }
        }
    }

    pub(crate) async fn notify_client_error(&self, id: RequestId, error: JSONRPCErrorError) {
        let entry = {
            let mut request_id_to_callback = self.requests.pending.lock().await;
            request_id_to_callback.remove_entry(&id)
        };

        match entry {
            Some((id, _sender)) => {
                warn!("client responded with error for {id:?}: {error:?}");
            }
            None => {
                warn!("could not find callback for {id:?}");
            }
        }
    }

    pub(crate) async fn cancel_request(&self, id: &RequestId) -> bool {
        let entry = {
            let mut request_id_to_callback = self.requests.pending.lock().await;
            request_id_to_callback.remove_entry(id)
        };
        entry.is_some()
    }

    pub(crate) async fn send_response<T: Serialize>(
        &self,
        request_id: ConnectionRequestId,
        response: T,
    ) {
        match serde_json::to_value(response) {
            Ok(result) => {
                let outgoing_message = OutgoingMessage::Response(OutgoingResponse {
                    id: request_id.request_id,
                    result,
                });
                if let Err(err) = self
                    .sender
                    .send(OutgoingEnvelope::ToConnection {
                        connection_id: request_id.connection_id,
                        message: outgoing_message,
                    })
                    .await
                {
                    warn!("failed to send response to client: {err:?}");
                }
            }
            Err(err) => {
                self.send_error(
                    request_id,
                    JSONRPCErrorError {
                        code: INTERNAL_ERROR_CODE,
                        message: format!("failed to serialize response: {err}"),
                        data: None,
                    },
                )
                .await;
            }
        }
    }

    pub(crate) async fn send_server_notification(&self, notification: ServerNotification) {
        if should_publish_to_codexd(&notification)
            && let Some(codexd_producer) = &self.codexd_producer
            && let Some(mut hub) = HubNotification::from_server(&notification)
        {
            if let Some((parent, child)) = &self.delegate_route
                && let Some(params) = hub.params.as_mut()
                && params["threadId"].as_str() == Some(parent)
                && let Some(turn) = params["turnId"].as_str()
            {
                let key = format!("{child}:{turn}");
                params["threadId"] = serde_json::json!(child);
                params["turnKey"] = serde_json::json!(key);
            }
            codexd_producer.publish_hub_notification(hub).await;
        }

        if let Err(err) = self
            .sender
            .send(OutgoingEnvelope::Broadcast {
                message: OutgoingMessage::AppServerNotification(notification),
            })
            .await
        {
            warn!("failed to send server notification to client: {err:?}");
        }
    }

    /// Scope only the daemon copy of notifications emitted for this event.
    /// Public v2 clients still receive the original thread and typed payload.
    pub(crate) fn for_delegate(
        &self,
        parent: codex_protocol::ThreadId,
        child: codex_protocol::ThreadId,
    ) -> Self {
        Self {
            delegate_route: Some((parent.to_string(), child.to_string())),
            ..self.clone()
        }
    }

    pub(crate) async fn publish_delegate_context(
        &self,
        context: &codex_protocol::protocol::RuntimeContextSnapshot,
        turn_id: &str,
    ) {
        let Some(producer) = &self.codexd_producer else {
            return;
        };
        let key = format!("{}:{turn_id}", context.session_id);
        producer.publish_hub_notification(HubNotification {
            method: "turn/contextUpdated".into(),
            params: Some(serde_json::json!({
                "threadId":context.session_id, "turnId":turn_id, "turnKey":key,
                "scope":"delegate","taskKind":context.task_kind, "sessionSource":context.session_source.to_string(),
                "parentThreadId":context.parent_session_id, "parentTurnId":context.parent_turn_id,
                "model":context.model,"modelProvider":context.model_provider_id,"cwd":context.cwd,
                "thinkingLevel":context.reasoning_effort,"modelContextWindow":context.model_context_window,"threadName":context.thread_name
            })),
        }).await;
    }

    pub(crate) async fn publish_delegate_usage(
        &self,
        thread: codex_protocol::ThreadId,
        turn: &str,
        usage: codex_protocol::protocol::TokenUsageInfo,
    ) {
        if let Some(producer) = &self.codexd_producer {
            producer
                .publish_server_notification(&ServerNotification::ThreadTokenUsageUpdated(
                    codex_app_server_protocol::ThreadTokenUsageUpdatedNotification {
                        thread_id: thread.to_string(),
                        turn_id: turn.to_owned(),
                        token_usage: usage.into(),
                    },
                ))
                .await;
        }
    }

    pub(crate) async fn publish_core_turn_started(
        &self,
        thread_id: codex_protocol::ThreadId,
        turn_id: &str,
        model: &str,
        context_window: Option<i64>,
    ) {
        if let Some(producer) = &self.codexd_producer {
            producer.publish_hub_notification(codex_codexd::protocol::HubNotification {
                method: "turn/started".into(),
                params: Some(serde_json::json!({"threadId":thread_id, "turn":{
                    "id":turn_id, "status":"inProgress", "model":model, "modelContextWindow":context_window
                }})),
            }).await;
        }
    }

    pub(crate) async fn publish_delegate_turn_ended(
        &self,
        thread_id: codex_protocol::ThreadId,
        turn_id: &str,
        status: codex_protocol::protocol::TerminalTurnStatus,
    ) {
        if let Some(producer) = &self.codexd_producer {
            producer
                .publish_hub_notification(codex_codexd::protocol::HubNotification {
                    method: "turn/completed".into(),
                    params: Some(serde_json::json!({"threadId":thread_id, "turn":{"id":turn_id, "status":status}})),
                })
                .await;
        }
    }

    /// All notifications should be migrated to [`ServerNotification`] and
    /// [`OutgoingMessage::Notification`] should be removed.
    pub(crate) async fn send_notification(&self, notification: OutgoingNotification) {
        let outgoing_message = OutgoingMessage::Notification(notification);
        if let Err(err) = self
            .sender
            .send(OutgoingEnvelope::Broadcast {
                message: outgoing_message,
            })
            .await
        {
            warn!("failed to send notification to client: {err:?}");
        }
    }

    pub(crate) async fn send_error(
        &self,
        request_id: ConnectionRequestId,
        error: JSONRPCErrorError,
    ) {
        let outgoing_message = OutgoingMessage::Error(OutgoingError {
            id: request_id.request_id,
            error,
        });
        if let Err(err) = self
            .sender
            .send(OutgoingEnvelope::ToConnection {
                connection_id: request_id.connection_id,
                message: outgoing_message,
            })
            .await
        {
            warn!("failed to send error to client: {err:?}");
        }
    }
}

fn should_publish_to_codexd(notification: &ServerNotification) -> bool {
    matches!(
        notification,
        ServerNotification::ThreadStarted(_)
            | ServerNotification::ThreadNameUpdated(_)
            | ServerNotification::ThreadTokenUsageUpdated(_)
            | ServerNotification::TurnCompleted(_)
            | ServerNotification::TurnDiffUpdated(_)
            | ServerNotification::TurnPlanUpdated(_)
            | ServerNotification::TurnProgressTrace(_)
            | ServerNotification::ModelRerouted(_)
            | ServerNotification::ModelVerification(_)
            | ServerNotification::ItemStarted(_)
            | ServerNotification::ItemCompleted(_)
            | ServerNotification::AgentMessageDelta(_)
            | ServerNotification::PlanDelta(_)
            | ServerNotification::CommandExecutionOutputDelta(_)
            | ServerNotification::TerminalInteraction(_)
            | ServerNotification::FileChangeOutputDelta(_)
            | ServerNotification::McpToolCallProgress(_)
            | ServerNotification::AccountUpdated(_)
            | ServerNotification::ReasoningSummaryTextDelta(_)
            | ServerNotification::ReasoningSummaryPartAdded(_)
            | ServerNotification::ReasoningTextDelta(_)
            | ServerNotification::Error(_)
            | ServerNotification::AccountRateLimitsUpdated(_)
            | ServerNotification::ContextCompacted(_)
            | ServerNotification::ConfigWarning(_)
            | ServerNotification::AccountLoginCompleted(_)
    )
}

/// Outgoing message from the server to the client.
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub(crate) enum OutgoingMessage {
    Request(ServerRequest),
    Notification(OutgoingNotification),
    /// AppServerNotification is specific to the case where this is run as an
    /// "app server" as opposed to an MCP server.
    AppServerNotification(ServerNotification),
    Response(OutgoingResponse),
    Error(OutgoingError),
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct OutgoingNotification {
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct OutgoingResponse {
    pub id: RequestId,
    pub result: Result,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct OutgoingError {
    pub error: JSONRPCErrorError,
    pub id: RequestId,
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use codex_app_server_protocol::AccountLoginCompletedNotification;
    use codex_app_server_protocol::AccountRateLimitsUpdatedNotification;
    use codex_app_server_protocol::AccountUpdatedNotification;
    use codex_app_server_protocol::AuthMode;
    use codex_app_server_protocol::ConfigWarningNotification;
    use codex_app_server_protocol::LoginChatGptCompleteNotification;
    use codex_app_server_protocol::RateLimitSnapshot;
    use codex_app_server_protocol::RateLimitWindow;
    use pretty_assertions::assert_eq;
    use serde_json::json;
    use tokio::time::timeout;
    use uuid::Uuid;

    use super::*;

    #[test]
    fn verify_server_notification_serialization() {
        let notification =
            ServerNotification::LoginChatGptComplete(LoginChatGptCompleteNotification {
                login_id: Uuid::nil(),
                success: true,
                error: None,
            });

        let jsonrpc_notification = OutgoingMessage::AppServerNotification(notification);
        assert_eq!(
            json!({
                "method": "loginChatGptComplete",
                "params": {
                    "loginId": Uuid::nil(),
                    "success": true,
                    "error": null,
                },
            }),
            serde_json::to_value(jsonrpc_notification)
                .expect("ensure the strum macros serialize the method field correctly"),
            "ensure the strum macros serialize the method field correctly"
        );
    }

    #[test]
    fn verify_account_login_completed_notification_serialization() {
        let notification =
            ServerNotification::AccountLoginCompleted(AccountLoginCompletedNotification {
                login_id: Some(Uuid::nil().to_string()),
                success: true,
                error: None,
            });

        let jsonrpc_notification = OutgoingMessage::AppServerNotification(notification);
        assert_eq!(
            json!({
                "method": "account/login/completed",
                "params": {
                    "loginId": Uuid::nil().to_string(),
                    "success": true,
                    "error": null,
                },
            }),
            serde_json::to_value(jsonrpc_notification)
                .expect("ensure the notification serializes correctly"),
            "ensure the notification serializes correctly"
        );
    }

    #[test]
    fn verify_account_rate_limits_notification_serialization() {
        let notification =
            ServerNotification::AccountRateLimitsUpdated(AccountRateLimitsUpdatedNotification {
                rate_limits: RateLimitSnapshot {
                    primary: Some(RateLimitWindow {
                        used_percent: 25,
                        window_duration_mins: Some(15),
                        resets_at: Some(123),
                    }),
                    secondary: None,
                    credits: None,
                    plan_type: Some(PlanType::Plus),
                },
            });

        let jsonrpc_notification = OutgoingMessage::AppServerNotification(notification);
        assert_eq!(
            json!({
                "method": "account/rateLimits/updated",
                "params": {
                    "rateLimits": {
                        "primary": {
                            "usedPercent": 25,
                            "windowDurationMins": 15,
                            "resetsAt": 123
                        },
                        "secondary": null,
                        "credits": null,
                        "planType": "plus"
                    }
                },
            }),
            serde_json::to_value(jsonrpc_notification)
                .expect("ensure the notification serializes correctly"),
            "ensure the notification serializes correctly"
        );
    }

    #[test]
    fn verify_account_updated_notification_serialization() {
        let notification = ServerNotification::AccountUpdated(AccountUpdatedNotification {
            auth_mode: Some(AuthMode::ApiKey),
        });

        let jsonrpc_notification = OutgoingMessage::AppServerNotification(notification);
        assert_eq!(
            json!({
                "method": "account/updated",
                "params": {
                    "authMode": "apikey"
                },
            }),
            serde_json::to_value(jsonrpc_notification)
                .expect("ensure the notification serializes correctly"),
            "ensure the notification serializes correctly"
        );
    }

    #[test]
    fn verify_config_warning_notification_serialization() {
        let notification = ServerNotification::ConfigWarning(ConfigWarningNotification {
            summary: "Config error: using defaults".to_string(),
            details: Some("error loading config: bad config".to_string()),
            path: None,
            range: None,
        });

        let jsonrpc_notification = OutgoingMessage::AppServerNotification(notification);
        assert_eq!(
            json!( {
                "method": "configWarning",
                "params": {
                    "summary": "Config error: using defaults",
                    "details": "error loading config: bad config",
                },
            }),
            serde_json::to_value(jsonrpc_notification)
                .expect("ensure the notification serializes correctly"),
            "ensure the notification serializes correctly"
        );
    }

    #[tokio::test]
    async fn send_response_routes_to_target_connection() {
        let (tx, mut rx) = mpsc::channel::<OutgoingEnvelope>(4);
        let outgoing = OutgoingMessageSender::new(tx);
        let request_id = ConnectionRequestId {
            connection_id: ConnectionId(42),
            request_id: RequestId::Integer(7),
        };

        outgoing
            .send_response(request_id.clone(), json!({ "ok": true }))
            .await;

        let envelope = timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("should receive envelope before timeout")
            .expect("channel should contain one message");

        match envelope {
            OutgoingEnvelope::ToConnection {
                connection_id,
                message,
            } => {
                assert_eq!(connection_id, ConnectionId(42));
                let OutgoingMessage::Response(response) = message else {
                    panic!("expected response message");
                };
                assert_eq!(response.id, request_id.request_id);
                assert_eq!(response.result, json!({ "ok": true }));
            }
            other => panic!("expected targeted response envelope, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn send_error_routes_to_target_connection() {
        let (tx, mut rx) = mpsc::channel::<OutgoingEnvelope>(4);
        let outgoing = OutgoingMessageSender::new(tx);
        let request_id = ConnectionRequestId {
            connection_id: ConnectionId(9),
            request_id: RequestId::Integer(3),
        };
        let error = JSONRPCErrorError {
            code: INTERNAL_ERROR_CODE,
            message: "boom".to_string(),
            data: None,
        };

        outgoing.send_error(request_id.clone(), error.clone()).await;

        let envelope = timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("should receive envelope before timeout")
            .expect("channel should contain one message");

        match envelope {
            OutgoingEnvelope::ToConnection {
                connection_id,
                message,
            } => {
                assert_eq!(connection_id, ConnectionId(9));
                let OutgoingMessage::Error(outgoing_error) = message else {
                    panic!("expected error message");
                };
                assert_eq!(outgoing_error.id, RequestId::Integer(3));
                assert_eq!(outgoing_error.error, error);
            }
            other => panic!("expected targeted error envelope, got: {other:?}"),
        }
    }
}
