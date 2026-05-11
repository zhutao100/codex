# Responses WebSocket Minimal Backport - Problem Statement

## Target

This proposal targets this project and compares it with the upstream project.

The goal is not to cherry-pick the upstream project. The goal is a minimal surfaced backport plan for the remaining Responses WebSocket correctness fixes and feature gaps that still matter after this project's existing partial backport.

## Current External Contract

The current WebSocket Mode contract is the v2 shape:

- Connect to `/v1/responses` over WebSocket.
- Start each turn with `response.create`.
- Continue by sending another `response.create` with `previous_response_id` and only the new input items.
- Optionally prewarm by sending `response.create` with `generate: false`; the returned response ID can be chained by a later generated turn.
- Treat connection-local previous-response state as an optimization, not durable history.
- Expect one in-flight response per connection and reconnect when the 60-minute connection limit is reached.
- Treat WebSocket `type: "error"` payloads as application-level errors; examples include `previous_response_not_found` and `websocket_connection_limit_reached`.

## Inspection Summary

### What this project already has

This project has already incorporated a substantial subset of the upstream project's WebSocket fixes. Do not re-port these as if they were absent.

|Area|This project status|Relevant paths|
|---|---|---|
|v2 `response.create` continuation|The OpenAI WebSocket path constructs `ResponsesWsRequest::ResponseCreate` for both initial and incremental requests, using `previous_response_id` for valid deltas.|`core/src/client.rs`; `codex-api/src/common.rs`|
|`generate` field|`ResponseCreateWsRequest` already includes `generate: Option<bool>`.|`codex-api/src/common.rs`|
|Body-level `client_metadata` field|`ResponseCreateWsRequest` already includes `client_metadata: Option<HashMap<String, String>>`; this project currently populates turn metadata, subagent, and parent-thread IDs.|`codex-api/src/common.rs`; `core/src/client.rs`|
|Incremental comparison normalization|`generate` and `client_metadata` are cleared before comparing non-input request fields, avoiding false misses for transient WebSocket-only fields.|`core/src/client.rs`|
|SSE fixture suppression|WebSocket transport is disabled while `CODEX_RS_SSE_FIXTURE` is set.|`core/src/client.rs`|
|HTTP fallback on `426 Upgrade Required`|Connect-time 426 maps to a local `FallbackToHttp` outcome and activates session-scoped HTTP fallback.|`core/src/client.rs`; `core/tests/suite/websocket_fallback.rs`|
|Connect timeout|WebSocket connect is bounded by `DEFAULT_WEBSOCKET_CONNECT_TIMEOUT`.|`core/src/client.rs`|
|Default v2 beta header|WebSocket connects send `OpenAI-Beta: responses_websockets=2026-02-06`.|`core/src/client.rs`|
|Header merge precedence|Provider headers, extra headers, and default headers are merged with HTTP-compatible precedence.|`codex-api/src/endpoint/responses_websocket.rs`|
|Background pump|`WsStream` has a pump task that continuously reads, responds to ping frames, and serializes writes.|`codex-api/src/endpoint/responses_websocket.rs`|
|Drop-on-terminal-error|Terminal stream errors drop the failed stream instead of awaiting a graceful close handshake.|`codex-api/src/endpoint/responses_websocket.rs`|
|Wrapped WebSocket errors|`type: "error"` payloads are parsed before ordinary Responses stream events; non-success statuses map to HTTP-like transport errors.|`codex-api/src/endpoint/responses_websocket.rs`|
|60-minute connection-limit retry|`websocket_connection_limit_reached` maps to `ApiError::Retryable`.|`codex-api/src/endpoint/responses_websocket.rs`; `core/tests/suite/client_websockets.rs`|
|`response.incomplete` and `end_turn`|Shared stream parsing handles `response.incomplete` and preserves `response.completed.response.end_turn`.|`codex-api/src/sse/responses.rs`; `codex-api/src/common.rs`|
|Turn-scoped reset hook|`reset_websocket_session()` clears connection, last request, and last response receiver.|`core/src/client.rs`|

### What the upstream project has beyond this project

|Area|Upstream project behavior|This project behavior|Impact|
|---|---|---|---|
|`response.processed` acknowledgement|Adds `ResponseProcessedWsRequest`, `ResponsesWsRequest::ResponseProcessed`, `ResponsesWebsocketConnection::send_response_processed()`, a feature flag, and call sites after successful turn processing and remote compaction.|No `response.processed` request type or feature flag.|The server never receives an explicit processed acknowledgement from this project. This is the highest-value remaining protocol gap.|
|Request send timeout|Wraps WebSocket request frame send in the stream idle timeout.|Sends request frames without a timeout; connect and receive are bounded, but a stuck send path is not.|A pathological or wedged WebSocket write can hang longer than intended.|
|Consumer-drop cancellation|Cancels the stream-mapping task when the core `ResponseStream` consumer is dropped.|The mapping task keeps polling the provider stream until another event, error, or timeout.|A paused, interrupted, or cancelled turn can leave response processing alive longer than intended.|
|Handshake `openai-model` handling|Reads `openai-model` from the handshake and emits `ResponseEvent::ServerModel`.|Ignores `openai-model`.|Server model reroute information is invisible.|
|Streaming model metadata|`ResponsesStreamEvent` can extract model headers from stream payloads and emit deduplicated `ServerModel` events.|No stream-level model metadata extraction.|Model changes during or after request processing are invisible.|
|Model verification recommendations|Parses `model_verifications` from stream metadata and emits `ResponseEvent::ModelVerifications`.|No event type or parser.|Account-verification recommendations are dropped.|
|Custom-tool input delta|Parses `response.custom_tool_call_input.delta` into `ResponseEvent::ToolCallInputDelta`.|No event type or parser.|Large custom-tool input streams are not surfaced incrementally.|
|Provider error classification|Maps `cyber_policy`, `server_is_overloaded`, and `slow_down` to dedicated stream errors.|Falls through to generic retryable stream handling.|Some non-retryable or overload conditions can get the wrong retry/user-facing behavior.|
|Per-message deflate|Uses a WebSocket config with `permessage-deflate` enabled.|Uses default tungstenite config.|Higher bandwidth and less parity with the upstream transport. This may require the upstream tungstenite fork or equivalent dependency support.|
|Custom CA for WebSocket TLS|Uses the upstream custom-CA rustls helper and `connect_async_tls_with_config`.|Uses `connect_async` with default TLS behavior.|Not a WebSocket-only bug in this project unless custom-CA support is ported generally; otherwise an optional enterprise parity feature.|
|Cross-turn WebSocket cache|Caches a `WebsocketSession` in `ModelClient`, moves it into new turn sessions, and invalidates it on window-generation changes/fallback.|WebSocket sessions are turn-scoped by design.|Later turns cannot use active-socket continuation latency benefits. This interacts with `/pause` and `/continue`; keep it second-stage.|
|Preconnect|Can open a WebSocket before the generated turn request.|No preconnect method.|The first generated WebSocket request pays connect latency.|
|Request prewarm|Can send `response.create` with `generate: false` and consume the warmup completion before the generated turn.|The request shape has `generate`, but no prewarm path uses it.|No request-state warmup latency benefit.|
|Trace and identity metadata|Adds installation ID, window ID, W3C trace context, and request-start timestamp into `client_metadata`.|Only turn metadata, subagent, and parent-thread IDs are populated.|Reduced observability and weaker parity with upstream diagnostics.|
|Richer rate-limit parsing|Supports multiple metered-limit header families and limit identifiers in `RateLimitSnapshot`.|Parses the legacy/default rate-limit snapshot shape.|Multi-limit rate-limit details are collapsed or ignored. This depends on protocol schema changes and is not WebSocket-only.|
|`ResponseStream.upstream_request_id` and response debug context|Carries upstream request IDs and richer response debug metadata.|Not present in the WebSocket `ResponseStream` shape.|Useful for diagnostics, but not required for WebSocket correctness.|

## Bugs fixed upstream that remain present here

|Bug|Status in this project|Minimal fix stance|
|---|---|---|
|Unbounded WebSocket send|A `ws_stream.send(...)` is awaited directly. Connect and receive have timeouts, but send does not.|Add `send_websocket_request(...)` and wrap the send in `idle_timeout`, matching upstream behavior without pulling in the upstream telemetry/inference-trace refactor.|
|No `response.processed` acknowledgement|No request type, API method, feature flag, or call site exists.|Backport the request type and method; gate call sites behind a new under-development feature. This is a small protocol extension with clear tests.|
|Consumer-drop cancellation leak|The core mapper has no cancellation token tied to `ResponseStream::drop`.|Add a drop-triggered cancellation token so pause, interruption, and cancellation stop mapper polling promptly.|
|Server model metadata dropped|Handshake `openai-model` and stream model headers are ignored.|Add `ResponseEvent::ServerModel`, handshake parsing, and stream metadata parsing. Initially log or forward through existing event plumbing; avoid importing unrelated upstream UX.|
|Model verification metadata dropped|No `ModelVerifications` event exists.|Add parser/event support only if this project has a consumer or wants to preserve this server signal for later UI work.|
|Custom-tool input deltas dropped|`response.custom_tool_call_input.delta` is ignored by shared stream parsing.|Add a parser and event. Route through core only when the consumer can use it; otherwise keep this optional.|
|Underclassified stream errors|`cyber_policy`, `server_is_overloaded`, and `slow_down` use generic fallback handling.|Classify them in shared Responses parsing without importing broader upstream UX.|
|No per-message deflate|Default WebSocket config is used.|Port only if dependency support is available with a small Cargo change. If the upstream tungstenite fork is required, treat it as a prerequisite decision, not an incidental patch.|
|No WebSocket custom-CA parity|This project does not have the upstream custom-CA helper.|Do not make this a prerequisite for other WebSocket fixes. Port it only if enterprise/custom-CA support is desired globally.|

## Already-fixed items to exclude from new backport work

The following items are already present in this project and should be treated as validation targets, not proposed implementation work:

- SSE fixture suppression for WebSockets.
- 426 fallback to HTTP.
- v2 `response.create` incremental path for OpenAI WebSocket requests.
- `ResponseCreateWsRequest.generate` and `client_metadata` fields.
- Excluding `generate` and `client_metadata` from incremental request equality.
- Background WebSocket pump for ping/pong.
- Dropping failed streams instead of waiting for close handshake.
- Wrapped WebSocket error mapping.
- `websocket_connection_limit_reached` retryability.
- `response.incomplete` and `response.completed.end_turn` parsing.
- Default `OpenAI-Beta: responses_websockets=2026-02-06` handshake header.
- Connect timeout.

## `/pause` and `/continue` constraints

This project's `/pause` and `/continue` behavior is the main local constraint on any acceleration work.

Backported changes must preserve these invariants:

- `/pause` must not persist a synthetic user prompt.
- `/continue` must rebuild from durable completed history, not partial text/reasoning/tool deltas.
- A stream aborted by `/pause`, cancellation, interruption, or stream error must not leave reusable WebSocket incremental state.
- A previous response ID is safe only after `response.completed` has been processed and the completed response has been integrated into durable state.
- `generate: false` warmup responses are setup artifacts, not assistant output to record in rollout history.
- Cross-turn WebSocket caching must be invalidated on pause, interruption, compaction, rollback, fallback, and any stream error unless the latest completed chain is explicitly known safe.

The safe initial rule remains:

> WebSocket incremental state is reusable only after a completed response. Any pause, stream error, cancellation, compaction, rollback, or rollout rewrite clears it.

## Non-goals

Do not include these in the minimal backport:

- The upstream project's `core/src/session/*` refactor.
- The upstream provider/auth stack rewrite.
- The upstream inference-trace and full response-debug telemetry refactor.
- Full startup prewarm scheduling.
- Full model-reroute/trusted-access/account-verification UX.
- Realtime/WebRTC changes unrelated to Responses WebSocket.
- Broad protocol schema changes for multi-limit rate limits unless they are already being ported for non-WebSocket reasons.

## Recommended Backport Levels

|Level|Contents|Rationale|
|---|---|---|
|P0 correctness|Bound request send, add `response.processed` request support and feature-gated call sites, cancel mapped streams on consumer drop|Fixes the remaining small correctness/protocol gaps with low dependency cost.|
|P1 parser and error parity|`ServerModel`, model verification metadata, custom-tool input deltas, provider stream error classification|Preserves server signals currently dropped by this project and keeps retry/user-facing behavior aligned with upstream.|
|P2 transport hardening|`permessage-deflate`; custom CA only if the prerequisite custom-CA helper is intentionally ported|Improves transport parity without blocking P0/P1.|
|P3 observability metadata|W3C trace metadata, installation/window IDs, request-start timestamp|Improves diagnostics; can be incremental and does not change prompt/history behavior.|
|P4 acceleration|Preconnect, request prewarm, optional cross-turn cached WebSocket session with explicit invalidation|Adds latency wins after correctness and parser parity are stable.|
|P5 broad diagnostics/rate limits|`upstream_request_id`, response debug context, multi-limit rate-limit schema|Useful but not WebSocket-minimal; defer unless adjacent work already touches these schema and telemetry paths.|
