# Responses WebSocket Minimal Backport - Design Proposal

## Design rules

1. Patch the existing architecture rather than importing the upstream client stack.
2. Keep `codex-api` responsible for transport mechanics and stream-event parsing.
3. Keep `core/src/client.rs` responsible for transport selection, per-turn state, retry, and fallback.
4. Keep the current WebSocket feature gates.
5. Preserve full-request fallback whenever incremental eligibility is uncertain.
6. Treat physical connection reuse and logical response-chain reuse as separate capabilities.
7. Make `/pause`, `/continue`, compaction, and fallback explicit in cache invalidation tests.

## Patch series overview

|Patch|Scope|Dependency|User-visible effect|
|---|---|---|---|
|1|Remove non-create request paths|None|Removes dormant request types and an under-development feature/config key.|
|2|Direct request serialization|Patch 1 recommended but not required|None; allocation/CPU reduction.|
|3|Borrowed incremental comparison|None|None; allocation/CPU reduction.|
|4|Request-scoped turn state|None|Internal metadata transport change.|
|5|Physical connection cache|Patch 4|Lower handshake latency after the first completed turn.|
|6|Custom CA and rustls provider|Independent|Secure WebSocket and HTTP connectivity behind enterprise CA interception.|
|7|Configurable connect timeout|Independent|New optional provider configuration.|

## Patch 1 - Remove obsolete non-create WebSocket requests

### Rationale

The current public WebSocket protocol uses client `response.create` messages. The upstream project removed its processed acknowledgement because it no longer carried useful behavior. This project retains the entire path behind an under-development feature flag.

Workspace inspection also finds no construction of `response.append`; keeping that dead public shape would preserve needless wire-shape drift from upstream. This branch treats `codex-api` as workspace-internal for this backport, so remove `response.append` in the same cleanup patch.

### Changes

|Path|Change|
|---|---|
|`codex-api/src/common.rs`|Remove `ResponseAppendWsRequest`, `ResponseProcessedWsRequest`, `ResponsesWsRequest::ResponseAppend`, and `ResponsesWsRequest::ResponseProcessed`.|
|`codex-api/src/lib.rs`|Remove the append- and processed-request re-exports.|
|`codex-api/src/endpoint/responses_websocket.rs`|Remove the processed-request import and `ResponsesWebsocketConnection::send_response_processed(...)`.|
|`core/src/client.rs`|Remove `ModelClientSession::send_response_processed(...)`.|
|`core/src/session/turn.rs`|Remove the successful-turn acknowledgement branch and the now-unused feature import if applicable.|
|`core/src/features.rs`|Remove `Feature::ResponsesWebsocketResponseProcessed` and its feature specification.|
|`core/config.schema.json`|Regenerate or remove both schema occurrences of `responses_websocket_response_processed`.|
|`core/tests/suite/client_websockets.rs`|Remove the enabled/disabled processed-request tests and helper setup.|

Do not add any new acknowledgement or append protocol in their place.

### Acceptance

- No `ResponseAppend`, `ResponseProcessed`, `send_response_processed`, or `responses_websocket_response_processed` symbol remains.
- Successful turns emit no post-completion WebSocket request.
- Existing ordinary and incremental request tests still pass.

## Patch 2 - Serialize WebSocket requests directly

### Current cost

`ResponsesWebsocketConnection::stream_request(...)` performs:

1. `serde_json::to_value(&request)`.
2. Move the full JSON tree into the response task.
3. `serde_json::to_string(&request_body)` before send.

For a full-history request, the intermediate tree duplicates work and retains request-sized allocations until the send starts.

### Minimal implementation

Add a local helper in `codex-api/src/endpoint/responses_websocket.rs`:

```rust
fn serialize_websocket_request(request: &ResponsesWsRequest) -> Result<String, ApiError> {
    serde_json::to_string(request)
        .map_err(|err| ApiError::Stream(format!("failed to encode websocket request: {err}")))
}
```

Then:

- Serialize once in `stream_request(...)` before spawning.
- Change `run_websocket_response_stream(...)` to accept `String`.
- Change `send_websocket_request(...)` to accept `String`.
- If the processed path is temporarily retained, make it use the same helper.
- Do not change the `WsStream` pump, send timeout, telemetry callback, or error text.

### Explicit non-prerequisites

- Do not convert this project's borrowed `ResponsesApiRequest` to the upstream owned request type.
- Do not port tracing spans or response debug context.
- Do not change request fields.

### Test

Add `direct_serialization_preserves_websocket_request_payload` beside the existing endpoint unit tests. Construct a representative `ResponseCreateWsRequest`, compare `serde_json::to_value(...)` with `serde_json::from_str(serialize_websocket_request(...))`, and include input, tools, reasoning-related fields, `generate`, and `client_metadata`.

## Patch 3 - Compare incremental requests without cloning history

### Current cost

`get_incremental_items(...)` clones:

- The prior `ResponseCreateWsRequest`.
- The current `ResponseCreateWsRequest`.
- The previous request's entire input.
- The completed response's output items.
- The outgoing delta.

Only the outgoing delta must be owned.

### Minimal implementation

Keep `ResponseCreateWsRequest` as the comparison type. Add an exhaustive helper that compares only properties required for continuation:

```rust
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
```

The exhaustive destructuring is intentional: adding a request field must force an explicit decision about continuation equality.

Replace baseline construction with borrowed prefix checks:

```rust
let after_previous_request = request
    .input
    .as_slice()
    .strip_prefix(previous_request.input.as_slice())?;

let delta = match last_response {
    Some(last_response) => after_previous_request
        .strip_prefix(last_response.items_added.as_slice())?,
    None => after_previous_request,
};

if !allow_empty_delta && delta.is_empty() {
    return None;
}

Some(delta.to_vec())
```

### Semantics to preserve

- `input` is compared separately.
- `generate` remains ignored for logical continuation comparison.
- `client_metadata` remains ignored because it is request-scoped.
- `previous_response_id` is transport output, not part of logical request equality.
- A changed model, instructions, tools, tool choice, reasoning, storage, include list, service tier, cache key, or text controls forces a full create.
- A reordered, shortened, or rewritten history forces a full create.

### Tests

Retain the existing prefix, non-prefix, changed-property, and after-error integration tests. Add focused unit cases for:

- Exact prefix plus server-returned items plus one new tool output.
- Empty delta allowed and disallowed.
- Reordered server-returned items.
- Changed `client_metadata` remaining eligible.
- Changed `generate` remaining eligible.
- Every compared non-input property causing ineligibility.

## Patch 4 - Send turn state per WebSocket request

### Rationale

Today a fresh turn can capture `x-codex-turn-state` from the WebSocket upgrade response. That works while one physical connection belongs to one logical turn. It does not work after the connection spans turns.

The minimum reusable model is:

- One fresh `Arc<OnceLock<String>>` per `ModelClientSession`.
- The established value is inserted into each later `response.create.client_metadata`.
- A `response.metadata` event can establish the value for an already-open connection.
- First value wins.

### API parser changes

In `codex-api/src/sse/responses.rs`:

- Add `ResponsesStreamEvent::turn_state() -> Option<String>`.
- Only accept the value on `response.metadata`.
- Read `x-codex-turn-state` case-insensitively from the event's top-level `headers` object.
- Reuse the existing JSON-string-or-first-array-element conversion style.

In `codex-api/src/endpoint/responses_websocket.rs`:

- Change `ResponsesWebsocketConnection::stream_request(...)` to accept `Option<Arc<OnceLock<String>>>`.
- Pass a borrowed lock into `run_websocket_response_stream(...)`.
- Before normal event processing, call `event.turn_state()` and attempt `turn_state.set(value)`.
- Keep handshake response capture during transition, but do not depend on it.

### Core request changes

In `core/src/client.rs`:

- Build WebSocket client metadata as today.
- If `self.turn_state.get()` is set, insert `x-codex-turn-state` into that request's `client_metadata`.
- Pass `Some(Arc::clone(&self.turn_state))` to `stream_request(...)`.
- Do not use a previous turn's lock when adopting a cached connection.

For a newly opened WebSocket connection, continue passing the current turn's lock into the API connector so upgrade-response compatibility headers can still seed it. A reused connection must not carry any previous turn lock; request-body `client_metadata` and `response.metadata` establish state for the current turn.

### Compatibility headers

Turn metadata may remain in handshake compatibility headers on a newly opened socket, matching current behavior. Request-body `client_metadata` is authoritative for per-request values on a reused socket.

### Tests

- First request has no turn-state metadata.
- A `response.metadata` event establishes `state-a`.
- A same-turn second request includes `state-a` in `client_metadata`.
- A later metadata event with `state-b` does not replace `state-a`.
- A fresh logical turn on the same physical socket starts without `state-a`.

## Patch 5 - Cache only the physical connection across turns

### Ownership model

Add a one-slot cache to `ModelClientState`. The cache should contain:

```rust
struct CachedWebsocketConnection {
    provider: ModelProviderInfo,
    auth_mode: Option<AuthMode>,
    connection: ApiWebSocketConnection,
}
```

A `std::sync::Mutex<Option<CachedWebsocketConnection>>` is sufficient. Use `PoisonError::into_inner` as upstream does rather than introducing fallible cache operations.

The provider and auth mode are required local safeguards because this project supports `new_session_with_provider(...)` and resolves different OpenAI base URLs for ChatGPT versus API authentication.

### Session construction

`ModelClient::new_session_with_provider(...)` should:

1. Create a fresh turn-state lock.
2. Take the cached slot.
3. Adopt the connection only when the provider matches and the eventual request auth mode matches.

Because auth mode is resolved asynchronously, either:

- Store the taken slot temporarily on `ModelClientSession` and validate it on the first request attempt, or
- Key the cache by provider only and validate/drop it after `resolve_request_auth(...)` returns.

Do not reconnect merely because request-scoped metadata changed. That metadata is carried in `response.create.client_metadata` after Patch 4.

### Logical state remains turn-scoped

The following fields must still start empty for every `ModelClientSession`:

- `websocket_last_request`.
- `websocket_last_response_rx`.
- `turn_state`.

Therefore, the first request in every new logical turn is a full `response.create` without `previous_response_id`. Same-turn tool round trips continue to use the existing incremental path.

### Cacheability on drop

Implement a synchronous helper that consumes the last-response receiver with `try_recv()` and decides whether to return the connection:

```rust
fn take_cacheable_connection(&mut self) -> Option<ApiWebSocketConnection> {
    let response_finished = match self.websocket_last_response_rx.as_mut() {
        Some(receiver) => matches!(receiver.try_recv(), Ok(_)),
        None => self.websocket_last_request.is_none(),
    };

    response_finished.then(|| self.connection.take()).flatten()
}
```

The actual implementation should distinguish `Empty` and `Closed` for logging/tests. Both are non-cacheable.

`ModelClientSession::drop` should:

- Never cache while session-level HTTP fallback is active.
- Cache only a connection returned by the helper.
- Discard all logical request/response state.
- Overwrite and drop any older connection in the one-slot cache.

The next session must still call `connection.is_closed().await`. A cached but remotely closed socket is discarded and reconnected through the existing path.

### Split reset operations

Replace the all-or-nothing `reset_websocket_session()` with explicit operations:

```rust
fn clear_websocket_continuation(&mut self) {
    self.websocket_last_request = None;
    self.websocket_last_response_rx = None;
}

fn drop_websocket_connection(&mut self) {
    self.connection = None;
    self.clear_websocket_continuation();
}
```

Use them as follows:

|Call site|Operation|
|---|---|
|New physical connection or reconnect|Clear continuation.|
|Terminal stream error|Drop connection and clear continuation.|
|Session-level HTTP fallback|Drop current connection, clear continuation, and clear the shared cache.|
|Standalone or inline compaction after completed sampling|Clear continuation; retain healthy physical connection.|
|Rollback or uncertain rollout rewrite|Clear continuation; retain the socket only if no request is in flight.|
|Pause/cancel with receiver `Empty` or `Closed`|Drop through the cacheability rule.|

### `/pause` and `/continue`

No new pause-specific transport API is required.

- Pausing cancels the core consumer.
- The final `ModelClientSession` drops.
- Its last-response receiver is normally `Empty` or `Closed` when the model response was interrupted.
- The connection is not cached.
- `/continue` creates a fresh client session, opens a new socket, and sends full durable history.

Add a targeted integration test because this safety property depends on the receiver state, not merely on task cancellation.

### Retry and fallback

- A retry inside the same logical turn may reuse the current connection only when it is still open and continuation state is valid.
- A terminal WebSocket error already closes the lower-level stream; clear logical state before reconnecting.
- When `try_switch_fallback_transport(...)` activates HTTP, clear both the session connection and the shared cached slot.
- 426 fallback must never leave a cached WebSocket for a later turn.

### Telemetry

Connection-reused telemetry is useful but not a prerequisite. The smallest addition is a boolean set when a session adopts a cached connection and passed to `WebsocketTelemetry::on_ws_request(...)`. Avoid porting the upstream telemetry stack.

### Required tests

- Two completed logical turns use one handshake.
- Both turns' first requests are full creates without `previous_response_id`.
- The first turn's same-turn tool follow-up still uses `previous_response_id`.
- A stream error followed by another turn uses a second handshake.
- A provider override does not adopt a connection created for another provider.
- ChatGPT-auth and API-key endpoint modes do not share a cached connection.
- Session-level fallback clears the cache.

Deferred E2E validation:

- A paused in-flight turn followed by `/continue` uses a second handshake and full create. Current coverage is via the cacheability helper until the WebSocket mock can accept concurrent scripted connections.
- A completed turn followed by compaction retains the connection but sends full compacted input. Current cache-specific coverage verifies that clearing logical continuation makes the physical socket cacheable; the combined session-level WebSocket E2E remains deferred.

## Deferred Patch 5b - Cross-turn logical chain reuse

This is worthy but should not be bundled with physical reuse.

### Required design change

Do not cache the logical chain automatically in `Drop`. Add an explicit durability acknowledgement from the turn loop, for example:

```rust
client_session.mark_completed_chain_durable();
```

Call it only after:

- `response.completed` was observed.
- All output items needed for the next request were integrated into conversation history.
- Rollout persistence required by `/continue` completed.
- The turn did not pause, abort, fail, compact, rollback, or rewrite history after that response.

Only a marked session may cache:

- The last logical full request.
- The last response ID.
- The exact output items returned by that response.

The next turn must still run the full property and prefix checks. Any mismatch sends a full create and clears the cached chain while retaining the physical connection.

### Why deferred

This phase touches the durable turn boundary rather than only transport ownership. It needs dedicated pause-after-completion, review, compaction, rollback, and rollout-reconstruction tests. The physical-only patch delivers handshake reuse without assuming those semantics.

## Patch 6 - Optional custom CA and rustls provider parity

### Minimum coherent prerequisite

A WebSocket-only CA helper would make `wss` and HTTP fallback use different trust policies. The minimum coherent scope is a shared helper used by both:

- `core/src/default_client.rs` for Responses HTTP clients.
- `codex-api/src/endpoint/responses_websocket.rs` for secure WebSocket connectors.

### Minimal dependency scope

Add workspace dependencies for:

- `rustls` with `aws-lc-rs` and the required standard/TLS features.
- `rustls-native-certs`.
- `rustls-pki-types`.

Add a trimmed shared module under `codex-client` rather than porting the upstream utility crate hierarchy:

- Select `CODEX_CA_CERTIFICATE`, falling back to `SSL_CERT_FILE`.
- Treat empty values as unset.
- Load native roots.
- Parse all PEM `CERTIFICATE` sections from the selected file.
- Add them to `RootCertStore` with typed errors.
- Build a reqwest client from an existing `ClientBuilder`.
- Build an optional rustls `ClientConfig` for tungstenite.
- Install the `aws-lc-rs` process-wide provider without replacing an already installed compatible provider.
- Verify that the installed provider advertises `ECDSA_NISTP521_SHA512`.

In `codex-api/src/endpoint/responses_websocket.rs`:

1. Ensure the provider is installed.
2. Build the optional custom-CA rustls config.
3. Map it to `tokio_tungstenite::Connector::Rustls`.
4. Pass the connector to `connect_async_tls_with_config(...)`.

### Non-goals

- Do not update every reqwest client in the workspace in this patch.
- Do not port login subprocess probes or CLI doctor output.
- Do not make TLS work a prerequisite for allocation or connection-cache patches.

### Tests

- Environment precedence and empty-value handling.
- Multiple PEM certificate blocks.
- Invalid file and invalid PEM errors.
- Rustls config contains native and custom roots.
- Installed provider supports P-521/SHA-512.
- Hermetic local TLS server trusted only by the configured CA succeeds over both HTTPS and `wss`.

## Patch 7 - Optional provider-configurable connect timeout

### Changes

|Path|Change|
|---|---|
|`core/src/model_provider_info.rs`|Add `websocket_connect_timeout_ms: Option<u64>` and `websocket_connect_timeout() -> Duration`.|
|`core/config.schema.json`|Add the generated provider property.|
|Provider initializers/tests|Set `None` unless explicitly configured.|
|`core/src/client.rs`|Replace `DEFAULT_WEBSOCKET_CONNECT_TIMEOUT` use with the provider method.|

Use 15 seconds as the default to match upstream behavior, or preserve 10 seconds if compatibility is preferred. The important change is configurability.

This patch is independent unless startup prewarm is later added, in which case a bounded connect/warmup timeout becomes mandatory.

## Deferred upstream features and prerequisites

### Request prewarm with `generate: false`

Do not port request prewarm without all of these prerequisites:

1. Skip scheduling when WebSockets are disabled or fallback is active.
2. Bound and cancel connection/warmup resolution.
3. Emit turn-start lifecycle events before awaiting warmup.
4. Ensure `/pause`, interrupt, and task cancellation discard an in-flight warmup session.
5. When an untraced warmup response ID is reused, record the logical full request in rollout/inference tracing rather than the empty-delta wire request.
6. Do not persist the warmup response as assistant history.

### Handshake-only preconnect

Preconnect is narrower than request prewarm but still needs startup ownership and cancellation. Port it only after the physical connection cache exists, so the prepared socket has a clear owner and invalidation path.

### Activation policy

Do not remove the current feature gates as part of this series. Provider-capability-only activation changes user-visible behavior and fallback exposure; it is not required for protocol parity.

## Suggested commit boundaries

1. `websocket: remove obsolete non-create request paths`
2. `websocket: serialize requests directly`
3. `websocket: avoid cloning incremental request history`
4. `websocket: carry turn state in response.create metadata`
5. `websocket: cache completed physical connections across turns`
6. `client: honor custom CA for HTTP and websocket TLS` (optional)
7. `core: make websocket connect timeout provider-configurable` (optional)

Each commit should leave the workspace buildable and keep WebSocket integration tests runnable independently.
