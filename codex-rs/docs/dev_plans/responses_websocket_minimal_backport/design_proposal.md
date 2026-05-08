# Responses WebSocket Minimal Backport - Design Proposal

## Design Principle

Patch this branch in place.

Keep these current-branch shapes:

- `codex-api/src/endpoint/responses_websocket.rs` owns WebSocket connect and stream parsing.
- `codex-api/src/sse/responses.rs` owns shared Responses stream event parsing.
- `core/src/client.rs` owns transport selection, retries, WebSocket request construction, and fallback.
- `core/src/codex.rs` owns turn sampling and `/pause` / `/continue` semantics.

Do not port the upstream branch's `core/src/session/*` refactor. Use the upstream branch as a behavioral reference.

## Proposed Patch Sequence

Cross-validation against the alternative draft changes the order of the first patches:

- Suppress WebSockets when `CODEX_RS_SSE_FIXTURE` is set before adding more WebSocket behavior.
- Treat WebSocket connect `426 Upgrade Required` as immediate HTTP fallback instead of spending stream retry budget.
- Add body-level `client_metadata` with the first protocol patch because handshake headers are not resent per `response.create`.
- After adding `generate` and `client_metadata`, exclude both WebSocket-only fields from incremental eligibility comparisons unless a larger canonical request refactor is being ported.
- Treat connect timeout, custom CA, and `permessage-deflate` as transport hardening. `permessage-deflate` is low risk only if the pinned tungstenite API exposes the same extension config; custom CA should be skipped unless this branch has or receives a small standalone helper.
- Do not blindly cache cross-turn WebSocket state. Store/reuse only completed response chains, and reset on pause, interruption, fallback, compaction, rollback, or any stream error.

### Patch 0 - Fixture and fallback guardrails

Scope:

- `core/src/client.rs`
- `core/tests/suite/websocket_fallback.rs`

Changes:

1. Disable WebSocket transport while an SSE fixture is active:

```rust
fn responses_websocket_enabled(&self) -> bool {
    self.client.state.provider.supports_websockets
        && self.client.state.enable_responses_websockets
        && (*CODEX_RS_SSE_FIXTURE).is_none()
}
```

2. Add a small local outcome enum instead of porting the upstream branch session result model:

```rust
enum WebsocketStreamOutcome {
    Stream(ResponseStream),
    FallbackToHttp,
}
```

3. Map WebSocket connect `426 Upgrade Required` to `FallbackToHttp`, then call `try_switch_fallback_transport(...)` and stream the same request over HTTP.

Tests to add:

- `websocket_fallback_switches_to_http_on_upgrade_required_connect`
- fixture suppression coverage if it can be done without mutating process-wide environment in an in-process test

### Patch 1 - Terminal error and wrapped-error correctness

Scope:

- `codex-api/src/endpoint/responses_websocket.rs`
- `core/tests/suite/client_websockets.rs`

Changes:

1. Add wrapped error event parsing before ordinary `ResponsesStreamEvent` parsing:

```rust
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
    headers: Option<serde_json::Map<String, serde_json::Value>>,
}
```

2. Map `websocket_connection_limit_reached` to:

```rust
ApiError::Retryable { message, delay: None }
```

3. Map other non-success wrapped statuses to:

```rust
ApiError::Transport(TransportError::Http {
    status,
    url: None,
    headers: Some(mapped_headers),
    body: Some(original_payload),
})
```

4. On `run_websocket_response_stream(...)` error, do not call `close(None).await`. Drop the failed stream and send the original error:

```rust
let result = run_websocket_response_stream(...).await;
if let Err(err) = result {
    let failed_stream = guard.take();
    drop(guard);
    drop(failed_stream);
    let _ = tx_event.send(Err(err)).await;
}
```

5. Ensure a terminal error clears `websocket_last_request` and `websocket_last_response_rx` before the next request can compute an incremental delta. In the current this branch this can be done either:

- by clearing in `ModelClientSession::try_switch_fallback_transport(...)`; and
- by clearing when `websocket_connection(...)` sees the stored connection is closed;

or by adding a small helper:

```rust
fn reset_websocket_incremental_state(&mut self) {
    self.connection = None;
    self.websocket_last_request = None;
    self.websocket_last_response_rx = None;
}
```

Tests to add:

- `responses_websocket_usage_limit_error_emits_rate_limit_event`
- `responses_websocket_invalid_request_error_with_status_is_forwarded`
- `responses_websocket_connection_limit_error_reconnects_and_completes`
- `responses_websocket_v2_surfaces_terminal_error_without_close_handshake`
- `responses_websocket_v2_after_error_uses_full_create_without_previous_response_id`

Notes:

- This branch already maps `ApiError::Retryable` to retryable `CodexErr::Stream`; use that instead of inventing new retry plumbing.
- Do not make `previous_response_not_found` retryable. It means the connection-local state is unavailable. The safe fallback is a full `response.create` without `previous_response_id`.

### Patch 2 - Shared Responses stream parser parity

Scope:

- `codex-api/src/common.rs`
- `codex-api/src/sse/responses.rs`
- `core/src/client.rs`
- `core/src/codex.rs`

Changes:

1. Extend `ResponseEvent::Completed`:

```rust
Completed {
    response_id: String,
    token_usage: Option<TokenUsage>,
    end_turn: Option<bool>,
}
```

2. Parse `end_turn` from `response.completed.response.end_turn`.

3. Carry `end_turn` through `map_response_stream(...)`.

4. In `core/src/codex.rs`, treat `Some(false)` as follow-up-needed:

```rust
if let Some(false) = end_turn {
    needs_follow_up = true;
}
```

5. Add `response.incomplete` handling:

```rust
"response.incomplete" => {
    let reason = event.response.as_ref()
        .and_then(|response| response.get("incomplete_details"))
        .and_then(|details| details.get("reason"))
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    return Err(ResponsesEventError::Api(ApiError::Stream(
        format!("Incomplete response returned, reason: {reason}")
    )));
}
```

6. Optionally add explicit `ApiError::ServerOverloaded` and `ApiError::CyberPolicy { message }`. This is parser parity but not required for WebSocket correctness if this branch does not have the upstream branch’s user-facing cyber/trusted-access UX.

Required mechanical updates:

- Update all current-branch tests and constructors that match or build `ResponseEvent::Completed`.
- Update `codex-api/src/endpoint/aggregate.rs` and any fixture helpers to pass `end_turn: None`.

Tests to add:

- `response_completed_preserves_end_turn_false`
- `response_incomplete_is_stream_error`
- existing response-completed tests updated for `end_turn: None`

### Patch 3 - v2 `response.create` only for OpenAI WebSocket

Scope:

- `codex-api/src/common.rs`
- `core/src/client.rs`
- `core/tests/suite/client_websockets.rs`

Changes:

1. Add fields to `ResponseCreateWsRequest`:

```rust
#[serde(skip_serializing_if = "String::is_empty")]
pub instructions: String,

#[serde(skip_serializing_if = "Option::is_none")]
pub generate: Option<bool>,

#[serde(skip_serializing_if = "Option::is_none")]
pub client_metadata: Option<HashMap<String, String>>,
```

If this branch already has `instructions`, only add the skip attribute; do not duplicate the field.

2. Populate `client_metadata` on every `response.create` payload before changing incremental behavior:

```rust
fn websocket_client_metadata(
    turn_metadata_header: Option<&str>,
    session_source: &SessionSource,
) -> Option<HashMap<String, String>> {
    let mut metadata = HashMap::new();
    if let Some(value) = turn_metadata_header {
        metadata.insert("x-codex-turn-metadata".to_string(), value.to_string());
    }
    // Add subagent and parent-thread fields when available.
    (!metadata.is_empty()).then_some(metadata)
}
```

3. When checking incremental eligibility, ignore WebSocket-only fields:

```rust
let mut previous_without_input = previous_request.clone();
previous_without_input.input.clear();
previous_without_input.generate = None;
previous_without_input.client_metadata = None;

let mut request_without_input = request.clone();
request_without_input.input.clear();
request_without_input.generate = None;
request_without_input.client_metadata = None;
```

4. Keep `ResponseAppendWsRequest` and `ResponsesWsRequest::ResponseAppend` only if private provider tests still depend on it. Codex's OpenAI path should stop constructing it.

5. In `ModelClientSession::prepare_websocket_request(...)`, remove the v1 append branch for OpenAI Responses WebSocket:

```rust
let Some(last_response) = self.get_last_response() else {
    return (ResponsesWsRequest::ResponseCreate(payload.clone()), payload);
};
let Some(delta) = self.get_incremental_items(&payload, Some(&last_response), true) else {
    return (ResponsesWsRequest::ResponseCreate(payload.clone()), payload);
};
if last_response.response_id.is_empty() {
    return (ResponsesWsRequest::ResponseCreate(payload.clone()), payload);
}
let incremental = ResponseCreateWsRequest {
    previous_response_id: Some(last_response.response_id),
    input: delta,
    ..payload.clone()
};
(ResponsesWsRequest::ResponseCreate(incremental), payload)
```

6. Always send the v2 beta header value for OpenAI Responses WebSocket:

```rust
headers.insert(
    OPENAI_BETA_HEADER,
    HeaderValue::from_static(RESPONSES_WEBSOCKETS_V2_BETA_HEADER_VALUE),
);
```

7. Preserve the old feature flags as compatibility gates if needed, but make v2 the only emitted OpenAI WebSocket protocol.

Tests to update/add:

- Replace `responses_websocket_appends_on_prefix` with `responses_websocket_uses_incremental_create_on_prefix`.
- Keep `responses_websocket_creates_on_non_prefix`.
- Keep `responses_websocket_v2_creates_with_previous_response_id_on_prefix` as a regression test.
- Add `responses_websocket_v2_after_error_uses_full_create_without_previous_response_id`.

Pause/continue rule:

- `/continue` starts from durable history. If the previous stream was paused/cancelled before `response.completed`, `last_response_rx` will not contain a valid completed response. The next request must be full `response.create` without `previous_response_id`.

### Patch 4 - Header and metadata parity

Scope:

- `codex-api/src/endpoint/responses_websocket.rs`
- `codex-api/src/common.rs`
- `core/src/client.rs`

Changes:

1. Add `merge_request_headers(provider_headers, extra_headers, default_headers)` in the WebSocket endpoint module:

```rust
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
```

2. Change `ResponsesWebsocketClient::connect(...)` to accept `default_headers: HeaderMap` and use the helper.

3. If this branch has a default-header builder equivalent to upstream branch's `codex_login::default_client::default_headers()`, pass it. If not, add only the minimal defaults the HTTP path already uses, especially user-agent/originator if present.

4. Add `client_metadata` support to `ResponseCreateWsRequest` and populate fields available in this branch:

|Metadata key|Current-branch source|Required?|
|---|---|---|
|`x-codex-turn-metadata`|`turn_metadata_header` argument|Yes|
|`x-openai-subagent`|existing `build_subagent_headers()` / `SessionSource::SubAgent`|Yes for subagents|
|`x-codex-parent-thread-id`|`SessionSource::SubAgent(ThreadSpawn { parent_thread_id, .. })`|Yes if available|
|`x-codex-installation-id`|only if old config/state has it|Optional|
|`x-codex-window-id`|only if cross-turn cache/window generation is added|Optional until Patch 8|
|`traceparent`, `tracestate`|only if old telemetry exposes W3C trace context|Optional|

5. Keep headers for sticky routing and transport-level controls. Do not move `x-codex-turn-state` into `client_metadata`; it remains a header contract.

Tests to add:

- `merge_request_headers_matches_http_precedence`
- `responses_websocket_forwards_turn_metadata_on_initial_and_incremental_create`
- `responses_websocket_preserves_custom_turn_metadata_fields`
- subagent metadata test if `SessionSource::SubAgent` is active

### Patch 5 - Server model and verification event intake

Scope:

- `codex-api/src/common.rs`
- `codex-api/src/sse/responses.rs`
- `codex-api/src/endpoint/responses_websocket.rs`
- optionally `protocol/src/protocol.rs`, `core/src/codex.rs`, TUI/app-server consumers

Changes:

1. Read `openai-model` from WebSocket handshake response headers and store it on `ResponsesWebsocketConnection`.

2. Emit `ResponseEvent::ServerModel(model)` at stream start when the handshake provided one.

3. In `ResponsesStreamEvent`, add `headers: Option<Value>` and a helper that extracts `openai-model` / `x-openai-model` from:

- `response.headers`
- top-level `headers`

4. Deduplicate server-model events per stream.

5. Minimal surfaced behavior for this branch:

- log a warning when server model differs from `turn_context.model_info.slug`; or
- add a small `EventMsg::ModelReroute` equivalent if this branch needs upstream branch's user-visible cyber reroute warning.

6. Optional model verification support:

```rust
#[serde(rename_all = "snake_case")]
pub enum ModelVerification {
    TrustedAccessForCyber,
}

pub struct ModelVerificationEvent {
    pub verifications: Vec<ModelVerification>,
}
```

Parse `response.metadata.openai_verification_recommendation` and emit at most once per turn.

Tests to add:

- `spawn_response_stream_emits_header_events`
- `process_sse_emits_server_model_from_response_headers_payload`
- `responses_stream_event_response_model_reads_top_level_headers`
- `process_sse_emits_model_verification_field` if the surfaced event is added

Porting note:

- This patch is not a prerequisite for WebSocket correctness. Keep it after P0/P1 unless the product needs model reroute/trusted-access UX immediately.

### Patch 6 - Pump-based WebSocket I/O and compression

Scope:

- `codex-api/src/endpoint/responses_websocket.rs`
- `codex-api/Cargo.toml` if tungstenite extension features are missing

Changes:

1. Replace the raw alias:

```rust
type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;
```

with a wrapper that owns:

```rust
struct WsStream {
    tx_command: mpsc::Sender<WsCommand>,
    rx_message: mpsc::UnboundedReceiver<Result<Message, WsError>>,
    pump_task: tokio::task::JoinHandle<()>,
}
```

2. The pump loop should:

- receive commands for outbound sends;
- continuously read inbound frames;
- answer `Ping` with `Pong`;
- drop `Pong`;
- forward text/binary/close/errors to `rx_message`;
- abort the pump in `Drop`.

3. Change `run_websocket_response_stream(...)` to call wrapper `send(...)` and `next(...)`.

4. Add `websocket_config()`:

```rust
fn websocket_config() -> WebSocketConfig {
    let mut extensions = ExtensionsConfig::default();
    extensions.permessage_deflate = Some(DeflateConfig::default());

    let mut config = WebSocketConfig::default();
    config.extensions = extensions;
    config
}
```

5. Switch from `connect_async(request)` to `connect_async_tls_with_config(request, Some(websocket_config()), false, connector)`.

6. Custom CA prerequisite:

- If this branch already has `maybe_build_rustls_client_config_with_custom_ca()` and the rustls provider helper, call them here.
- If not, do not import the whole upstream branch auth/provider stack. Either defer custom CA support or port only the helper plus its smallest dependency set.

Tests to add:

- `websocket_config_enables_permessage_deflate`
- a fake-server ping test if the existing test harness can trigger ping frames
- existing streaming tests should continue to pass

### Patch 7 - Request prewarm with `generate: false`

Scope:

- `core/src/client.rs`
- `core/src/codex.rs`
- `core/tests/suite/client_websockets.rs`
- `core/tests/suite/abort_tasks.rs` or pause/continue-specific tests if available

Prerequisites:

- Patch 3 added `generate: Option<bool>`.
- Patch 1 guarantees failed prewarm streams clear incremental state.

Changes:

1. Add:

```rust
pub async fn prewarm_websocket(
    &mut self,
    prompt: &Prompt,
    model_info: &ModelInfo,
    otel_manager: &OtelManager,
    effort: Option<ReasoningEffortConfig>,
    summary: ReasoningSummaryConfig,
    service_tier: Option<ServiceTier>,
    turn_metadata_header: Option<&str>,
) -> Result<()> {
    if !self.responses_websocket_enabled() || self.websocket_last_request.is_some() {
        return Ok(());
    }
    // Call the same websocket stream path with generate = Some(false).
    // Drain until ResponseEvent::Completed, then keep the completed response id for the real turn.
}
```

2. Make the WebSocket stream helper accept a `warmup: bool` parameter that sets `ws_payload.generate = Some(false)` and suppresses rollout/usage side effects while draining the warmup stream.

3. Call `prewarm_websocket(...)` best-effort immediately before the first generated sampling request in `core/src/codex.rs`, after prompt construction is stable and before the request that should benefit from warmup.

4. Do not add upstream branch's startup prewarm scheduler in the first patch. A just-in-time prewarm inside the turn has fewer lifetime interactions with `/pause` and rollout resume.

5. If prewarm sees `426 Upgrade Required`, switch to HTTP fallback without surfacing a warning as a turn error.

6. If prewarm fails for a retryable stream reason, let the existing stream retry/fallback path handle the first generated request. Do not record warmup output to rollout.

Tests to add:

- `responses_websocket_request_prewarm_reuses_connection`
- `responses_websocket_prewarm_uses_v2_when_provider_supports_websockets`
- `websocket_first_turn_uses_request_prewarm_and_create`
- `pause_during_websocket_prewarm_clears_incremental_state`

Pause/continue rule:

- If `/pause` cancels during warmup, drop the WebSocket session and clear `websocket_last_request` / `websocket_last_response_rx`. `/continue` should issue a full `response.create` from durable history.

### Patch 8 - Optional cross-turn cached WebSocket session

Scope:

- `core/src/client.rs`
- compaction/rollback/session-history mutation sites in `core/src/codex.rs` and related modules
- tests in `core/tests/suite/client_websockets.rs` and pause/continue tests

Prerequisite:

- Do this only after P0/P1/P2 are stable. Cross-turn cache is an acceleration feature with more invalidation risk.

Changes:

1. Add a small cache to `ModelClientState`:

```rust
cached_websocket_session: std::sync::Mutex<WebsocketSession>,
```

2. Wrap the old per-session fields into:

```rust
#[derive(Default)]
struct WebsocketSession {
    connection: Option<ApiWebSocketConnection>,
    last_request: Option<ResponseCreateWsRequest>,
    last_response_rx: Option<oneshot::Receiver<LastResponse>>,
    connection_reused: bool,
}
```

3. `ModelClient::new_session()` takes the cached session; `Drop for ModelClientSession` stores it back.

4. Add explicit invalidators:

```rust
pub(crate) fn reset_cached_websocket_session(&self) { ... }
pub(crate) fn advance_window_generation(&self) { ... }
```

5. Call the invalidator on:

- standalone `/responses/compact` result adoption;
- local auto-compaction history replacement;
- thread rollback;
- fallback to HTTP;
- `/pause` and `/continue` cleanup if the prior stream did not complete;
- any branch-specific history rewrite.

6. Only reuse `previous_response_id` when the saved `last_response_rx` produced a completed response and the current request is an incremental extension of the saved request plus completed output items.

Tests to add:

- `responses_websocket_reuses_connection_after_session_drop`
- `responses_websocket_v2_incremental_requests_are_reused_across_turns`
- `compaction_resets_cached_websocket_session`
- `pause_then_continue_does_not_reuse_partial_previous_response_id`
- `rollback_resets_cached_websocket_session`

Design caution:

- This branch's comments correctly state that `x-codex-turn-state` is turn scoped. Do not reuse a stale `turn_state` header across new WebSocket handshakes. An already-open socket can be reused, but any reconnect for a new turn must use that turn's fresh `OnceLock`.

### Patch 9 - Optional custom-tool input delta UI

Scope:

- `codex-api/src/common.rs`
- `codex-api/src/sse/responses.rs`
- `core/src/codex.rs`
- UI protocol consumers if present

Changes:

1. Add:

```rust
ToolCallInputDelta {
    item_id: String,
    call_id: Option<String>,
    delta: String,
}
```

2. Parse only `response.custom_tool_call_input.delta`. Upstream branch does not emit an event for `response.function_call_arguments.delta` in this path.

3. Wire into this branch only if it already has a tool-argument-diff consumer. Otherwise, parse and ignore is not worth a surfaced protocol change.

## Minimal Dependency Matrix

|Desired change|Minimal prerequisite|Avoid porting|
|---|---|---|
|Wrapped error mapping|`serde::Deserialize`, existing `ApiError` and `TransportError`|upstream branch session refactor|
|Connection-limit retry|Wrapped error mapping|new retry subsystem|
|Drop-on-error|None|close-handshake orchestration|
|`response.incomplete`|Shared parser edit|upstream branch full error taxonomy|
|`end_turn`|`ResponseEvent::Completed` signature update|upstream branch turn module split|
|v2 create-only|Existing `previous_response_id` v2 fields|upstream branch's `ResponsesApiRequest` if too invasive|
|`generate: false` prewarm|`generate` field, v2 create-only|startup prewarm scheduler|
|`client_metadata`|`HashMap<String, String>` field on `ResponseCreateWsRequest`|upstream branch W3C trace plumbing if absent|
|Header merge defaults|local helper + existing default-header function|upstream branch auth/provider stack|
|Pump/ping-pong|local wrapper in endpoint module|core session refactor|
|`permessage-deflate`|tungstenite extension config enabled in Cargo features|custom CA if helper unavailable|
|Custom CA|port only the rustls helper and provider init|upstream branch login/provider rewrite|
|Cross-turn cache|small `WebsocketSession` and explicit invalidators|upstream branch full window/session machinery|
|Model verification UX|tiny protocol enum/event|full cyber/trusted-access UX if undesired|

## Test Plan

### Unit tests

Add or back-port focused tests in `codex-api`:

- `websocket_config_enables_permessage_deflate`
- `parse_wrapped_websocket_error_event_maps_to_transport_http`
- `parse_wrapped_websocket_error_event_with_connection_limit_maps_retryable`
- `merge_request_headers_matches_http_precedence`
- `response_incomplete_is_stream_error`
- `response_completed_preserves_end_turn`
- `responses_stream_event_response_model_reads_top_level_headers`

### Core WebSocket tests

Extend `core/tests/suite/client_websockets.rs`:

- wrapped usage-limit error emits token/rate-limit data then an error;
- invalid request error with `status` is forwarded;
- connection-limit error reconnects and completes;
- terminal `response.failed` surfaces without close-handshake delay;
- after a stream error, the next request is a full `response.create` without `previous_response_id`;
- initial and incremental creates carry turn metadata in `client_metadata`;
- prewarm sends `generate: false`, drains to completion, and the real request chains from the warmup response;
- `426 Upgrade Required` activates HTTP fallback immediately.

### Pause/continue regression tests

Add at least these branch-specific tests:

- pause during an active WebSocket stream clears incremental state;
- `/continue` after a paused WebSocket stream sends full durable history, not a `previous_response_id` from a partial stream;
- `/continue` after a completed WebSocket turn may reuse previous response only if no history rewrite occurred;
- pause during prewarm cancels/drops the warmup connection and does not write warmup output to rollout;
- rollback or compaction after a prewarmed/cached connection invalidates the cache.

### Manual smoke test

Run a short conversation over a fake WebSocket server:

1. initial `response.create` completes;
2. tool call response triggers a second `response.create` with `previous_response_id` and delta input;
3. send `websocket_connection_limit_reached`; verify reconnect;
4. send `previous_response_not_found`; verify next request is full create;
5. run `/pause`, then `/continue`; verify no synthetic user prompt and no stale previous response ID.

## Rollout Strategy

1. Land P0 correctness first with no feature-flag surface change.
2. Land v2 create-only and metadata behind the existing WebSocket feature gate.
3. Land transport parity (`permessage-deflate`, pump, optional custom CA).
4. Land prewarm behind a new internal feature flag if this branch needs staged rollout.
5. Land cross-turn cache only after pause/continue and compaction invalidation tests are passing.
6. Land optional model-verification/model-reroute UI separately.

## Acceptance Criteria

The back-port is acceptable when:

- no WebSocket stream error waits on a close handshake before reaching the caller;
- wrapped WebSocket usage-limit and invalid-request errors produce the same user-visible classification as HTTP;
- `websocket_connection_limit_reached` reconnects under the configured stream retry budget;
- Codex's OpenAI WebSocket requests use `response.create` for both initial and incremental turns;
- warmup, if enabled, uses `generate: false` and never persists model-visible rollout content;
- `/pause` and `/continue` never reuse a partial or failed previous-response ID;
- fallback to HTTP remains sticky after retry exhaustion or `426 Upgrade Required`;
- compaction and rollback invalidate any cached connection-local previous-response state;
- tests document all non-obvious invalidation paths.
