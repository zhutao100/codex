//! Session- and turn-scoped helpers for talking to model provider APIs.
//!
//! `ModelClient` is intended to live for the lifetime of a Codex session and holds the stable
//! configuration and state needed to talk to a provider (auth, provider selection, conversation id,
//! and feature-gated request behavior).
//!
//! Per-turn settings (model selection, reasoning controls, telemetry context, and turn metadata)
//! are passed explicitly to streaming and unary methods so that the turn lifetime is visible at the
//! call site.
//!
//! A [`ModelClientSession`] is created per turn and is used to stream one or more Responses API
//! requests during that turn. It caches a Responses WebSocket connection (opened lazily) and
//! stores per-turn state such as the `x-codex-turn-state` token used for sticky routing.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::OnceLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use crate::api_bridge::CoreAuthProvider;
use crate::api_bridge::map_api_error;
use crate::api_bridge::resolve_request_auth;
use crate::auth::AuthMode;
use crate::auth::UnauthorizedRecovery;
use codex_api::CompactClient as ApiCompactClient;
use codex_api::CompactionInput as ApiCompactionInput;
use codex_api::MemoriesClient as ApiMemoriesClient;
use codex_api::MemoryTrace as ApiMemoryTrace;
use codex_api::MemoryTraceSummarizeInput as ApiMemoryTraceSummarizeInput;
use codex_api::MemoryTraceSummaryOutput as ApiMemoryTraceSummaryOutput;
use codex_api::Prompt as ApiPrompt;
use codex_api::RequestTelemetry;
use codex_api::ReqwestTransport;
use codex_api::ResponseCreateWsRequest;
use codex_api::ResponsesClient as ApiResponsesClient;
use codex_api::ResponsesOptions as ApiResponsesOptions;
use codex_api::ResponsesWebsocketClient as ApiWebSocketResponsesClient;
use codex_api::ResponsesWebsocketConnection as ApiWebSocketConnection;
use codex_api::SseTelemetry;
use codex_api::TransportError;
use codex_api::WebsocketTelemetry;
use codex_api::build_conversation_headers;
use codex_api::common::Reasoning;
use codex_api::common::ResponsesWsRequest;
use codex_api::create_text_param_for_request;
use codex_api::error::ApiError;
use codex_api::requests::responses::Compression;
use codex_otel::OtelManager;

use codex_protocol::ThreadId;
use codex_protocol::config_types::ReasoningSummary as ReasoningSummaryConfig;
use codex_protocol::config_types::ServiceTier;
use codex_protocol::config_types::Verbosity as VerbosityConfig;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::openai_models::ReasoningEffort as ReasoningEffortConfig;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use eventsource_stream::Event;
use eventsource_stream::EventStreamError;
use futures::StreamExt;
use http::HeaderMap as ApiHeaderMap;
use http::HeaderValue;
use http::StatusCode as HttpStatusCode;
use reqwest::StatusCode;
use serde_json::Value;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::sync::oneshot::error::TryRecvError;
use tokio_tungstenite::tungstenite::Error;
use tokio_tungstenite::tungstenite::Message;
use tracing::warn;

use crate::AuthManager;
use crate::auth::RefreshTokenError;
use crate::client_common::Prompt;
use crate::client_common::ResponseEvent;
use crate::client_common::ResponseStream;
use crate::default_client::build_reqwest_client;
use crate::default_client::default_headers;
use crate::error::CodexErr;
use crate::error::Result;
use crate::flags::CODEX_RS_SSE_FIXTURE;
use crate::model_provider_info::ModelProviderInfo;
use crate::model_provider_info::WireApi;
use crate::tools::spec::create_tools_json_for_responses_api;

pub const X_CODEX_TURN_STATE_HEADER: &str = "x-codex-turn-state";
pub const X_CODEX_TURN_METADATA_HEADER: &str = "x-codex-turn-metadata";
pub const X_CODEX_PARENT_THREAD_ID_HEADER: &str = "x-codex-parent-thread-id";
pub const X_OPENAI_SUBAGENT_HEADER: &str = "x-openai-subagent";
pub const X_RESPONSESAPI_INCLUDE_TIMING_METRICS_HEADER: &str =
    "x-responsesapi-include-timing-metrics";
const OPENAI_BETA_HEADER: &str = "OpenAI-Beta";
const RESPONSES_WEBSOCKETS_V2_BETA_HEADER_VALUE: &str = "responses_websockets=2026-02-06";
const DEFAULT_WEBSOCKET_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Session-scoped state shared by all [`ModelClient`] clones.
///
/// This is intentionally kept minimal so `ModelClient` does not need to hold a full `Config`. Most
/// configuration is per turn and is passed explicitly to streaming/unary methods.
#[derive(Debug)]
struct ModelClientState {
    auth_manager: Option<Arc<AuthManager>>,
    conversation_id: ThreadId,
    provider: ModelProviderInfo,
    session_source: SessionSource,
    model_verbosity: Option<VerbosityConfig>,
    enable_responses_websockets: bool,
    enable_request_compression: bool,
    include_timing_metrics: bool,
    beta_features_header: Option<String>,
    disable_websockets: AtomicBool,
    cached_websocket_connection: StdMutex<Option<CachedWebsocketConnection>>,
}

/// A session-scoped client for model-provider API calls.
///
/// This holds configuration and state that should be shared across turns within a Codex session
/// (auth, provider selection, conversation id, feature-gated request behavior, and transport
/// fallback state).
///
/// WebSocket fallback is session-scoped: once a turn activates the HTTP fallback, subsequent turns
/// will also use HTTP for the remainder of the session.
///
/// Turn-scoped settings (model selection, reasoning controls, telemetry context, and turn metadata)
/// are passed explicitly to the relevant methods to keep turn lifetime visible at the call site.
///
/// This type is cheap to clone.
#[derive(Debug, Clone)]
pub struct ModelClient {
    state: Arc<ModelClientState>,
}

/// A turn-scoped streaming session created from a [`ModelClient`].
///
/// The session lazily establishes a Responses WebSocket connection (and reuses it across multiple
/// requests) and caches per-turn state:
///
/// - The last request and completed response, so subsequent calls can use the transport's
///   incremental request form when the input extends the previous request.
/// - The `x-codex-turn-state` sticky-routing token, which must be replayed for all requests within
///   the same turn.
///
/// Create a fresh `ModelClientSession` for each Codex turn. Reusing it across turns would replay
/// the previous turn's sticky-routing token into the next turn, which violates the client/server
/// contract and can cause routing bugs.
pub struct ModelClientSession {
    client: ModelClient,
    provider: ModelProviderInfo,
    connection: Option<ApiWebSocketConnection>,
    connection_auth_mode: Option<Option<AuthMode>>,
    websocket_last_request: Option<ResponseCreateWsRequest>,
    websocket_last_response_rx: Option<oneshot::Receiver<LastResponse>>,
    /// Turn state for sticky routing.
    ///
    /// This is an `OnceLock` that stores the turn state value received from the server
    /// on turn start via the `x-codex-turn-state` response header. Once set, this value
    /// should be sent back to the server in the `x-codex-turn-state` request header for
    /// all subsequent requests within the same turn to maintain sticky routing.
    ///
    /// This is a contract between the client and server: we receive it at turn start,
    /// keep sending it unchanged between turn requests (e.g., for retries, incremental
    /// appends, or continuation requests), and must not send it between different turns.
    turn_state: Arc<OnceLock<String>>,
}

#[derive(Debug, Clone, PartialEq)]
struct LastResponse {
    response_id: String,
    items_added: Vec<ResponseItem>,
}

struct CachedWebsocketConnection {
    provider: ModelProviderInfo,
    auth_mode: Option<AuthMode>,
    connection: ApiWebSocketConnection,
}

impl std::fmt::Debug for CachedWebsocketConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CachedWebsocketConnection")
            .field("provider", &self.provider)
            .field("auth_mode", &self.auth_mode)
            .finish_non_exhaustive()
    }
}

enum WebsocketStreamOutcome {
    Stream(ResponseStream),
    FallbackToHttp,
}

impl ModelClient {
    #[allow(clippy::too_many_arguments)]
    /// Creates a new session-scoped `ModelClient`.
    ///
    /// All arguments are expected to be stable for the lifetime of a Codex session. Per-turn values
    /// are passed to [`ModelClientSession::stream`] (and other turn-scoped methods) explicitly.
    pub fn new(
        auth_manager: Option<Arc<AuthManager>>,
        conversation_id: ThreadId,
        provider: ModelProviderInfo,
        session_source: SessionSource,
        model_verbosity: Option<VerbosityConfig>,
        enable_responses_websockets: bool,
        enable_responses_websockets_v2: bool,
        enable_request_compression: bool,
        include_timing_metrics: bool,
        beta_features_header: Option<String>,
    ) -> Self {
        let enable_responses_websockets =
            enable_responses_websockets || enable_responses_websockets_v2;
        Self {
            state: Arc::new(ModelClientState {
                auth_manager,
                conversation_id,
                provider,
                session_source,
                model_verbosity,
                enable_responses_websockets,
                enable_request_compression,
                include_timing_metrics,
                beta_features_header,
                disable_websockets: AtomicBool::new(false),
                cached_websocket_connection: StdMutex::new(None),
            }),
        }
    }

    /// Creates a fresh turn-scoped streaming session.
    ///
    /// This does not open any network connections; the WebSocket connection is established lazily
    /// when the first WebSocket stream request is issued.
    pub fn new_session(&self) -> ModelClientSession {
        self.new_session_with_provider(self.state.provider.clone())
    }

    pub fn new_session_with_provider(&self, provider: ModelProviderInfo) -> ModelClientSession {
        ModelClientSession {
            client: self.clone(),
            provider,
            connection: None,
            connection_auth_mode: None,
            websocket_last_request: None,
            websocket_last_response_rx: None,
            turn_state: Arc::new(OnceLock::new()),
        }
    }

    /// Compacts the current conversation history using the Compact endpoint.
    ///
    /// This is a unary call (no streaming) that returns a new list of
    /// `ResponseItem`s representing the compacted transcript.
    ///
    /// The model selection and telemetry context are passed explicitly to keep `ModelClient`
    /// session-scoped.
    pub async fn compact_conversation_history(
        &self,
        prompt: &Prompt,
        model_info: &ModelInfo,
        otel_manager: &OtelManager,
    ) -> Result<Vec<ResponseItem>> {
        self.compact_conversation_history_with_provider(
            &self.state.provider,
            prompt,
            model_info,
            otel_manager,
        )
        .await
    }

    pub async fn compact_conversation_history_with_provider(
        &self,
        provider: &ModelProviderInfo,
        prompt: &Prompt,
        model_info: &ModelInfo,
        otel_manager: &OtelManager,
    ) -> Result<Vec<ResponseItem>> {
        if prompt.input.is_empty() {
            return Ok(Vec::new());
        }
        let auth_manager = self.state.auth_manager.clone();
        let auth = match auth_manager.as_ref() {
            Some(manager) => manager.auth().await,
            None => None,
        };
        let request_auth = resolve_request_auth(auth, provider)?;
        let api_provider = provider.to_api_provider(request_auth.auth_mode)?;
        let api_auth = request_auth.provider;
        let transport = ReqwestTransport::new(build_reqwest_client());
        let request_telemetry = Self::build_request_telemetry(otel_manager);
        let client = ApiCompactClient::new(transport, api_provider, api_auth)
            .with_telemetry(Some(request_telemetry));

        let instructions = prompt.base_instructions.text.clone();
        let payload = ApiCompactionInput {
            model: &model_info.slug,
            input: &prompt.input,
            instructions: &instructions,
        };

        let mut extra_headers = self.build_subagent_headers();
        extra_headers.extend(build_conversation_headers(Some(
            self.state.conversation_id.to_string(),
        )));
        client
            .compact_input(&payload, extra_headers)
            .await
            .map_err(map_api_error)
    }

    /// Builds memory summaries for each provided normalized trace.
    ///
    /// This is a unary call (no streaming) to `/v1/memories/trace_summarize`.
    ///
    /// The model selection, reasoning effort, and telemetry context are passed explicitly to keep
    /// `ModelClient` session-scoped.
    pub async fn summarize_memory_traces(
        &self,
        traces: Vec<ApiMemoryTrace>,
        model_info: &ModelInfo,
        effort: Option<ReasoningEffortConfig>,
        otel_manager: &OtelManager,
    ) -> Result<Vec<ApiMemoryTraceSummaryOutput>> {
        if traces.is_empty() {
            return Ok(Vec::new());
        }

        let auth_manager = self.state.auth_manager.clone();
        let auth = match auth_manager.as_ref() {
            Some(manager) => manager.auth().await,
            None => None,
        };
        let request_auth = resolve_request_auth(auth, &self.state.provider)?;
        let api_provider = self
            .state
            .provider
            .to_api_provider(request_auth.auth_mode)?;
        let api_auth = request_auth.provider;
        let transport = ReqwestTransport::new(build_reqwest_client());
        let request_telemetry = Self::build_request_telemetry(otel_manager);
        let client = ApiMemoriesClient::new(transport, api_provider, api_auth)
            .with_telemetry(Some(request_telemetry));

        let payload = ApiMemoryTraceSummarizeInput {
            model: model_info.slug.clone(),
            traces,
            reasoning: effort.map(|effort| Reasoning {
                effort: Some(effort),
                summary: None,
            }),
        };

        client
            .trace_summarize_input(&payload, self.build_subagent_headers())
            .await
            .map_err(map_api_error)
    }

    fn build_subagent_headers(&self) -> ApiHeaderMap {
        let mut extra_headers = ApiHeaderMap::new();
        if let Some(subagent) = subagent_header_value(&self.state.session_source)
            && let Ok(val) = HeaderValue::from_str(&subagent)
        {
            extra_headers.insert(X_OPENAI_SUBAGENT_HEADER, val);
        }
        if let Some(parent_thread_id) = parent_thread_id_header_value(&self.state.session_source)
            && let Ok(val) = HeaderValue::from_str(&parent_thread_id)
        {
            extra_headers.insert(X_CODEX_PARENT_THREAD_ID_HEADER, val);
        }
        extra_headers
    }

    fn build_ws_client_metadata(
        &self,
        turn_metadata_header: Option<&str>,
    ) -> Option<HashMap<String, String>> {
        let mut metadata = HashMap::new();
        if let Some(turn_metadata_header) = turn_metadata_header {
            metadata.insert(
                X_CODEX_TURN_METADATA_HEADER.to_string(),
                turn_metadata_header.to_string(),
            );
        }
        if let Some(subagent) = subagent_header_value(&self.state.session_source) {
            metadata.insert(X_OPENAI_SUBAGENT_HEADER.to_string(), subagent);
        }
        if let Some(parent_thread_id) = parent_thread_id_header_value(&self.state.session_source) {
            metadata.insert(
                X_CODEX_PARENT_THREAD_ID_HEADER.to_string(),
                parent_thread_id,
            );
        }
        (!metadata.is_empty()).then_some(metadata)
    }

    /// Builds request telemetry for unary API calls (e.g., Compact endpoint).
    fn build_request_telemetry(otel_manager: &OtelManager) -> Arc<dyn RequestTelemetry> {
        let telemetry = Arc::new(ApiTelemetry::new(otel_manager.clone()));
        let request_telemetry: Arc<dyn RequestTelemetry> = telemetry;
        request_telemetry
    }

    fn take_cached_websocket_connection(
        &self,
        provider: &ModelProviderInfo,
        auth_mode: Option<AuthMode>,
    ) -> Option<ApiWebSocketConnection> {
        let mut cached = self
            .state
            .cached_websocket_connection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let connection = cached.take()?;

        if connection.provider == *provider && connection.auth_mode == auth_mode {
            Some(connection.connection)
        } else {
            *cached = Some(connection);
            None
        }
    }

    fn store_cached_websocket_connection(
        &self,
        provider: ModelProviderInfo,
        auth_mode: Option<AuthMode>,
        connection: ApiWebSocketConnection,
    ) {
        let mut cached = self
            .state
            .cached_websocket_connection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *cached = Some(CachedWebsocketConnection {
            provider,
            auth_mode,
            connection,
        });
    }

    fn clear_cached_websocket_connection(&self) {
        *self
            .state
            .cached_websocket_connection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }
}

impl ModelClientSession {
    pub(crate) fn clear_websocket_continuation(&mut self) {
        self.websocket_last_request = None;
        self.websocket_last_response_rx = None;
    }

    fn drop_websocket_connection(&mut self) {
        self.connection = None;
        self.connection_auth_mode = None;
        self.clear_websocket_continuation();
    }

    fn take_cacheable_connection(&mut self) -> Option<ApiWebSocketConnection> {
        let response_finished = websocket_response_finished_for_cache(
            self.websocket_last_request.as_ref(),
            self.websocket_last_response_rx.as_mut(),
        );
        if response_finished {
            self.clear_websocket_continuation();
            self.connection.take()
        } else {
            None
        }
    }

    fn disable_websockets(&self) -> bool {
        self.client.state.disable_websockets.load(Ordering::Relaxed)
    }

    fn activate_http_fallback(&self, websocket_enabled: bool) -> bool {
        websocket_enabled
            && !self
                .client
                .state
                .disable_websockets
                .swap(true, Ordering::Relaxed)
    }

    fn responses_websocket_enabled(&self) -> bool {
        self.provider.supports_websockets
            && self.client.state.enable_responses_websockets
            && (*CODEX_RS_SSE_FIXTURE).is_none()
    }

    fn build_responses_request(prompt: &Prompt) -> Result<ApiPrompt> {
        let instructions = prompt.base_instructions.text.clone();
        let tools_json: Vec<Value> = create_tools_json_for_responses_api(&prompt.tools)?;
        Ok(build_api_prompt(prompt, instructions, tools_json))
    }

    #[allow(clippy::too_many_arguments)]
    fn build_responses_options(
        &self,
        prompt: &Prompt,
        model_info: &ModelInfo,
        effort: Option<ReasoningEffortConfig>,
        summary: ReasoningSummaryConfig,
        service_tier: Option<String>,
        turn_metadata_header: Option<&str>,
        compression: Compression,
    ) -> ApiResponsesOptions {
        let turn_metadata_header =
            turn_metadata_header.and_then(|value| HeaderValue::from_str(value).ok());

        let default_reasoning_effort = model_info.default_reasoning_level;
        let reasoning = if model_info.supports_reasoning_summaries {
            Some(Reasoning {
                effort: effort.or(default_reasoning_effort),
                summary: if summary == ReasoningSummaryConfig::None {
                    None
                } else {
                    Some(summary)
                },
            })
        } else {
            None
        };

        let include = if reasoning.is_some() {
            vec!["reasoning.encrypted_content".to_string()]
        } else {
            Vec::new()
        };

        let verbosity = if model_info.support_verbosity {
            self.client
                .state
                .model_verbosity
                .or(model_info.default_verbosity)
        } else {
            if self.client.state.model_verbosity.is_some() {
                warn!(
                    "model_verbosity is set but ignored as the model does not support verbosity: {}",
                    model_info.slug
                );
            }
            None
        };

        let text = create_text_param_for_request(verbosity, &prompt.output_schema);
        let conversation_id = self.client.state.conversation_id.to_string();

        ApiResponsesOptions {
            reasoning,
            include,
            service_tier: service_tier_for_wire(&self.provider, service_tier),
            prompt_cache_key: Some(conversation_id.clone()),
            text,
            store_override: None,
            conversation_id: Some(conversation_id),
            session_source: Some(self.client.state.session_source.clone()),
            extra_headers: build_responses_headers(
                self.client.state.beta_features_header.as_deref(),
                Some(&self.turn_state),
                turn_metadata_header.as_ref(),
            ),
            compression,
            turn_state: Some(Arc::clone(&self.turn_state)),
        }
    }

    fn get_incremental_items(
        &self,
        request: &ResponseCreateWsRequest,
        last_response: Option<&LastResponse>,
        allow_empty_delta: bool,
    ) -> Option<Vec<ResponseItem>> {
        // Incremental websocket requests are only valid when non-input fields are unchanged and
        // the new input extends the previous request plus output items already returned by the server.
        let previous_request = self.websocket_last_request.as_ref()?;
        if !response_create_properties_match(previous_request, request) {
            return None;
        }

        let after_previous_request = request
            .input
            .as_slice()
            .strip_prefix(previous_request.input.as_slice())?;
        let delta = match last_response {
            Some(last_response) => {
                after_previous_request.strip_prefix(last_response.items_added.as_slice())?
            }
            None => after_previous_request,
        };

        if !allow_empty_delta && delta.is_empty() {
            return None;
        }

        Some(delta.to_vec())
    }

    fn get_last_response(&mut self) -> Option<LastResponse> {
        self.websocket_last_response_rx
            .take()
            .and_then(|mut receiver| match receiver.try_recv() {
                Ok(last_response) => Some(last_response),
                Err(TryRecvError::Closed) | Err(TryRecvError::Empty) => None,
            })
    }

    fn prepare_websocket_request(
        &mut self,
        model_slug: &str,
        api_prompt: &ApiPrompt,
        options: &ApiResponsesOptions,
    ) -> (ResponsesWsRequest, ResponseCreateWsRequest) {
        let ApiResponsesOptions {
            reasoning,
            include,
            service_tier,
            prompt_cache_key,
            text,
            store_override,
            ..
        } = options;

        let store = store_override.unwrap_or(false);
        let mut client_metadata = self
            .client
            .build_ws_client_metadata(turn_metadata_header_from_options(options));
        if let Some(turn_state) = self.turn_state.get() {
            client_metadata
                .get_or_insert_with(HashMap::new)
                .insert(X_CODEX_TURN_STATE_HEADER.to_string(), turn_state.clone());
        }

        let payload = ResponseCreateWsRequest {
            model: model_slug.to_string(),
            instructions: api_prompt.instructions.clone(),
            previous_response_id: None,
            input: api_prompt.input.clone(),
            tools: api_prompt.tools.clone(),
            tool_choice: "auto".to_string(),
            parallel_tool_calls: api_prompt.parallel_tool_calls,
            reasoning: reasoning.clone(),
            store,
            stream: true,
            include: include.clone(),
            service_tier: service_tier.clone(),
            prompt_cache_key: prompt_cache_key.clone(),
            text: text.clone(),
            generate: None,
            client_metadata,
        };

        let Some(last_response) = self.get_last_response() else {
            return (ResponsesWsRequest::ResponseCreate(payload.clone()), payload);
        };
        let Some(incremental_items) =
            self.get_incremental_items(&payload, Some(&last_response), true)
        else {
            return (ResponsesWsRequest::ResponseCreate(payload.clone()), payload);
        };
        if last_response.response_id.is_empty() {
            return (ResponsesWsRequest::ResponseCreate(payload.clone()), payload);
        }

        (
            ResponsesWsRequest::ResponseCreate(ResponseCreateWsRequest {
                previous_response_id: Some(last_response.response_id),
                input: incremental_items,
                ..payload.clone()
            }),
            payload,
        )
    }

    async fn websocket_connection(
        &mut self,
        otel_manager: &OtelManager,
        api_provider: codex_api::Provider,
        api_auth: CoreAuthProvider,
        auth_mode: Option<AuthMode>,
        options: &ApiResponsesOptions,
    ) -> std::result::Result<&ApiWebSocketConnection, ApiError> {
        if self.connection.is_some() && self.connection_auth_mode != Some(auth_mode) {
            self.drop_websocket_connection();
        }
        if self.connection.is_none()
            && let Some(connection) = self
                .client
                .take_cached_websocket_connection(&self.provider, auth_mode)
        {
            self.connection = Some(connection);
            self.connection_auth_mode = Some(auth_mode);
        }

        let needs_new = match self.connection.as_ref() {
            Some(conn) => conn.is_closed().await,
            None => true,
        };

        if needs_new {
            self.drop_websocket_connection();
            let mut headers = options.extra_headers.clone();
            headers.extend(build_conversation_headers(options.conversation_id.clone()));
            headers.insert(
                OPENAI_BETA_HEADER,
                HeaderValue::from_static(RESPONSES_WEBSOCKETS_V2_BETA_HEADER_VALUE),
            );
            if self.client.state.include_timing_metrics {
                headers.insert(
                    X_RESPONSESAPI_INCLUDE_TIMING_METRICS_HEADER,
                    HeaderValue::from_static("true"),
                );
            }
            let websocket_telemetry = Self::build_websocket_telemetry(otel_manager);
            let websocket_client = ApiWebSocketResponsesClient::new(api_provider, api_auth);
            let connect = websocket_client.connect(
                headers,
                default_headers(),
                options.turn_state.clone(),
                Some(websocket_telemetry),
            );
            let new_conn: ApiWebSocketConnection =
                tokio::time::timeout(DEFAULT_WEBSOCKET_CONNECT_TIMEOUT, connect)
                    .await
                    .map_err(|_| ApiError::Transport(TransportError::Timeout))??;
            self.connection = Some(new_conn);
            self.connection_auth_mode = Some(auth_mode);
        }

        self.connection.as_ref().ok_or(ApiError::Stream(
            "websocket connection is unavailable".to_string(),
        ))
    }

    fn responses_request_compression(&self, auth_mode: Option<AuthMode>) -> Compression {
        if self.client.state.enable_request_compression
            && auth_mode == Some(AuthMode::Chatgpt)
            && self.provider.is_openai()
        {
            Compression::Zstd
        } else {
            Compression::None
        }
    }

    /// Streams a turn via the OpenAI Responses API.
    ///
    /// Handles SSE fixtures, reasoning summaries, verbosity, and the
    /// `text` controls used for output schemas.
    #[allow(clippy::too_many_arguments)]
    async fn stream_responses_api(
        &self,
        prompt: &Prompt,
        model_info: &ModelInfo,
        otel_manager: &OtelManager,
        effort: Option<ReasoningEffortConfig>,
        summary: ReasoningSummaryConfig,
        service_tier: Option<String>,
        turn_metadata_header: Option<&str>,
    ) -> Result<ResponseStream> {
        if let Some(path) = &*CODEX_RS_SSE_FIXTURE {
            warn!(path, "Streaming from fixture");
            let stream = codex_api::stream_from_fixture(path, self.provider.stream_idle_timeout())
                .map_err(map_api_error)?;
            let (stream, _last_response_rx) = map_response_stream(stream, otel_manager.clone());
            return Ok(stream);
        }

        let auth_manager = self.client.state.auth_manager.clone();
        let api_prompt = Self::build_responses_request(prompt)?;

        let mut auth_recovery = None;
        loop {
            let auth = match auth_manager.as_ref() {
                Some(manager) => manager.auth().await,
                None => None,
            };
            let request_auth = resolve_request_auth(auth, &self.provider)?;
            if request_auth.enable_unauthorized_recovery && auth_recovery.is_none() {
                auth_recovery = auth_manager
                    .as_ref()
                    .map(super::auth::AuthManager::unauthorized_recovery);
            }
            let api_provider = self.provider.to_api_provider(request_auth.auth_mode)?;
            let api_auth = request_auth.provider;
            let transport = ReqwestTransport::new(build_reqwest_client());
            let (request_telemetry, sse_telemetry) = Self::build_streaming_telemetry(otel_manager);
            let compression = self.responses_request_compression(request_auth.auth_mode);

            let client = ApiResponsesClient::new(transport, api_provider, api_auth)
                .with_telemetry(Some(request_telemetry), Some(sse_telemetry));

            let options = self.build_responses_options(
                prompt,
                model_info,
                effort,
                summary,
                service_tier.clone(),
                turn_metadata_header,
                compression,
            );

            let stream_result = client
                .stream_prompt(&model_info.slug, &api_prompt, options)
                .await;

            match stream_result {
                Ok(stream) => {
                    let (stream, _last_response_rx) =
                        map_response_stream(stream, otel_manager.clone());
                    return Ok(stream);
                }
                Err(ApiError::Transport(
                    unauthorized_transport @ TransportError::Http { status, .. },
                )) if status == StatusCode::UNAUTHORIZED => {
                    handle_unauthorized(unauthorized_transport, &mut auth_recovery).await?;
                    continue;
                }
                Err(err) => return Err(map_api_error(err)),
            }
        }
    }

    /// Streams a turn via the Responses API over WebSocket transport.
    #[allow(clippy::too_many_arguments)]
    async fn stream_responses_websocket(
        &mut self,
        prompt: &Prompt,
        model_info: &ModelInfo,
        otel_manager: &OtelManager,
        effort: Option<ReasoningEffortConfig>,
        summary: ReasoningSummaryConfig,
        service_tier: Option<String>,
        turn_metadata_header: Option<&str>,
    ) -> Result<WebsocketStreamOutcome> {
        let auth_manager = self.client.state.auth_manager.clone();
        let api_prompt = Self::build_responses_request(prompt)?;

        let mut auth_recovery = None;
        loop {
            let auth = match auth_manager.as_ref() {
                Some(manager) => manager.auth().await,
                None => None,
            };
            let request_auth = resolve_request_auth(auth, &self.provider)?;
            if request_auth.enable_unauthorized_recovery && auth_recovery.is_none() {
                auth_recovery = auth_manager
                    .as_ref()
                    .map(super::auth::AuthManager::unauthorized_recovery);
            }
            let api_provider = self.provider.to_api_provider(request_auth.auth_mode)?;
            let api_auth = request_auth.provider;
            let compression = self.responses_request_compression(request_auth.auth_mode);

            let options = self.build_responses_options(
                prompt,
                model_info,
                effort,
                summary,
                service_tier.clone(),
                turn_metadata_header,
                compression,
            );
            match self
                .websocket_connection(
                    otel_manager,
                    api_provider.clone(),
                    api_auth.clone(),
                    request_auth.auth_mode,
                    &options,
                )
                .await
            {
                Ok(_) => {}
                Err(ApiError::Transport(
                    unauthorized_transport @ TransportError::Http { status, .. },
                )) if status == StatusCode::UNAUTHORIZED => {
                    handle_unauthorized(unauthorized_transport, &mut auth_recovery).await?;
                    continue;
                }
                Err(ApiError::Transport(TransportError::Http { status, .. }))
                    if status == StatusCode::UPGRADE_REQUIRED =>
                {
                    return Ok(WebsocketStreamOutcome::FallbackToHttp);
                }
                Err(err) => return Err(map_api_error(err)),
            };

            let (request, last_request) =
                self.prepare_websocket_request(&model_info.slug, &api_prompt, &options);
            let connection = self.connection.as_ref().ok_or_else(|| {
                map_api_error(ApiError::Stream(
                    "websocket connection is unavailable".to_string(),
                ))
            })?;
            let stream_result = connection
                .stream_request(request, Some(Arc::clone(&self.turn_state)))
                .await
                .map_err(map_api_error)?;
            self.websocket_last_request = Some(last_request);
            let (stream, last_response_rx) =
                map_response_stream(stream_result, otel_manager.clone());
            self.websocket_last_response_rx = Some(last_response_rx);

            return Ok(WebsocketStreamOutcome::Stream(stream));
        }
    }

    /// Builds request and SSE telemetry for streaming API calls.
    fn build_streaming_telemetry(
        otel_manager: &OtelManager,
    ) -> (Arc<dyn RequestTelemetry>, Arc<dyn SseTelemetry>) {
        let telemetry = Arc::new(ApiTelemetry::new(otel_manager.clone()));
        let request_telemetry: Arc<dyn RequestTelemetry> = telemetry.clone();
        let sse_telemetry: Arc<dyn SseTelemetry> = telemetry;
        (request_telemetry, sse_telemetry)
    }

    /// Builds telemetry for the Responses API WebSocket transport.
    fn build_websocket_telemetry(otel_manager: &OtelManager) -> Arc<dyn WebsocketTelemetry> {
        let telemetry = Arc::new(ApiTelemetry::new(otel_manager.clone()));
        let websocket_telemetry: Arc<dyn WebsocketTelemetry> = telemetry;
        websocket_telemetry
    }

    #[allow(clippy::too_many_arguments)]
    /// Streams a single model request within the current turn.
    ///
    /// The caller is responsible for passing per-turn settings explicitly (model selection,
    /// reasoning settings, telemetry context, and turn metadata). This method will prefer the
    /// Responses WebSocket transport when enabled and healthy, and will fall back to the HTTP
    /// Responses API transport otherwise.
    pub async fn stream(
        &mut self,
        prompt: &Prompt,
        model_info: &ModelInfo,
        otel_manager: &OtelManager,
        effort: Option<ReasoningEffortConfig>,
        summary: ReasoningSummaryConfig,
        service_tier: Option<String>,
        turn_metadata_header: Option<&str>,
    ) -> Result<ResponseStream> {
        let wire_api = self.provider.wire_api;
        match wire_api {
            WireApi::Responses => {
                let websocket_enabled =
                    self.responses_websocket_enabled() && !self.disable_websockets();

                if websocket_enabled {
                    match self
                        .stream_responses_websocket(
                            prompt,
                            model_info,
                            otel_manager,
                            effort,
                            summary,
                            service_tier.clone(),
                            turn_metadata_header,
                        )
                        .await?
                    {
                        WebsocketStreamOutcome::Stream(stream) => return Ok(stream),
                        WebsocketStreamOutcome::FallbackToHttp => {
                            self.try_switch_fallback_transport(otel_manager);
                        }
                    }
                }

                self.stream_responses_api(
                    prompt,
                    model_info,
                    otel_manager,
                    effort,
                    summary,
                    service_tier.clone(),
                    turn_metadata_header,
                )
                .await
            }
        }
    }

    /// Permanently disables WebSockets for this Codex session and resets WebSocket state.
    ///
    /// This is used after exhausting the provider retry budget, to force subsequent requests onto
    /// the HTTP transport. Returns `true` if this call activated fallback, or `false` if fallback
    /// was already active.
    pub(crate) fn try_switch_fallback_transport(&mut self, otel_manager: &OtelManager) -> bool {
        let websocket_enabled = self.responses_websocket_enabled();
        let activated = self.activate_http_fallback(websocket_enabled);
        if activated {
            warn!("falling back to HTTP");
            otel_manager.counter(
                "codex.transport.fallback_to_http",
                1,
                &[("from_wire_api", "responses_websocket")],
            );

            self.client.clear_cached_websocket_connection();
            self.drop_websocket_connection();
        }
        activated
    }
}

impl Drop for ModelClientSession {
    fn drop(&mut self) {
        if self.disable_websockets() {
            return;
        }

        let Some(auth_mode) = self.connection_auth_mode else {
            return;
        };
        let Some(connection) = self.take_cacheable_connection() else {
            return;
        };

        self.client
            .store_cached_websocket_connection(self.provider.clone(), auth_mode, connection);
    }
}

/// Adapts the core `Prompt` type into the `codex-api` payload shape.
fn build_api_prompt(prompt: &Prompt, instructions: String, tools_json: Vec<Value>) -> ApiPrompt {
    ApiPrompt {
        instructions,
        input: prompt.get_formatted_input(),
        tools: tools_json,
        parallel_tool_calls: prompt.parallel_tool_calls,
        output_schema: prompt.output_schema.clone(),
    }
}

fn websocket_response_finished_for_cache(
    last_request: Option<&ResponseCreateWsRequest>,
    last_response_rx: Option<&mut oneshot::Receiver<LastResponse>>,
) -> bool {
    match last_response_rx {
        Some(receiver) => match receiver.try_recv() {
            Ok(_) => true,
            Err(TryRecvError::Empty | TryRecvError::Closed) => false,
        },
        None => last_request.is_none(),
    }
}

fn response_create_properties_match(
    previous: &ResponseCreateWsRequest,
    current: &ResponseCreateWsRequest,
) -> bool {
    let ResponseCreateWsRequest {
        model: previous_model,
        instructions: previous_instructions,
        previous_response_id: _,
        input: _,
        tools: previous_tools,
        tool_choice: previous_tool_choice,
        parallel_tool_calls: previous_parallel_tool_calls,
        reasoning: previous_reasoning,
        store: previous_store,
        stream: previous_stream,
        include: previous_include,
        service_tier: previous_service_tier,
        prompt_cache_key: previous_prompt_cache_key,
        text: previous_text,
        generate: _,
        client_metadata: _,
    } = previous;

    let ResponseCreateWsRequest {
        model: current_model,
        instructions: current_instructions,
        previous_response_id: _,
        input: _,
        tools: current_tools,
        tool_choice: current_tool_choice,
        parallel_tool_calls: current_parallel_tool_calls,
        reasoning: current_reasoning,
        store: current_store,
        stream: current_stream,
        include: current_include,
        service_tier: current_service_tier,
        prompt_cache_key: current_prompt_cache_key,
        text: current_text,
        generate: _,
        client_metadata: _,
    } = current;

    previous_model == current_model
        && previous_instructions == current_instructions
        && previous_tools == current_tools
        && previous_tool_choice == current_tool_choice
        && previous_parallel_tool_calls == current_parallel_tool_calls
        && previous_reasoning == current_reasoning
        && previous_store == current_store
        && previous_stream == current_stream
        && previous_include == current_include
        && previous_service_tier == current_service_tier
        && previous_prompt_cache_key == current_prompt_cache_key
        && previous_text == current_text
}

fn turn_metadata_header_from_options(options: &ApiResponsesOptions) -> Option<&str> {
    options
        .extra_headers
        .get(X_CODEX_TURN_METADATA_HEADER)
        .and_then(|value| value.to_str().ok())
}

fn subagent_header_value(session_source: &SessionSource) -> Option<String> {
    let SessionSource::SubAgent(subagent_source) = session_source else {
        return None;
    };
    Some(match subagent_source {
        SubAgentSource::Review => "review".to_string(),
        SubAgentSource::Compact => "compact".to_string(),
        SubAgentSource::ThreadSpawn { .. } => "collab_spawn".to_string(),
        SubAgentSource::Other(label) => label.clone(),
    })
}

fn parent_thread_id_header_value(session_source: &SessionSource) -> Option<String> {
    let SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id, ..
    }) = session_source
    else {
        return None;
    };
    Some(parent_thread_id.to_string())
}

/// Builds the extra headers attached to Responses API requests.
///
/// These headers implement Codex-specific conventions:
///
/// - `x-codex-beta-features`: comma-separated beta feature keys enabled for the session.
/// - `x-codex-turn-state`: sticky routing token captured earlier in the turn.
/// - `x-codex-turn-metadata`: optional per-turn metadata for observability.
fn build_responses_headers(
    beta_features_header: Option<&str>,
    turn_state: Option<&Arc<OnceLock<String>>>,
    turn_metadata_header: Option<&HeaderValue>,
) -> ApiHeaderMap {
    let mut headers = ApiHeaderMap::new();
    if let Some(value) = beta_features_header
        && !value.is_empty()
        && let Ok(header_value) = HeaderValue::from_str(value)
    {
        headers.insert("x-codex-beta-features", header_value);
    }
    if let Some(turn_state) = turn_state
        && let Some(state) = turn_state.get()
        && let Ok(header_value) = HeaderValue::from_str(state)
    {
        headers.insert(X_CODEX_TURN_STATE_HEADER, header_value);
    }
    if let Some(header_value) = turn_metadata_header {
        headers.insert(X_CODEX_TURN_METADATA_HEADER, header_value.clone());
    }
    headers
}

fn service_tier_for_wire(
    provider: &ModelProviderInfo,
    service_tier: Option<String>,
) -> Option<String> {
    if !provider.is_openai() {
        return None;
    }

    service_tier.map(ServiceTier::normalize_request_value)
}

fn map_response_stream<S>(
    api_stream: S,
    otel_manager: OtelManager,
) -> (ResponseStream, oneshot::Receiver<LastResponse>)
where
    S: futures::Stream<Item = std::result::Result<ResponseEvent, ApiError>>
        + Unpin
        + Send
        + 'static,
{
    let (tx_event, rx_event) = mpsc::channel::<Result<ResponseEvent>>(1600);
    let (tx_last_response, rx_last_response) = oneshot::channel::<LastResponse>();
    let consumer_dropped = tokio_util::sync::CancellationToken::new();
    let consumer_dropped_for_stream = consumer_dropped.clone();

    tokio::spawn(async move {
        let mut tx_last_response = Some(tx_last_response);
        let mut items_added = Vec::new();
        let mut logged_error = false;
        let mut api_stream = api_stream;
        loop {
            let event = tokio::select! {
                _ = consumer_dropped.cancelled() => return,
                event = api_stream.next() => event,
            };
            let Some(event) = event else {
                break;
            };
            match event {
                Ok(ResponseEvent::OutputItemDone(item)) => {
                    items_added.push(item.clone());
                    if tx_event
                        .send(Ok(ResponseEvent::OutputItemDone(item)))
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
                Ok(ResponseEvent::Completed {
                    response_id,
                    token_usage,
                    end_turn,
                }) => {
                    if let Some(usage) = &token_usage {
                        otel_manager.sse_event_completed(
                            usage.input_tokens,
                            usage.output_tokens,
                            Some(usage.cached_input_tokens),
                            Some(usage.reasoning_output_tokens),
                            usage.total_tokens,
                        );
                    }
                    if let Some(tx_last_response) = tx_last_response.take() {
                        let _ = tx_last_response.send(LastResponse {
                            response_id: response_id.clone(),
                            items_added: std::mem::take(&mut items_added),
                        });
                    }
                    if tx_event
                        .send(Ok(ResponseEvent::Completed {
                            response_id,
                            token_usage,
                            end_turn,
                        }))
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
                Ok(event) => {
                    if tx_event.send(Ok(event)).await.is_err() {
                        return;
                    }
                }
                Err(err) => {
                    let mapped = map_api_error(err);
                    if !logged_error {
                        otel_manager.see_event_completed_failed(&mapped);
                        logged_error = true;
                    }
                    if tx_event.send(Err(mapped)).await.is_err() {
                        return;
                    }
                }
            }
        }
    });

    (
        ResponseStream {
            rx_event,
            consumer_dropped: consumer_dropped_for_stream,
        },
        rx_last_response,
    )
}

/// Handles a 401 response by optionally refreshing ChatGPT tokens once.
///
/// When refresh succeeds, the caller should retry the API call; otherwise
/// the mapped `CodexErr` is returned to the caller.
async fn handle_unauthorized(
    transport: TransportError,
    auth_recovery: &mut Option<UnauthorizedRecovery>,
) -> Result<()> {
    if let Some(recovery) = auth_recovery
        && recovery.has_next()
    {
        return match recovery.next().await {
            Ok(_) => Ok(()),
            Err(RefreshTokenError::Permanent(failed)) => Err(CodexErr::RefreshTokenFailed(failed)),
            Err(RefreshTokenError::Transient(other)) => Err(CodexErr::Io(other)),
        };
    }

    Err(map_api_error(ApiError::Transport(transport)))
}

struct ApiTelemetry {
    otel_manager: OtelManager,
}

impl ApiTelemetry {
    fn new(otel_manager: OtelManager) -> Self {
        Self { otel_manager }
    }
}

impl RequestTelemetry for ApiTelemetry {
    fn on_request(
        &self,
        attempt: u64,
        status: Option<HttpStatusCode>,
        error: Option<&TransportError>,
        duration: Duration,
    ) {
        let error_message = error.map(std::string::ToString::to_string);
        self.otel_manager.record_api_request(
            attempt,
            status.map(|s| s.as_u16()),
            error_message.as_deref(),
            duration,
        );
    }
}

impl SseTelemetry for ApiTelemetry {
    fn on_sse_poll(
        &self,
        result: &std::result::Result<
            Option<std::result::Result<Event, EventStreamError<TransportError>>>,
            tokio::time::error::Elapsed,
        >,
        duration: Duration,
    ) {
        self.otel_manager.log_sse_event(result, duration);
    }
}

impl WebsocketTelemetry for ApiTelemetry {
    fn on_ws_request(&self, duration: Duration, error: Option<&ApiError>) {
        let error_message = error.map(std::string::ToString::to_string);
        self.otel_manager
            .record_websocket_request(duration, error_message.as_deref());
    }

    fn on_ws_event(
        &self,
        result: &std::result::Result<Option<std::result::Result<Message, Error>>, ApiError>,
        duration: Duration,
    ) {
        self.otel_manager.record_websocket_event(result, duration);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model_provider_info::OLLAMA_OSS_PROVIDER_ID;
    use crate::model_provider_info::built_in_model_providers;
    use codex_api::common::OpenAiVerbosity;
    use codex_api::common::TextControls;
    use codex_protocol::models::ContentItem;
    use codex_protocol::models::FunctionCallOutputPayload;
    use pretty_assertions::assert_eq;
    use serde_json::json;
    use tokio::sync::oneshot;

    type RequestMutation = fn(&mut ResponseCreateWsRequest);

    #[test]
    fn service_tier_for_wire_maps_only_openai_provider() {
        let providers = built_in_model_providers();
        let openai = providers.get("openai").expect("openai provider");
        let ollama = providers
            .get(OLLAMA_OSS_PROVIDER_ID)
            .expect("ollama provider");

        assert_eq!(
            service_tier_for_wire(openai, Some("flex".to_string())),
            Some("flex".to_string())
        );
        assert_eq!(
            service_tier_for_wire(openai, Some("fast".to_string())),
            Some("priority".to_string())
        );
        assert_eq!(
            service_tier_for_wire(openai, Some("experimental-tier-id".to_string())),
            Some("experimental-tier-id".to_string())
        );
        assert_eq!(service_tier_for_wire(openai, None), None);
        assert_eq!(
            service_tier_for_wire(ollama, Some("flex".to_string())),
            None
        );
    }

    #[tokio::test]
    async fn dropping_mapped_response_stream_cancels_provider_stream() {
        struct PendingApiStream {
            dropped: Option<oneshot::Sender<()>>,
        }

        impl futures::Stream for PendingApiStream {
            type Item = std::result::Result<ResponseEvent, ApiError>;

            fn poll_next(
                self: std::pin::Pin<&mut Self>,
                _cx: &mut std::task::Context<'_>,
            ) -> std::task::Poll<Option<Self::Item>> {
                std::task::Poll::Pending
            }
        }

        impl Drop for PendingApiStream {
            fn drop(&mut self) {
                if let Some(dropped) = self.dropped.take() {
                    let _ = dropped.send(());
                }
            }
        }

        let (dropped_tx, dropped_rx) = oneshot::channel();
        let (stream, _last_response_rx) = map_response_stream(
            PendingApiStream {
                dropped: Some(dropped_tx),
            },
            test_otel_manager(),
        );

        drop(stream);

        let drop_result = tokio::time::timeout(Duration::from_secs(1), dropped_rx).await;
        assert!(matches!(drop_result, Ok(Ok(()))));
    }

    #[tokio::test]
    async fn mapped_response_stream_completion_populates_last_response() {
        let item = assistant_message_item("msg-1", "hello");
        let api_stream = futures::stream::iter(vec![
            Ok(ResponseEvent::OutputItemDone(item.clone())),
            Ok(ResponseEvent::Completed {
                response_id: "resp-1".to_string(),
                token_usage: None,
                end_turn: Some(true),
            }),
        ]);
        let (mut stream, last_response_rx) = map_response_stream(api_stream, test_otel_manager());

        while let Some(event) = stream.next().await {
            if matches!(event, Ok(ResponseEvent::Completed { .. })) {
                break;
            }
        }

        assert_eq!(
            last_response_rx.await.ok(),
            Some(LastResponse {
                response_id: "resp-1".to_string(),
                items_added: vec![item],
            })
        );
    }

    #[test]
    fn websocket_response_finished_for_cache_tracks_receiver_state() {
        assert!(websocket_response_finished_for_cache(None, None));

        let request = ws_request(Vec::new());
        assert!(!websocket_response_finished_for_cache(Some(&request), None));

        let (completed_tx, mut completed_rx) = oneshot::channel();
        completed_tx
            .send(LastResponse {
                response_id: "resp-1".to_string(),
                items_added: Vec::new(),
            })
            .expect("send completed response");
        assert!(websocket_response_finished_for_cache(
            Some(&request),
            Some(&mut completed_rx)
        ));

        let (_pending_tx, mut pending_rx) = oneshot::channel();
        assert!(!websocket_response_finished_for_cache(
            Some(&request),
            Some(&mut pending_rx)
        ));

        let (closed_tx, mut closed_rx) = oneshot::channel::<LastResponse>();
        drop(closed_tx);
        assert!(!websocket_response_finished_for_cache(
            Some(&request),
            Some(&mut closed_rx)
        ));
    }

    #[test]
    fn incremental_items_preserve_prefix_when_steer_appends_after_committed_response() {
        let initial_user = user_message_item("initial prompt");
        let assistant = assistant_message_item("msg-1", "working on it");
        let tool_output = function_call_output_item("call-1", "tool complete");
        let steer = user_message_item("steer: prefer focused tests");

        let previous_request = ws_request(vec![initial_user.clone()]);
        let last_response = LastResponse {
            response_id: "resp-1".to_string(),
            items_added: vec![assistant.clone()],
        };
        let session = test_client_session(previous_request);

        let post_steer_request = ws_request(vec![
            initial_user.clone(),
            assistant.clone(),
            tool_output.clone(),
            steer.clone(),
        ]);

        assert_eq!(
            session.get_incremental_items(&post_steer_request, Some(&last_response), false),
            Some(vec![tool_output.clone(), steer.clone()])
        );

        let reordered_request = ws_request(vec![initial_user, steer, assistant, tool_output]);

        assert_eq!(
            session.get_incremental_items(&reordered_request, Some(&last_response), false),
            None
        );
    }

    #[test]
    fn incremental_items_respects_empty_delta_setting() {
        let initial_user = user_message_item("initial prompt");
        let request = ws_request(vec![initial_user]);
        let session = test_client_session(request.clone());

        assert_eq!(
            session.get_incremental_items(&request, None, true),
            Some(Vec::new())
        );
        assert_eq!(session.get_incremental_items(&request, None, false), None);
    }

    #[test]
    fn incremental_items_ignore_request_scoped_transport_fields() {
        let initial_user = user_message_item("initial prompt");
        let next_user = user_message_item("next prompt");
        let previous_request = ws_request(vec![initial_user.clone()]);
        let session = test_client_session(previous_request);
        let mut request = ws_request(vec![initial_user, next_user.clone()]);

        request.previous_response_id = Some("resp-ignored".to_string());
        request.generate = Some(false);
        request.client_metadata = Some(HashMap::from([(
            "x-codex-turn-state".to_string(),
            "turn-state".to_string(),
        )]));

        assert_eq!(
            session.get_incremental_items(&request, None, false),
            Some(vec![next_user])
        );
    }

    #[test]
    fn incremental_items_rejects_changed_request_properties() {
        let initial_user = user_message_item("initial prompt");
        let next_user = user_message_item("next prompt");

        let cases: Vec<(&str, RequestMutation)> = vec![
            ("model", |request| request.model = "other-model".to_string()),
            ("instructions", |request| {
                request.instructions = "changed instructions".to_string();
            }),
            ("tools", |request| {
                request.tools = vec![json!({
                    "type": "function",
                    "name": "lookup",
                    "parameters": {"type": "object"}
                })];
            }),
            ("tool_choice", |request| {
                request.tool_choice = "none".to_string();
            }),
            ("parallel_tool_calls", |request| {
                request.parallel_tool_calls = !request.parallel_tool_calls;
            }),
            ("reasoning", |request| {
                request.reasoning = Some(Reasoning {
                    effort: Some(ReasoningEffortConfig::High),
                    summary: Some(ReasoningSummaryConfig::Detailed),
                });
            }),
            ("store", |request| request.store = !request.store),
            ("stream", |request| request.stream = !request.stream),
            ("include", |request| {
                request.include = vec!["reasoning.encrypted_content".to_string()];
            }),
            ("service_tier", |request| {
                request.service_tier = Some("priority".to_string());
            }),
            ("prompt_cache_key", |request| {
                request.prompt_cache_key = Some("other-thread".to_string());
            }),
            ("text", |request| {
                request.text = Some(TextControls {
                    verbosity: Some(OpenAiVerbosity::High),
                    format: None,
                });
            }),
        ];

        for (name, mutate) in cases {
            let previous_request = ws_request(vec![initial_user.clone()]);
            let session = test_client_session(previous_request);
            let mut request = ws_request(vec![initial_user.clone(), next_user.clone()]);
            mutate(&mut request);

            assert_eq!(
                session.get_incremental_items(&request, None, false),
                None,
                "{name} change should force full create"
            );
        }
    }

    fn assistant_message_item(id: &str, text: &str) -> ResponseItem {
        ResponseItem::Message {
            id: Some(id.to_string()),
            role: "assistant".to_string(),
            content: vec![ContentItem::OutputText {
                text: text.to_string(),
            }],
            end_turn: None,
            phase: None,
        }
    }

    fn user_message_item(text: &str) -> ResponseItem {
        ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: text.to_string(),
            }],
            end_turn: None,
            phase: None,
        }
    }

    fn function_call_output_item(call_id: &str, output: &str) -> ResponseItem {
        ResponseItem::FunctionCallOutput {
            call_id: call_id.to_string(),
            output: FunctionCallOutputPayload::from_text(output.to_string()),
        }
    }

    fn ws_request(input: Vec<ResponseItem>) -> ResponseCreateWsRequest {
        ResponseCreateWsRequest {
            model: "test-model".to_string(),
            instructions: "test instructions".to_string(),
            previous_response_id: None,
            input,
            tools: Vec::new(),
            tool_choice: "auto".to_string(),
            parallel_tool_calls: false,
            reasoning: None,
            store: false,
            stream: true,
            include: Vec::new(),
            service_tier: None,
            prompt_cache_key: Some("test-thread".to_string()),
            text: None,
            generate: None,
            client_metadata: None,
        }
    }

    fn test_client_session(previous_request: ResponseCreateWsRequest) -> ModelClientSession {
        let provider = built_in_model_providers()
            .get("openai")
            .expect("openai provider")
            .clone();
        let client = ModelClient::new(
            None,
            ThreadId::new(),
            provider,
            SessionSource::Exec,
            None,
            true,
            false,
            false,
            false,
            None,
        );
        let mut session = client.new_session();
        session.websocket_last_request = Some(previous_request);
        session
    }

    fn test_otel_manager() -> OtelManager {
        OtelManager::new(
            ThreadId::new(),
            "test-model",
            "test-model",
            None,
            None,
            None,
            false,
            "test".to_string(),
            SessionSource::Exec,
        )
    }
}
