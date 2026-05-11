# Responses WebSocket Minimal Backport - Design Proposal

## Design Principle

Patch this project in place and keep the current architecture:

- `codex-api/src/endpoint/responses_websocket.rs` owns WebSocket connect, request send, and WebSocket event parsing.
- `codex-api/src/sse/responses.rs` owns shared Responses stream-event parsing.
- `core/src/client.rs` owns transport selection, retries, WebSocket request construction, incremental state, and HTTP fallback.
- `core/src/session/*` owns turn lifecycle, rollout durability, `/pause`, and `/continue` behavior.

Use the upstream project as a behavioral reference, not a source tree to import wholesale.

## Patch Sequence

### Patch 1 - Bound WebSocket request sends

Scope:

- `codex-api/src/endpoint/responses_websocket.rs`
- `core/tests/suite/client_websockets.rs`

Problem:

This project bounds WebSocket connect and response receive, but `run_websocket_response_stream(...)` directly awaits `ws_stream.send(...)`. The upstream project factors sends into `send_websocket_request(...)` and wraps the send with `tokio::time::timeout(idle_timeout, ...)`.

Minimal change:

1. Add a helper near `run_websocket_response_stream(...)`:

```rust
async fn send_websocket_request(
    ws_stream: &WsStream,
    request_body: Value,
    idle_timeout: Duration,
    telemetry: Option<&Arc<dyn WebsocketTelemetry>>,
) -> Result<(), ApiError> {
    let request_text = serde_json::to_string(&request_body).map_err(|err| {
        ApiError::Stream(format!("failed to encode websocket request: {err}"))
    })?;

    let request_start = Instant::now();
    let result = tokio::time::timeout(
        idle_timeout,
        ws_stream.send(Message::Text(request_text.into())),
    )
    .await
    .map_err(|_| ApiError::Stream("idle timeout sending websocket request".into()))
    .and_then(|result| {
        result.map_err(|err| ApiError::Stream(format!("failed to send websocket request: {err}")))
    });

    if let Some(t) = telemetry.as_ref() {
        t.on_ws_request(request_start.elapsed(), result.as_ref().err());
    }

    result
}
```

2. Replace the direct send block in `run_websocket_response_stream(...)` with the helper.

3. Do not port the upstream project's `connection_reused` telemetry argument yet; that belongs with Patch 7 or Patch 8.

Tests:

- Add a unit or harness test that injects a stalled send path and expects `idle timeout sending websocket request`.
- Keep existing wrapped-error and connection-limit tests unchanged.

Risk:

Low. The helper preserves the same error type and telemetry callback shape already used by this project.

### Patch 2 - Add `response.processed` acknowledgement

Scope:

- `codex-api/src/common.rs`
- `codex-api/src/endpoint/responses_websocket.rs`
- `core/src/features.rs`
- `core/config.schema.json`
- `core/src/client.rs`
- `core/src/session/turn.rs`
- `core/src/compact_remote_v2.rs`, only if this project has the same remote-compaction v2 call shape at the target revision
- `core/tests/suite/client_websockets.rs`

Problem:

The upstream project can send a WebSocket `response.processed` message after a successful turn and after remote compaction. This project has no request type or call site for that acknowledgement.

Minimal change:

1. Add the API request shape:

```rust
#[derive(Debug, Serialize)]
pub struct ResponseProcessedWsRequest {
    pub response_id: String,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type")]
#[allow(clippy::large_enum_variant)]
pub enum ResponsesWsRequest {
    #[serde(rename = "response.create")]
    ResponseCreate(ResponseCreateWsRequest),
    #[serde(rename = "response.append")]
    ResponseAppend(ResponseAppendWsRequest),
    #[serde(rename = "response.processed")]
    ResponseProcessed(ResponseProcessedWsRequest),
}
```

Keep `ResponseAppend` initially because this project's API crate still exports it and private-provider or downstream tests may depend on the type, even though the OpenAI path should not construct it.

2. Add `ResponsesWebsocketConnection::send_response_processed(response_id: String) -> Result<(), ApiError>`.

3. Reuse the Patch 1 `send_websocket_request(...)` helper so acknowledgement sends are also bounded.

4. Add `ModelClientSession::send_response_processed(&self, response_id: &str)` that:

- returns immediately if no WebSocket connection exists;
- logs at debug level on failure;
- does not fail the completed user turn if the acknowledgement send fails.

5. Add an under-development feature flag:

```rust
Feature::ResponsesWebsocketResponseProcessed
key: "responses_websocket_response_processed"
default_enabled: false
```

6. In the turn loop, after assistant text/tool streams are flushed and only when the turn outcome is successful, send the acknowledgement if the feature is enabled and a completed response ID exists.

7. In remote compaction v2, send the acknowledgement after the compacted history has been durably integrated, only if the feature is enabled and the active `ModelClientSession` has a WebSocket connection.

Ordering rule:

Do not send `response.processed` for a stream that ended before `response.completed`, for a cancelled/paused/interrupted turn, or before rollout/history state has consumed the completed response.

Tests:

- `responses_websocket_sends_response_processed_when_feature_enabled`
- `responses_websocket_omits_response_processed_without_feature`
- `responses_websocket_sends_response_processed_after_remote_compaction_v2`, if remote compaction v2 exists in the target source shape
- a negative test for cancelled/errored turns if the harness can express it without flaky timing

Risk:

Moderate-low. The main risk is lifecycle ordering. Keep the call best-effort and feature-gated.

### Patch 2a - Cancel mapped streams when consumers drop

Scope:

- `core/src/client_common.rs`
- `core/src/client.rs`
- focused core unit tests

Problem:

The core stream mapper currently keeps polling the provider stream even after the consumer drops the mapped `ResponseStream`. During `/pause`, interruption, or cancellation, that can keep a provider/WebSocket stream alive until a later event, error, or timeout.

Minimal change:

1. Add a `CancellationToken` field to the core `ResponseStream`.

2. Cancel the token in `Drop` for `ResponseStream`.

3. In `map_response_stream(...)`, create the token and use `tokio::select!` to stop the mapper when the consumer is dropped.

4. Keep WebSocket previous-response state unchanged: only a completed response still populates the last-response receiver.

Tests:

- dropping the mapped stream stops polling a pending provider stream;
- completed streams still populate last-response state.

Risk:

Low. The cancellation is local to the mapper and only shortens abandoned work.

### Patch 3 - Preserve server model metadata

Scope:

- `codex-api/src/common.rs`
- `codex-api/src/endpoint/responses_websocket.rs`
- `codex-api/src/sse/responses.rs`
- `core/src/client_common.rs` and any event mapping path that mirrors `ResponseEvent`
- UI/app-server event paths only if this project already has a suitable event surface

Problem:

The upstream project emits `ResponseEvent::ServerModel` when the server reports an effective model through handshake or stream headers. This project ignores those signals.

Minimal change:

1. Add:

```rust
ResponseEvent::ServerModel(String)
```

2. In `ResponsesWebsocketConnection`, store `server_model: Option<String>`.

3. In `connect_websocket(...)`, read the `openai-model` handshake header.

4. At the start of `stream_request(...)`, emit `ResponseEvent::ServerModel(model)` before other server-state events when present.

5. Extend `ResponsesStreamEvent` with optional `headers: Option<Value>` and `metadata: Option<Value>` if not already present.

6. Add `ResponsesStreamEvent::response_model()` with this precedence:

- response-level headers;
- top-level headers;
- metadata field used by the current service shape, if present.

7. During stream processing, emit deduplicated `ServerModel` events when the effective model changes.

8. Initially map the event through core without importing the upstream project's full model-reroute/trusted-access UX. A debug log or existing raw event surface is enough for the minimal backport.

Tests:

- handshake `openai-model` emits `ServerModel` for WebSocket;
- response headers payload emits `ServerModel`;
- payload-level plain `response.model` is ignored if upstream behavior ignores it;
- duplicate model events are not repeatedly emitted.

Risk:

Medium. Adding a core event variant can require mechanical updates to exhaustive matches. Keep user-facing behavior minimal unless this project already has a natural display path.

### Patch 4 - Preserve model verification metadata

Scope:

- `protocol/src/protocol.rs`, if `ModelVerification` does not exist in this project
- `codex-api/src/common.rs`
- `codex-api/src/sse/responses.rs`
- `codex-api/src/endpoint/responses_websocket.rs`
- downstream event mapping paths only where required by exhaustive matching

Problem:

The upstream project parses server-provided model verification recommendations. This project drops them.

Minimal change:

1. Add or reuse a `ModelVerification` protocol enum/struct.

2. Add:

```rust
ResponseEvent::ModelVerifications(Vec<ModelVerification>)
```

3. Add `ResponsesStreamEvent::model_verifications()` that parses only known values and ignores malformed or unknown shapes.

4. Emit `ResponseEvent::ModelVerifications(...)` before ordinary `process_responses_event(...)` handling.

5. Do not port the upstream account-verification UX as part of this patch.

Tests:

- known verification field emits the event;
- unknown values are ignored;
- non-array shapes are ignored;
- ordinary stream processing still reaches `response.completed`.

Risk:

Medium. This may require protocol schema updates. If the current project has no consumer and no protocol type, this patch can be deferred behind Patch 3.

### Patch 5 - Preserve custom-tool input deltas

Scope:

- `codex-api/src/common.rs`
- `codex-api/src/sse/responses.rs`
- `core/src/client_common.rs` and UI/app-server mapping only if they can consume the event

Problem:

The upstream project parses `response.custom_tool_call_input.delta`. This project ignores it, so custom-tool input streams are only visible if they appear in final items.

Minimal change:

1. Add:

```rust
ResponseEvent::ToolCallInputDelta {
    item_id: String,
    call_id: Option<String>,
    delta: String,
}
```

2. Parse only `response.custom_tool_call_input.delta`.

3. Route it through core only if a consumer can apply the delta. Otherwise keep the parser/event addition local to `codex-api` and add tests.

Tests:

- one delta event maps to `ToolCallInputDelta`;
- missing `item_id` or `delta` is ignored;
- normal `response.completed` still terminates the stream.

Risk:

Medium. The event is safe to parse, but surfacing it may require UI/tool-call state changes.

### Patch 5a - Classify provider stream errors

Scope:

- `codex-api/src/error.rs`
- `codex-api/src/sse/responses.rs`
- `core/src/api_bridge.rs`
- `core/src/error.rs`, only if a dedicated core error is needed

Problem:

The upstream project distinguishes `cyber_policy`, `server_is_overloaded`, and `slow_down` response failures. This project currently falls through to generic retryable stream errors for these codes.

Minimal change:

1. Add `ApiError` variants for the provider classifications.

2. In `process_responses_event(...)`, map:

- `cyber_policy` to a non-retryable policy error with a fallback message when the server message is empty;
- `server_is_overloaded` and `slow_down` to an overload classification.

3. Map the new API errors into existing core errors unless this project already has a dedicated user-facing variant.

Tests:

- `cyber_policy` does not become a generic retryable stream error;
- blank `cyber_policy` messages use the fallback;
- `server_is_overloaded` and `slow_down` map to overload handling.

Risk:

Low. The parser already handles provider failure events; this only avoids over-broad retry behavior.

### Patch 6 - Transport config parity: `permessage-deflate`

Scope:

- root `Cargo.toml`
- `codex-api/Cargo.toml`
- `codex-api/src/endpoint/responses_websocket.rs`

Problem:

The upstream project enables WebSocket `permessage-deflate`. This project uses the default tungstenite config.

Minimal change:

1. First decide whether the dependency graph can support extension configuration without dragging in unrelated upstream patches.

2. If supported, add a direct `tungstenite` dependency with the necessary compression/deflate feature, or use the upstream fork if that is the only compatible route for this revision.

3. Switch connect from:

```rust
tokio_tungstenite::connect_async(request).await
```

to:

```rust
connect_async_tls_with_config(
    request,
    Some(websocket_config()),
    false,
    connector,
).await
```

where `connector` is `None` unless Patch 7 is also applied.

4. Add:

```rust
fn websocket_config() -> WebSocketConfig {
    let mut extensions = ExtensionsConfig::default();
    extensions.permessage_deflate = Some(DeflateConfig::default());

    let mut config = WebSocketConfig::default();
    config.extensions = extensions;
    config
}
```

Tests:

- `websocket_config_enables_permessage_deflate`
- one handshake test that validates the connection still succeeds against the existing local WebSocket test server

Risk:

Medium. The risk is dependency-level, not algorithmic. Do not let this patch force the upstream provider/auth/session refactors.

### Patch 7 - Optional custom-CA parity for WebSocket TLS

Scope:

- `codex-client/src/custom_ca.rs`, if ported
- `utils/rustls-provider`, if ported
- `codex-api/Cargo.toml`
- `codex-api/src/endpoint/responses_websocket.rs`

Problem:

The upstream project makes WebSocket TLS honor the same custom-CA policy as HTTPS. This project does not currently include the upstream custom-CA helper.

Minimal stance:

Do not make this a prerequisite for P0/P1. It is only a WebSocket-minimal backport if this project also wants custom-CA support generally.

If ported, keep the prerequisite narrow:

1. Add the upstream custom-CA helper and rustls provider utility without importing unrelated provider/auth changes.

2. Call `ensure_rustls_crypto_provider()` before building the connection.

3. Build an optional connector:

```rust
let connector = maybe_build_rustls_client_config_with_custom_ca()
    .map_err(|err| ApiError::Stream(format!("failed to configure websocket TLS: {err}")))?
    .map(tokio_tungstenite::Connector::Rustls);
```

4. Pass that connector to `connect_async_tls_with_config(...)`.

Tests:

- helper returns `None` when no custom CA is configured;
- helper error maps to `ApiError::Stream`;
- optional integration test only if a local custom-CA TLS fixture already exists.

Risk:

Medium-high. Custom CA is valuable for enterprise environments, but not necessary for the current WebSocket protocol correctness fixes.

### Patch 8 - Observability metadata parity

Scope:

- `codex-api/src/common.rs`
- `core/src/client.rs`
- `codex-otel` only if this project already exposes W3C trace context helpers or the helper can be ported narrowly

Problem:

This project's `client_metadata` carries only a subset of the upstream fields. The upstream project also sends W3C trace context, installation ID, window ID, and a WebSocket request-start timestamp.

Minimal change:

1. Add trace metadata helper to `codex-api/src/common.rs` only if a W3C trace context type/helper exists or can be ported narrowly:

```rust
pub const WS_REQUEST_HEADER_TRACEPARENT_CLIENT_METADATA_KEY: &str = "ws_request_header_traceparent";
pub const WS_REQUEST_HEADER_TRACESTATE_CLIENT_METADATA_KEY: &str = "ws_request_header_tracestate";
```

2. Add `response_create_client_metadata(...)` that merges existing metadata with trace fields.

3. Add a request-start timestamp just before `stream_request(...)` sends the frame:

```rust
request.client_metadata
    .get_or_insert_with(HashMap::new)
    .insert("x-codex-ws-stream-request-start-ms".to_string(), now_ms.to_string());
```

Use this project's existing time helper if one exists; otherwise keep the timestamp field out of the minimal patch.

4. Installation ID and window ID require durable session/window state. Add them only if those concepts already exist in this project or are introduced by Patch 10.

Tests:

- turn metadata is preserved;
- traceparent/tracestate are included when supplied;
- per-request timestamp changes between consecutive requests but does not block incremental reuse.

Risk:

Medium. The metadata itself is safe, but adding installation/window generation as new state is outside the minimal P0 scope.

### Patch 9 - Preconnect without request prewarm

Scope:

- `core/src/client.rs`
- call site chosen by the startup/prewarm path, if this project has one
- `core/tests/suite/client_websockets.rs`

Problem:

This project opens the WebSocket lazily on the first generated request. The upstream project can preconnect before the first request.

Minimal change:

1. Add `ModelClientSession::preconnect_websocket(...)` that:

- returns early if WebSockets are disabled or fallback is active;
- returns early if a connection already exists;
- resolves auth/provider through the existing current path;
- calls the same `websocket_connection(...)`/connect logic used by `stream_responses_websocket(...)`;
- never sends prompt payloads.

2. Do not add cross-turn connection caching in this patch.

3. Do not change `/pause` and `/continue` semantics.

Tests:

- preconnect opens exactly one connection;
- a later generated request reuses that connection;
- 426 during preconnect activates the same HTTP fallback behavior or is handled as a best-effort preconnect miss, depending on the chosen call site.

Risk:

Medium-low. This only moves connection setup earlier; it does not alter prompt/history state.

### Patch 10 - Request prewarm with `generate: false`

Scope:

- `core/src/client.rs`
- startup/prewarm caller, if this project has a compatible one
- `core/tests/suite/client_websockets.rs`

Problem:

The request type has `generate`, but this project never sends `generate: false` prewarm requests.

Minimal change:

1. Add `ModelClientSession::prewarm_websocket(...)` that calls the existing WebSocket stream path with `warmup = true`.

2. When `warmup = true`, set `ResponseCreateWsRequest.generate = Some(false)`.

3. Consume the resulting stream until `ResponseEvent::Completed`.

4. Treat the warmup response as setup state only:

- do not emit assistant output into rollout;
- do not record an inference trace attempt if this project has tracing;
- keep `websocket_last_request` and `websocket_last_response_rx` so the later generated request can use `previous_response_id` if its input extends the warmed request;
- clear state on any error.

5. Keep this turn-scoped initially.

Tests:

- prewarm sends `response.create` with `generate: false`;
- the generated turn chains from the warmup response ID when the prompt is still compatible;
- a changed prompt sends a full `response.create`;
- prewarm failure does not corrupt subsequent HTTP fallback or `/continue` behavior.

Risk:

Medium. The key risk is accidentally treating warmup as model-visible history. Keep all rollout writes out of the prewarm path.

### Patch 11 - Optional cross-turn WebSocket cache

Scope:

- `core/src/client.rs`
- session lifecycle call sites
- compaction/rollback/pause/continue paths that must invalidate the cache
- tests for cache invalidation

Problem:

The upstream project can move a WebSocket session from one turn-scoped `ModelClientSession` to the next. This project intentionally creates a fresh WebSocket session per turn to avoid sticky turn-state leakage.

Minimal stance:

Do not include this in the first backport. Add it only after P0/P1/P2 are stable and after explicit invalidation points are audited.

If implemented:

1. Add a `WebsocketSession` struct containing:

```rust
struct WebsocketSession {
    connection: Option<ApiWebSocketConnection>,
    last_request: Option<ResponseCreateWsRequest>,
    last_response_rx: Option<oneshot::Receiver<LastResponse>>,
    connection_reused: bool,
}
```

2. Store it behind a `Mutex` in `ModelClient` and move it into new `ModelClientSession` instances.

3. Remove or isolate `x-codex-turn-state` from cached cross-turn connection reuse. This project's current comments explicitly say the sticky token must not leak across turns.

4. Add a window-generation or equivalent invalidation token before trusting cross-turn previous-response state.

5. Invalidate on:

- `/pause`;
- `/continue` start and completion cleanup;
- stream error;
- cancellation/interruption;
- compaction;
- rollback;
- HTTP fallback;
- auth recovery that changes auth material;
- any non-prefix request.

Tests:

- second normal turn can reuse a completed prior chain;
- paused/interrupted turn does not reuse cached state;
- compaction starts a new chain without `previous_response_id` unless using server-side compaction semantics that preserve the chain;
- 426 fallback clears the cache;
- closed connection clears last request/response state.

Risk:

High. This is the main place where the upstream project architecture differs from this project. Keep it separate from protocol correctness.

## Minimal Dependency Matrix

|Change|Required dependency scope|Avoid importing|
|---|---|---|
|Bound send timeout|None beyond current dependencies|Upstream telemetry refactor|
|`response.processed`|New request struct/enum variant, feature flag, two call sites|Upstream session refactor|
|`ServerModel`|One event variant, handshake header read, small stream helper|Trusted-access/model-reroute UX|
|Model verifications|Protocol type only if absent; parser helper|Full account-verification UI|
|Custom-tool input delta|Event variant and parser|Tool UI rewrite unless needed|
|`permessage-deflate`|Direct/forked tungstenite support for extension config|Provider/auth/session rewrites|
|Custom CA|Only the custom-CA helper and rustls provider utility|Global network stack refactor|
|Trace metadata|Only W3C helper if available|Full inference trace subsystem|
|Preconnect/prewarm|Current `ModelClientSession` plus one call site|Cross-turn cache|
|Cross-turn cache|Explicit cache/invalidation state|Full upstream `core/src/session/*` refactor|

## Validation Plan

Run the smallest deterministic checks after each patch:

```bash
CODEX_SANDBOX_NETWORK_DISABLED=1 scripts/cargo-local test -p codex-api responses_websocket
CODEX_SANDBOX_NETWORK_DISABLED=1 scripts/cargo-local test -p codex-api sse::responses
CODEX_SANDBOX_NETWORK_DISABLED=1 scripts/cargo-local test -p codex-core --test all client_websockets
CODEX_SANDBOX_NETWORK_DISABLED=1 scripts/cargo-local test -p codex-core --test all websocket_fallback
```

For patches that touch features or schema:

```bash
CODEX_SANDBOX_NETWORK_DISABLED=1 scripts/cargo-local test -p codex-core features
CODEX_SANDBOX_NETWORK_DISABLED=1 scripts/cargo-local test -p codex-core config
```

Expected regression constraints:

- Existing `response.create` v2 tests still pass.
- Existing 426 fallback tests still pass.
- Existing wrapped-error and connection-limit tests still pass.
- Existing `/pause` and `/continue` tests still pass.
- No warmup or preconnect path writes assistant output or synthetic user prompts to rollout.

## Recommended Landing Order

1. Patch 1: bound WebSocket request sends.
2. Patch 2: `response.processed` acknowledgement behind a feature flag.
3. Patch 2a: consumer-drop cancellation for mapped streams.
4. Patch 3: `ServerModel` metadata.
5. Patch 4 and Patch 5: model verifications and custom-tool input deltas, if consumers are ready or parser parity is desired.
6. Patch 5a: provider stream error classification.
7. Patch 6: `permessage-deflate`, only after dependency feasibility is confirmed.
8. Patch 7: custom CA only if custom-CA support is wanted globally.
9. Patch 8: observability metadata, starting with trace fields and request-start timestamp.
10. Patch 9 and Patch 10: preconnect and request prewarm.
11. Patch 11: cross-turn cache, only after explicit invalidation tests are in place.
