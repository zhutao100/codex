use crate::auth::AuthProvider;
use crate::auth::add_auth_headers_to_header_map;
use crate::common::ResponseEvent;
use crate::common::ResponseStream;
use crate::common::ResponsesWsRequest;
use crate::error::ApiError;
use crate::provider::Provider;
use crate::rate_limits::parse_rate_limit_event;
use crate::sse::responses::ResponsesStreamEvent;
use crate::sse::responses::process_responses_event;
use crate::telemetry::WebsocketTelemetry;
use codex_client::TransportError;
use http::HeaderMap;
use http::HeaderName;
use http::HeaderValue;
use http::StatusCode;
use serde::Deserialize;
use serde_json::Value;
use serde_json::map::Map as JsonMap;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_tungstenite::connect_async_tls_with_config;
use tokio_tungstenite::tungstenite::Error as WsError;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tracing::debug;
use tracing::error;
use tracing::info;
use tracing::trace;
use tungstenite::extensions::ExtensionsConfig;
use tungstenite::extensions::compression::deflate::DeflateConfig;
use tungstenite::protocol::WebSocketConfig;
use url::Url;

mod pump;
use pump::WsSendOutcome;
use pump::WsStream;

const X_CODEX_TURN_STATE_HEADER: &str = "x-codex-turn-state";
const X_MODELS_ETAG_HEADER: &str = "x-models-etag";
const X_REASONING_INCLUDED_HEADER: &str = "x-reasoning-included";
const OPENAI_MODEL_HEADER: &str = "openai-model";
const WEBSOCKET_CONNECTION_LIMIT_REACHED_CODE: &str = "websocket_connection_limit_reached";
const PREVIOUS_RESPONSE_NOT_FOUND_CODE: &str = "previous_response_not_found";
const WEBSOCKET_CONNECTION_LIMIT_REACHED_MESSAGE: &str = "Responses websocket connection limit reached (60 minutes). Create a new websocket connection to continue.";

pub struct ResponsesWebsocketConnection {
    stream: Arc<Mutex<Option<WsStream>>>,
    // TODO (pakrym): is this the right place for timeout?
    idle_timeout: Duration,
    server_reasoning_included: bool,
    models_etag: Option<String>,
    server_model: Option<String>,
    telemetry: Option<Arc<dyn WebsocketTelemetry>>,
}

impl ResponsesWebsocketConnection {
    fn new(
        stream: WsStream,
        idle_timeout: Duration,
        server_reasoning_included: bool,
        models_etag: Option<String>,
        server_model: Option<String>,
        telemetry: Option<Arc<dyn WebsocketTelemetry>>,
    ) -> Self {
        Self {
            stream: Arc::new(Mutex::new(Some(stream))),
            idle_timeout,
            server_reasoning_included,
            models_etag,
            server_model,
            telemetry,
        }
    }

    pub async fn is_closed(&self) -> bool {
        let mut stream = self.stream.lock().await;
        let closed = stream.as_ref().is_none_or(WsStream::is_closed);
        if closed {
            stream.take();
        }
        closed
    }

    pub async fn stream_request(
        &self,
        request: ResponsesWsRequest,
        turn_state: Option<Arc<OnceLock<String>>>,
    ) -> Result<ResponseStream, ApiError> {
        let (tx_event, rx_event) =
            mpsc::channel::<std::result::Result<ResponseEvent, ApiError>>(1600);
        let stream = Arc::clone(&self.stream);
        let idle_timeout = self.idle_timeout;
        let server_reasoning_included = self.server_reasoning_included;
        let models_etag = self.models_etag.clone();
        let server_model = self.server_model.clone();
        let telemetry = self.telemetry.clone();
        let request_text = serialize_websocket_request(&request)?;

        tokio::spawn(async move {
            if let Some(model) = server_model {
                let _ = tx_event.send(Ok(ResponseEvent::ServerModel(model))).await;
            }
            if let Some(etag) = models_etag {
                let _ = tx_event.send(Ok(ResponseEvent::ModelsEtag(etag))).await;
            }
            if server_reasoning_included {
                let _ = tx_event
                    .send(Ok(ResponseEvent::ServerReasoningIncluded(true)))
                    .await;
            }
            let mut guard = stream.lock().await;
            let result = {
                let Some(ws_stream) = guard.as_mut() else {
                    let _ = tx_event
                        .send(Err(ApiError::Stream(
                            "websocket connection is closed".to_string(),
                        )))
                        .await;
                    return;
                };

                run_websocket_response_stream(
                    ws_stream,
                    tx_event.clone(),
                    request_text,
                    idle_timeout,
                    telemetry,
                    turn_state.as_deref(),
                )
                .await
            };

            if let Err(err) = result {
                // Surface terminal stream errors immediately. Waiting for a graceful close
                // handshake here can stall indefinitely and hide the original failure.
                let failed_stream = guard.take();
                drop(guard);
                drop(failed_stream);
                let _ = tx_event.send(Err(err)).await;
            }
        });

        Ok(ResponseStream { rx_event })
    }
}

pub struct ResponsesWebsocketClient<A: AuthProvider> {
    provider: Provider,
    auth: A,
}

impl<A: AuthProvider> ResponsesWebsocketClient<A> {
    pub fn new(provider: Provider, auth: A) -> Self {
        Self { provider, auth }
    }

    pub async fn connect(
        &self,
        extra_headers: HeaderMap,
        default_headers: HeaderMap,
        turn_state: Option<Arc<OnceLock<String>>>,
        telemetry: Option<Arc<dyn WebsocketTelemetry>>,
    ) -> Result<ResponsesWebsocketConnection, ApiError> {
        let ws_url = self
            .provider
            .websocket_url_for_path("responses")
            .map_err(|err| ApiError::Stream(format!("failed to build websocket URL: {err}")))?;

        let mut headers =
            merge_request_headers(&self.provider.headers, extra_headers, default_headers);
        add_auth_headers_to_header_map(&self.auth, &mut headers);

        let (stream, server_reasoning_included, models_etag, server_model) =
            connect_websocket(ws_url, headers, turn_state.clone()).await?;
        Ok(ResponsesWebsocketConnection::new(
            stream,
            self.provider.stream_idle_timeout,
            server_reasoning_included,
            models_etag,
            server_model,
            telemetry,
        ))
    }
}

fn merge_request_headers(
    provider_headers: &HeaderMap,
    extra_headers: HeaderMap,
    default_headers: HeaderMap,
) -> HeaderMap {
    let mut headers = provider_headers.clone();
    headers.extend(extra_headers);
    for (name, value) in &default_headers {
        if let http::header::Entry::Vacant(entry) = headers.entry(name) {
            entry.insert(value.clone());
        }
    }
    headers
}

async fn connect_websocket(
    url: Url,
    headers: HeaderMap,
    turn_state: Option<Arc<OnceLock<String>>>,
) -> Result<(WsStream, bool, Option<String>, Option<String>), ApiError> {
    info!("connecting to websocket: {url}");

    let mut request = url
        .as_str()
        .into_client_request()
        .map_err(|err| ApiError::Stream(format!("failed to build websocket request: {err}")))?;
    request.headers_mut().extend(headers);

    let response = connect_async_tls_with_config(
        request,
        Some(websocket_config()),
        false, // `false` means "do not disable Nagle", which is tungstenite's recommended default.
        None,
    )
    .await;

    let (stream, response) = match response {
        Ok((stream, response)) => {
            info!(
                "successfully connected to websocket: {url}, headers: {:?}",
                response.headers()
            );
            (stream, response)
        }
        Err(err) => {
            error!("failed to connect to websocket: {err}, url: {url}");
            return Err(map_ws_error(err, &url));
        }
    };

    let reasoning_included = response.headers().contains_key(X_REASONING_INCLUDED_HEADER);
    let models_etag = response
        .headers()
        .get(X_MODELS_ETAG_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(ToString::to_string);
    let server_model = response
        .headers()
        .get(OPENAI_MODEL_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(ToString::to_string);
    if let Some(turn_state) = turn_state
        && let Some(header_value) = response
            .headers()
            .get(X_CODEX_TURN_STATE_HEADER)
            .and_then(|value| value.to_str().ok())
    {
        let _ = turn_state.set(header_value.to_string());
    }
    Ok((
        WsStream::new(stream),
        reasoning_included,
        models_etag,
        server_model,
    ))
}

fn websocket_config() -> WebSocketConfig {
    let mut extensions = ExtensionsConfig::default();
    extensions.permessage_deflate = Some(DeflateConfig::default());

    let mut config = WebSocketConfig::default();
    config.extensions = extensions;
    config
}

fn map_ws_error(err: WsError, url: &Url) -> ApiError {
    match err {
        WsError::Http(response) => {
            let status = response.status();
            let headers = response.headers().clone();
            let body = response
                .body()
                .as_ref()
                .and_then(|bytes| String::from_utf8(bytes.clone()).ok());
            ApiError::Transport(TransportError::Http {
                status,
                url: Some(url.to_string()),
                headers: Some(headers),
                body,
            })
        }
        WsError::ConnectionClosed | WsError::AlreadyClosed => {
            ApiError::Stream("websocket closed".to_string())
        }
        WsError::Io(err) => ApiError::Transport(TransportError::Network(err.to_string())),
        other => ApiError::Transport(TransportError::Network(other.to_string())),
    }
}

#[derive(Debug, Deserialize)]
struct WrappedWebsocketError {
    code: Option<String>,
    message: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WrappedWebsocketErrorEvent {
    #[serde(rename = "type")]
    kind: String,
    #[serde(alias = "status_code")]
    status: Option<u16>,
    #[serde(default)]
    error: Option<WrappedWebsocketError>,
    #[serde(default)]
    headers: Option<JsonMap<String, Value>>,
}

fn parse_wrapped_websocket_error_event(payload: &str) -> Option<WrappedWebsocketErrorEvent> {
    let event: WrappedWebsocketErrorEvent = serde_json::from_str(payload).ok()?;
    if event.kind != "error" {
        return None;
    }
    Some(event)
}

fn map_wrapped_websocket_error_event(
    event: WrappedWebsocketErrorEvent,
    original_payload: String,
) -> Option<ApiError> {
    let WrappedWebsocketErrorEvent {
        status,
        error,
        headers,
        ..
    } = event;

    if let Some(error) = error.as_ref()
        && let Some(code) = error.code.as_deref()
        && let Some(fallback) = match code {
            WEBSOCKET_CONNECTION_LIMIT_REACHED_CODE => {
                Some(WEBSOCKET_CONNECTION_LIMIT_REACHED_MESSAGE)
            }
            PREVIOUS_RESPONSE_NOT_FOUND_CODE => {
                Some("Previous response was not found. Retrying the full request.")
            }
            _ => None,
        }
    {
        return Some(ApiError::Retryable {
            message: error
                .message
                .clone()
                .unwrap_or_else(|| fallback.to_string()),
            delay: None,
        });
    }

    let status = StatusCode::from_u16(status?).ok()?;
    if status.is_success() {
        return None;
    }

    Some(ApiError::Transport(TransportError::Http {
        status,
        url: None,
        headers: headers.map(json_headers_to_http_headers),
        body: Some(original_payload),
    }))
}

fn json_headers_to_http_headers(headers: JsonMap<String, Value>) -> HeaderMap {
    let mut mapped = HeaderMap::new();
    for (name, value) in headers {
        let Ok(header_name) = HeaderName::from_bytes(name.as_bytes()) else {
            continue;
        };
        let Some(header_value) = json_header_value(value) else {
            continue;
        };
        mapped.insert(header_name, header_value);
    }
    mapped
}

fn json_header_value(value: Value) -> Option<HeaderValue> {
    let value = match value {
        Value::String(value) => value,
        Value::Number(value) => value.to_string(),
        Value::Bool(value) => value.to_string(),
        _ => return None,
    };
    HeaderValue::from_str(&value).ok()
}

async fn run_websocket_response_stream(
    ws_stream: &mut WsStream,
    tx_event: mpsc::Sender<std::result::Result<ResponseEvent, ApiError>>,
    request_text: String,
    idle_timeout: Duration,
    telemetry: Option<Arc<dyn WebsocketTelemetry>>,
    turn_state: Option<&OnceLock<String>>,
) -> Result<(), ApiError> {
    let mut last_server_model: Option<String> = None;
    trace!("websocket request: {request_text}");
    let start = Instant::now();
    let outcome = ws_stream.send(request_text, idle_timeout).await;
    if let Some(telemetry) = &telemetry {
        let error = match &outcome {
            WsSendOutcome::Flushed => None,
            WsSendOutcome::NotStarted(error) | WsSendOutcome::Interrupted(error) => Some(error),
        };
        telemetry.on_ws_request(start.elapsed(), error);
    }
    let send_error = match outcome {
        WsSendOutcome::Flushed => None,
        WsSendOutcome::NotStarted(error) => return Err(error),
        WsSendOutcome::Interrupted(error) => Some(error),
    };

    loop {
        let poll_start = Instant::now();
        let response = tokio::time::timeout(idle_timeout, ws_stream.next())
            .await
            .map_err(|_| ApiError::Stream("idle timeout waiting for websocket".into()));
        if let Some(t) = telemetry.as_ref() {
            t.on_ws_event(&response, poll_start.elapsed());
        }
        let message = match response {
            Ok(Some(Ok(msg))) => msg,
            Ok(Some(Err(err))) => {
                return Err(ApiError::Stream(err.to_string()));
            }
            Ok(None) => {
                if let Some(error) = send_error {
                    return Err(error);
                }
                return Err(ApiError::Stream(
                    "stream closed before response.completed".into(),
                ));
            }
            Err(err) => {
                return Err(err);
            }
        };

        match message {
            Message::Text(text) => {
                trace!("websocket event: {text}");
                if let Some(wrapped_error) = parse_wrapped_websocket_error_event(&text)
                    && let Some(error) =
                        map_wrapped_websocket_error_event(wrapped_error, text.to_string())
                {
                    return Err(error);
                }

                let event = match serde_json::from_str::<ResponsesStreamEvent>(&text) {
                    Ok(event) => event,
                    Err(err) => {
                        debug!("failed to parse websocket event: {err}, data: {text}");
                        continue;
                    }
                };
                if let Some(response_turn_state) = event.turn_state()
                    && let Some(turn_state) = turn_state
                {
                    let _ = turn_state.set(response_turn_state);
                }
                if event.kind() == "codex.rate_limits" {
                    if let Some(snapshot) = parse_rate_limit_event(&text) {
                        let _ = tx_event.send(Ok(ResponseEvent::RateLimits(snapshot))).await;
                    }
                    continue;
                }
                let model_verifications = event.model_verifications();
                if let Some(model) = event.response_model()
                    && last_server_model.as_deref() != Some(model.as_str())
                {
                    let _ = tx_event
                        .send(Ok(ResponseEvent::ServerModel(model.clone())))
                        .await;
                    last_server_model = Some(model);
                }
                if let Some(verifications) = model_verifications
                    && tx_event
                        .send(Ok(ResponseEvent::ModelVerifications(verifications)))
                        .await
                        .is_err()
                {
                    return Err(ApiError::Stream(
                        "response event consumer dropped".to_string(),
                    ));
                }
                match process_responses_event(event) {
                    Ok(Some(event)) => {
                        let is_completed = matches!(event, ResponseEvent::Completed { .. });
                        let _ = tx_event.send(Ok(event)).await;
                        if is_completed {
                            break;
                        }
                    }
                    Ok(None) => {}
                    Err(error) => {
                        return Err(error.into_api_error());
                    }
                }
            }
            Message::Binary(_) => {
                return Err(ApiError::Stream("unexpected binary websocket event".into()));
            }
            Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => {}
            Message::Close(_) => {
                return Err(ApiError::Stream(
                    "websocket closed by server before response.completed".into(),
                ));
            }
        }
    }

    Ok(())
}

fn serialize_websocket_request(request: &ResponsesWsRequest) -> Result<String, ApiError> {
    serde_json::to_string(request)
        .map_err(|err| ApiError::Stream(format!("failed to encode websocket request: {err}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::OpenAiVerbosity;
    use crate::common::Reasoning;
    use crate::common::ResponseCreateWsRequest;
    use crate::common::TextControls;
    use codex_protocol::config_types::ReasoningSummary as ReasoningSummaryConfig;
    use codex_protocol::models::ContentItem;
    use codex_protocol::models::ResponseItem;
    use codex_protocol::openai_models::ReasoningEffort as ReasoningEffortConfig;
    use pretty_assertions::assert_eq;
    use serde_json::json;
    use std::collections::HashMap;

    #[test]
    fn direct_serialization_preserves_websocket_request_payload() {
        let request = ResponsesWsRequest::ResponseCreate(ResponseCreateWsRequest {
            model: "gpt-test".to_string(),
            previous_response_id: Some("resp-1".to_string()),
            input: vec![ResponseItem::Message {
                encrypted_content: None,
                status: None,
                id: Some("msg-1".to_string()),
                role: "user".to_string(),
                content: vec![ContentItem::InputText {
                    text: "hello".to_string(),
                }],
                end_turn: None,
                phase: None,
            }]
            .into(),
            tools: vec![json!({
                "type": "function",
                "name": "lookup",
                "parameters": {"type": "object"}
            })],
            tool_choice: "auto".to_string(),
            parallel_tool_calls: true,
            reasoning: Some(Reasoning {
                effort: Some(ReasoningEffortConfig::High),
                summary: Some(ReasoningSummaryConfig::Detailed),
            }),
            store: false,
            stream: true,
            include: vec!["reasoning.encrypted_content".to_string()],
            service_tier: Some("priority".to_string()),
            prompt_cache_key: Some("cache-key".to_string()),
            text: Some(TextControls {
                verbosity: Some(OpenAiVerbosity::High),
                format: None,
            }),
            generate: Some(false),
            client_metadata: Some(HashMap::from([
                (
                    "traceparent".to_string(),
                    "00-0123456789abcdef0123456789abcdef-0123456789abcdef-01".to_string(),
                ),
                ("x-codex-turn-state".to_string(), "turn-state-1".to_string()),
            ])),
        });

        let previous_payload = serde_json::to_value(&request).expect("serialize previous payload");
        let request_text =
            serialize_websocket_request(&request).expect("serialize websocket request");
        let wire_payload =
            serde_json::from_str::<Value>(&request_text).expect("parse websocket request");

        assert_eq!(wire_payload, previous_payload);
    }

    #[test]
    fn websocket_config_enables_permessage_deflate() {
        let config = websocket_config();
        assert!(config.extensions.permessage_deflate.is_some());
    }

    #[test]
    fn parse_wrapped_websocket_error_event_maps_to_transport_http() {
        let payload = json!({
            "type": "error",
            "status": 429,
            "error": {
                "type": "usage_limit_reached",
                "message": "The usage limit has been reached",
                "plan_type": "pro",
                "resets_at": 1738888888
            },
            "headers": {
                "x-codex-primary-used-percent": "100.0",
                "x-codex-primary-window-minutes": 15
            }
        })
        .to_string();

        let wrapped_error = parse_wrapped_websocket_error_event(&payload)
            .expect("expected websocket error payload to be parsed");
        let api_error = map_wrapped_websocket_error_event(wrapped_error, payload)
            .expect("expected websocket error payload to map to ApiError");

        let ApiError::Transport(TransportError::Http {
            status,
            headers,
            body,
            ..
        }) = api_error
        else {
            panic!("expected ApiError::Transport(Http)");
        };

        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        let headers = headers.expect("expected headers");
        assert_eq!(
            headers
                .get("x-codex-primary-used-percent")
                .and_then(|value| value.to_str().ok()),
            Some("100.0")
        );
        assert_eq!(
            headers
                .get("x-codex-primary-window-minutes")
                .and_then(|value| value.to_str().ok()),
            Some("15")
        );
        let body = body.expect("expected body");
        assert!(body.contains("usage_limit_reached"));
        assert!(body.contains("The usage limit has been reached"));
    }

    #[test]
    fn parse_wrapped_websocket_error_event_ignores_non_error_payloads() {
        let payload = json!({
            "type": "response.created",
            "response": {
                "id": "resp-1"
            }
        })
        .to_string();

        let wrapped_error = parse_wrapped_websocket_error_event(&payload);
        assert!(wrapped_error.is_none());
    }

    #[test]
    fn parse_wrapped_websocket_error_event_with_status_maps_invalid_request() {
        let payload = json!({
            "type": "error",
            "status": 400,
            "error": {
                "type": "invalid_request_error",
                "message": "Model does not support image inputs"
            }
        })
        .to_string();

        let wrapped_error = parse_wrapped_websocket_error_event(&payload)
            .expect("expected websocket error payload to be parsed");
        let api_error = map_wrapped_websocket_error_event(wrapped_error, payload)
            .expect("expected websocket error payload to map to ApiError");
        let ApiError::Transport(TransportError::Http { status, body, .. }) = api_error else {
            panic!("expected ApiError::Transport(Http)");
        };
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let body = body.expect("expected body");
        assert!(body.contains("invalid_request_error"));
        assert!(body.contains("Model does not support image inputs"));
    }

    #[test]
    fn parse_wrapped_websocket_error_event_with_connection_limit_maps_retryable() {
        let payload = json!({
            "type": "error",
            "status": 400,
            "error": {
                "type": "invalid_request_error",
                "code": "websocket_connection_limit_reached",
                "message": "Responses websocket connection limit reached (60 minutes). Create a new websocket connection to continue."
            }
        })
        .to_string();

        let wrapped_error = parse_wrapped_websocket_error_event(&payload)
            .expect("expected websocket error payload to be parsed");
        let api_error = map_wrapped_websocket_error_event(wrapped_error, payload)
            .expect("expected websocket error payload to map to ApiError");
        let ApiError::Retryable { message, delay } = api_error else {
            panic!("expected ApiError::Retryable");
        };
        assert_eq!(message, WEBSOCKET_CONNECTION_LIMIT_REACHED_MESSAGE);
        assert_eq!(delay, None);
    }

    #[test]
    fn parse_wrapped_websocket_error_event_without_status_is_not_mapped() {
        let payload = json!({
            "type": "error",
            "error": {
                "type": "usage_limit_reached",
                "message": "The usage limit has been reached"
            },
            "headers": {
                "x-codex-primary-used-percent": "100.0",
                "x-codex-primary-window-minutes": 15
            }
        })
        .to_string();

        let wrapped_error = parse_wrapped_websocket_error_event(&payload)
            .expect("expected websocket error payload to be parsed");
        let api_error = map_wrapped_websocket_error_event(wrapped_error, payload);
        assert!(api_error.is_none());
    }

    #[test]
    fn merge_request_headers_matches_http_precedence() {
        let mut provider_headers = HeaderMap::new();
        provider_headers.insert(
            "originator",
            HeaderValue::from_static("provider-originator"),
        );
        provider_headers.insert("x-priority", HeaderValue::from_static("provider"));

        let mut extra_headers = HeaderMap::new();
        extra_headers.insert("x-priority", HeaderValue::from_static("extra"));

        let mut default_headers = HeaderMap::new();
        default_headers.insert("originator", HeaderValue::from_static("default-originator"));
        default_headers.insert("x-priority", HeaderValue::from_static("default"));
        default_headers.insert("x-default-only", HeaderValue::from_static("default-only"));

        let merged = merge_request_headers(&provider_headers, extra_headers, default_headers);

        assert_eq!(
            merged.get("originator"),
            Some(&HeaderValue::from_static("provider-originator"))
        );
        assert_eq!(
            merged.get("x-priority"),
            Some(&HeaderValue::from_static("extra"))
        );
        assert_eq!(
            merged.get("x-default-only"),
            Some(&HeaderValue::from_static("default-only"))
        );
    }
}
