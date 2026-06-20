# Responses WebSocket Inspection Matrix

## Compared implementation surfaces

|Purpose|This project|Upstream project|
|---|---|---|
|Wire request types|`codex-api/src/common.rs`|`codex-api/src/common.rs`|
|WebSocket endpoint|`codex-api/src/endpoint/responses_websocket.rs`|`codex-api/src/endpoint/responses_websocket.rs`|
|Shared stream parser|`codex-api/src/sse/responses.rs`|`codex-api/src/sse/responses.rs`|
|Core transport state|`core/src/client.rs`|`core/src/client.rs`|
|Core stream wrapper|`core/src/client_common.rs`|`core/src/client_common.rs`|
|Provider timeout model|`core/src/model_provider_info.rs`|`model-provider-info/src/lib.rs`|
|TLS helper|No shared custom-CA module|`codex-client/src/custom_ca.rs`; `utils/rustls-provider`|
|Turn lifecycle|`core/src/session/turn.rs`; `core/src/tasks/*`|`core/src/session/*`; `core/src/tasks/*`|
|Integration coverage|`core/tests/suite/client_websockets.rs`|`core/tests/suite/client_websockets.rs`|

## Already present in this project

These items were proposed by the earlier plan but are already implemented.

|Capability or fix|Evidence in this project|Backport action|
|---|---|---|
|Bound WebSocket send|`send_websocket_request(...)` wraps `WsStream::send(...)` in `tokio::time::timeout(...)`.|None; retain tests.|
|Background Ping/Pong processing|`WsStream` has a dedicated pump and responds to Ping frames while a request is idle.|None.|
|Immediate terminal-error propagation|`stream_request(...)` takes and drops the failed stream before forwarding the error.|None.|
|Per-message deflate|`websocket_config()` enables `DeflateConfig`.|None.|
|Handshake reasoning signal|`x-reasoning-included` is captured and emitted.|None.|
|Handshake model catalog ETag|`x-models-etag` is captured and emitted.|None.|
|Handshake server model|`openai-model` is captured and emitted as `ResponseEvent::ServerModel`.|None.|
|Stream server model|`ResponsesStreamEvent::response_model()` reads response and top-level headers and emits changes.|None.|
|Model verification metadata|`ResponsesStreamEvent::model_verifications()` emits `ResponseEvent::ModelVerifications`.|None.|
|Custom-tool input deltas|`response.custom_tool_call_input.delta` maps to `ToolCallInputDelta`.|None.|
|Cyber-policy classification|`cyber_policy` maps to a fatal policy error with fallback text.|None.|
|Overload classification|`server_is_overloaded` and `slow_down` map to overload handling.|None.|
|Incomplete response handling|`response.incomplete` becomes a stream error.|None.|
|`end_turn` preservation|`response.completed.end_turn` is parsed and forwarded.|None.|
|Wrapped WebSocket error mapping|`type: "error"` payloads with status are mapped to HTTP-like transport errors.|None.|
|Connection-limit retryability|`websocket_connection_limit_reached` is mapped to the retryable connection-limit path.|None.|
|426 fallback|Upgrade-required responses select HTTP fallback.|None.|
|Session-scoped fallback|After retry exhaustion, later turns remain on HTTP.|None.|
|401 recovery|The WebSocket path uses existing unauthorized recovery.|None.|
|SSE fixture suppression|WebSockets are disabled when an SSE fixture is active.|None.|
|Same-turn incremental create|`prepare_websocket_request(...)` sends `previous_response_id` plus delta input after a completed response.|Keep; optimize allocations only.|
|Non-prefix full create|A changed or reordered history sends a full request.|None.|
|Non-input property guard|Changed request properties prevent incremental reuse.|Keep; replace clone-based comparison with borrowed comparison.|
|`generate` field|`ResponseCreateWsRequest` already contains `generate`.|No type backport needed.|
|`client_metadata` field|`ResponseCreateWsRequest` already contains `client_metadata`.|Extend with request-scoped turn state only.|
|Turn metadata in request body|`build_ws_client_metadata(...)` carries `x-codex-turn-metadata` and subagent metadata.|Keep.|
|Core consumer-drop cancellation|`ResponseStream::drop` cancels the mapping task.|None.|
|WebSocket telemetry|Connect, request, and event hooks are present.|Connection-reused telemetry is optional.|
|Timing metrics header|The runtime flag adds `x-responsesapi-include-timing-metrics` to the handshake.|None.|
|V2 beta header|The WebSocket upgrade sets `responses_websockets=2026-02-06`.|None.|

## Wire-shape drift

|Shape|This project|Upstream project|Assessment|
|---|---|---|---|
|`response.create`|Implemented and used.|Implemented and used.|Canonical path.|
|`response.processed`|Type, enum variant, sender, feature flag, turn call site, schema entries, and tests remain.|Removed.|Obsolete. Remove in the first patch.|
|`response.append`|Public type and enum variant exist but workspace search finds no construction or send path.|Removed.|Dead protocol surface. Remove in this branch because `codex-api` is used as a workspace-internal crate here.|

## Upstream-fixed bugs still present here

|Bug|Current behavior|Upstream resolution|Minimal backport|
|---|---|---|---|
|Obsolete processed acknowledgement|A hidden feature can send `response.processed` after a successful turn.|Upstream removed the request type, sender, feature flag, schema, call sites, and tests.|Delete only this path; keep the rest of the transport architecture unchanged.|
|Double request serialization|`stream_request(...)` creates a full `serde_json::Value`; `send_websocket_request(...)` serializes that tree to `String`.|Serialize `ResponsesWsRequest` directly to `String` once.|Add a small serializer helper and pass the string through the existing task/send path.|
|Full-history cloning during incremental eligibility|Both request structs are cloned and normalized; previous input and server output are cloned into a baseline before the delta is copied.|Compare non-input fields by reference and use borrowed `strip_prefix` checks.|Adapt the borrowed algorithm to `ResponseCreateWsRequest`; do not port the upstream owned-request refactor.|
|Secure WebSocket ignores configured enterprise CA|The TLS connector is `None`, so tungstenite uses default native roots only.|Build an explicit rustls connector from the shared custom-CA policy.|Port a small shared CA helper and use it for both Responses HTTP and WebSocket clients.|
|rustls provider lacks required P-521 signature support in some enterprise chains|This project relies on dependency-default provider selection.|Upstream installs the `aws-lc-rs` provider and verifies `ECDSA_NISTP521_SHA512` support.|Include provider installation in the TLS prerequisite, not as an unrelated workspace refactor.|

## Feature gaps

|Feature|Upstream behavior|This project|Recommendation|
|---|---|---|---|
|Cross-turn physical connection reuse|A session-scoped cache moves a WebSocket session into a new turn and stores it on drop.|Every `ModelClientSession` starts with no connection.|Backport physical connection reuse only.|
|Cross-turn logical response-chain reuse|The cached session includes the prior logical request and completed response, enabling `previous_response_id` across turns.|Logical state is turn-scoped.|Defer to a second phase with explicit durability commit.|
|Request-scoped turn state|`x-codex-turn-state` is inserted into each `response.create.client_metadata`; `response.metadata` can establish it.|Turn state is captured from upgrade/HTTP headers; it is not parsed from WebSocket response metadata or sent in each request body.|Required prerequisite for cross-turn physical reuse.|
|Connection-reused telemetry|Request telemetry records whether a physical connection was reused.|No reuse state exists.|Optional addition in the connection-cache patch.|
|Per-request stream-start timestamp|Upstream stamps the WebSocket request start time into request metadata immediately before send.|Not present.|Defer with request tracing/diagnostics; it is not required for transport correctness.|
|Request prewarm|`response.create` with `generate: false` can establish request state before generation.|No production prewarm flow.|Defer; broad startup/task/trace dependencies.|
|Handshake-only preconnect|A connection can be opened before the first generated request.|No production preconnect flow.|Defer unless first-turn handshake latency is a measured priority.|
|Provider-configurable connect timeout|Provider config exposes `websocket_connect_timeout_ms`, defaulting upstream to 15 seconds.|Fixed 10-second constant.|Small independent optional patch.|
|Custom CA|Shared environment policy covers HTTPS and secure WebSockets.|No shared policy.|Optional correctness patch for enterprise environments.|
|W3C trace metadata|Traceparent/tracestate can be embedded per request.|Not present in WebSocket request metadata.|Defer with tracing work.|
|Canonical Responses metadata|Upstream centralizes thread, turn, window, request kind, and compatibility fields.|Metadata is assembled in several local helpers.|Do not port as a WebSocket prerequisite.|
|Turn moderation metadata|`response.metadata` can emit a typed turn-moderation event.|Not parsed.|Defer unless an app-server consumer is being backported.|
|Upstream request ID and response debug context|Response streams and errors carry richer request identifiers.|WebSocket `ResponseStream` only carries events.|Defer; diagnostics-only.|
|Handshake probe and doctor integration|Upstream exposes a handshake-only probe and CLI diagnostics.|Absent.|Defer; not transport correctness.|
|Provider-capability-only activation|Upstream enables WebSockets from provider support unless fallback is active.|This project also requires feature flags.|Keep this project's gates to minimize surfaced behavior changes.|
|Responses Lite metadata|Upstream sends mode metadata per request so one cached socket can serve model changes.|This project has no equivalent model mode.|Defer with the model capability.|
|Richer metered rate limits|Upstream supports newer multi-limit shapes.|This project supports the existing rate-limit event path.|Defer; schema and protocol work is not WebSocket-specific.|
|Transient retry notification suppression|Release builds suppress the first WebSocket reconnect notification while retaining all debug-build notifications.|Every retry is surfaced.|Keep current behavior in the minimal series; this is a UI policy change, not a transport prerequisite.|

## Cross-turn reuse comparison

|State|Upstream project|Recommended minimal state in this project|
|---|---|---|
|Physical connection|Cached across turns.|Cache across turns.|
|Last logical request|Cached across turns.|Clear at every logical turn boundary.|
|Last response ID/items|Cached across turns.|Clear at every logical turn boundary.|
|Turn state|Fresh `OnceLock` per turn; sent per request.|Fresh `OnceLock` per turn; add per-request send and metadata capture.|
|First request in new turn|May be incremental.|Always full `response.create` in phase one.|
|Same-turn tool continuation|Incremental when eligible.|Keep current incremental behavior.|
|Pause with response in flight|Upstream does not have this project's exact custom lifecycle.|Do not cache the connection; `/continue` reconnects and sends full durable history.|
|Standalone compaction|Clear logical chain; socket may remain healthy.|Split logical reset from connection drop and send a full create after compaction.|

## Cache safety state table

The final `ModelClientSession` state determines whether its physical connection may be returned to the session cache.

|Session state at drop|Physical connection cacheable?|Reason|
|---|---|---|
|No connection|No|Nothing to cache.|
|Connection exists; no request was accepted by `stream_request(...)`|Yes|No response is in flight.|
|Last-response receiver returns `Ok(LastResponse)`|Yes|The protocol response reached `response.completed`; logical state is discarded before caching.|
|Last-response receiver returns `Empty`|No|A response may still be in flight; this includes the normal pause/cancel race.|
|Last-response receiver returns `Closed`|No|The mapper ended without a completed response.|
|Fallback is active|No|The session has permanently selected HTTP.|
|Provider/auth-mode identity differs from the cached slot|No|A socket must not cross provider endpoints or auth modes.|

## `/pause` and `/continue` implications

|Event|Required WebSocket behavior|
|---|---|
|Pause before `response.completed`|Drop the connection instead of caching it.|
|Continue after such a pause|Open a new connection and send full durable history with no previous response ID.|
|Pause after a completed model response while local processing continues|The physical socket may be cacheable, but the cross-turn logical chain remains cleared in phase one.|
|Interrupted or failed stream|Drop connection and continuation state.|
|Successful ordinary turn|Cache the healthy physical connection; discard request/response chain.|
|Compaction|Keep a healthy connection only after clearing logical continuation state; next request is full.|
|Rollback or rollout rewrite|Clear logical continuation state; do not enable cross-turn chain reuse without an explicit durability proof.|

## Upstream change provenance

The supplied upstream implementation reflects the following relevant fixes. These references are behavioral provenance, not cherry-pick instructions.

|Change|Commit|Backport use|
|---|---|---|
|Remove `response.processed`|`d312a53e2a4419c339c890b79f02b56c19abac85`|Patch 1 cleanup.|
|Avoid cloning WebSocket request history|`95765542c92c922fae4a9b2ce8a97d101702d24d`|Borrowed comparison algorithm.|
|Serialize WebSocket requests directly|`baddb5e68632d3248750305be39179dae6c049dc`|Direct wire serialization.|
|Send request-scoped turn state|`640d61b121e886682a03374fdf668e8b9b97ecf2`|Prerequisite for connection reuse.|
|Extend custom CA handling to secure WebSockets|`6912da84a869a313e77a03b0baf0f35f21d34d8c`|Optional TLS patch.|
|Use the aws-lc rustls provider|`d5a8117e087fc1201cf059a2add082e7529022e3`|TLS prerequisite for P-521 chains.|
|Bound WebSocket request sends|`35aaa5d9fcb606fb6f27dd5747ecab3f4ba0c07e`|Already present; no work.|
|Prevent startup prewarm from blocking turn start|`6ea041032b500a6f3e8511d225af366d5e53439b`|Required only if prewarm is later ported.|
|Skip startup prewarm when WebSockets are disabled|`859dbe27616c593238bad63be63e13a2d80579d9`|Required only if prewarm is later ported.|
|Trace the logical request after untraced warmup|`20fedafff83f5c681fc62f73b0ca3227e42e3f8b`|Required only if request prewarm and tracing are later combined.|

## Excluded stale findings

The following claims from the prior proposal are incorrect for the current state of this project and must not drive implementation:

- WebSocket sends are unbounded.
- `response.processed` needs to be added.
- Consumer-drop cancellation is absent.
- Server model metadata is dropped.
- Model verification metadata is dropped.
- Custom-tool deltas are dropped.
- Cyber-policy and overload errors are underclassified.
- `permessage-deflate` is absent.
- Terminal errors wait for a graceful close handshake.
