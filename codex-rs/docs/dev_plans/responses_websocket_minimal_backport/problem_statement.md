# Responses WebSocket Minimal Backport - Problem Statement

## Scope

This document covers the Responses API WebSocket transport in this project and compares it with the upstream project. It also treats this project's `/pause` and `/continue` implementation as a first-class lifecycle constraint.

The inspected implementation surfaces are:

|Concern|This project paths|
|---|---|
|Wire request and stream types|`codex-api/src/common.rs`|
|WebSocket connection, pump, send, receive, and error mapping|`codex-api/src/endpoint/responses_websocket.rs`|
|Shared Responses stream parsing|`codex-api/src/sse/responses.rs`|
|Transport selection, incremental request construction, retry, and fallback|`core/src/client.rs`|
|Mapped-stream cancellation|`core/src/client_common.rs`; `core/src/client.rs`|
|Compaction invalidation|`core/src/compact.rs`; `core/src/session/turn.rs`|
|Turn lifecycle and `/continue` reconstruction|`core/src/session/turn.rs`; `core/src/tasks/*`; `core/src/session/rollout_reconstruction.rs`|
|Feature flags and schema|`core/src/features.rs`; `core/config.schema.json`|
|WebSocket integration tests|`core/tests/suite/client_websockets.rs`; `core/tests/suite/websocket_fallback.rs`|

## Protocol baseline

External contract reference: `https://developers.openai.com/api/docs/guides/websocket-mode`.

The current Responses WebSocket contract uses a persistent `/v1/responses` connection and client-sent `response.create` messages. Continuation sends the previous response ID and only newly appended input. `generate: false` is an optional request warmup. Requests on one connection are sequential, the service retains only the latest connection-local response state, and a connection is limited to 60 minutes.

Important consequences for this project:

1. `response.processed` is not required by the current contract.
2. A healthy physical connection is reusable independently of whether the client chooses to reuse the server's previous-response cache.
3. Cross-turn `previous_response_id` reuse is an optimization, not a prerequisite for cross-turn socket reuse.
4. After standalone `/responses/compact`, the client must start a new chain with full compacted input and no previous response ID.
5. After a failed continuation or a reconnect where the prior response is not available, the safe recovery path is a full request.

## Current implementation in this project

### Connection and frame pump

`codex-api/src/endpoint/responses_websocket.rs` owns one `WsStream` pump per connection. The pump serializes outgoing writes through a command channel, continuously reads incoming frames, responds to Ping frames, suppresses Pong frames, and forwards data or close frames to the response loop.

`ResponsesWebsocketConnection` holds the pump behind an async mutex. The mutex enforces one in-flight response per connection, matching the protocol's sequential request model.

Connection setup already:

- Converts the Responses URL to `ws` or `wss`.
- Merges provider, request, and default headers.
- Sends the WebSocket beta header.
- Enables `permessage-deflate`.
- Captures `x-reasoning-included`, `x-models-etag`, `openai-model`, and the handshake `x-codex-turn-state` value.
- Uses a bounded connect timeout in `core/src/client.rs`.

### Request send and response processing

`ResponsesWebsocketConnection::stream_request(...)` serializes the WebSocket request directly to a JSON string and applies the stream idle timeout to the actual send.

The response task:

- Emits handshake-derived server metadata.
- Parses wrapped `type: "error"` events into HTTP-like transport errors.
- Handles the 60-minute `websocket_connection_limit_reached` error as retryable.
- Parses rate-limit, server-model, and model-verification events.
- Delegates normal Responses events to `process_responses_event(...)`.
- Drops the underlying stream immediately on a terminal error rather than waiting for a close handshake.

### Core client lifetime

`ModelClient` is session-scoped. `ModelClientSession` is explicitly turn-scoped and currently owns:

- One lazily opened `ResponsesWebsocketConnection`.
- The last full `ResponseCreateWsRequest` for same-turn incremental comparison.
- A oneshot receiver containing the last completed response ID and returned output items.
- A fresh `OnceLock<String>` for `x-codex-turn-state`.

A new `ModelClientSession` is constructed for every regular turn and every continued turn. Completed healthy sessions may return the physical WebSocket connection to the session-scoped one-slot cache keyed by provider and auth mode; logical continuation state is still cleared at turn boundaries.

### Incremental request construction

Within a turn, `prepare_websocket_request(...)` can send an incremental `response.create` when:

- The non-input request properties are unchanged.
- Current input starts with the previous request input followed by the output items returned by the server.
- The last response completed and has a non-empty response ID.

The current comparison is correct but allocation-heavy. `get_incremental_items(...)` clones both requests, clears ignored fields, clones the prior input and response items into a baseline, and then allocates the outgoing delta.

### Retry and fallback

WebSocket use requires provider capability, the project feature gate, no SSE fixture, and no prior session-scoped fallback. The client:

- Recovers once from 401 where supported.
- Falls back immediately on HTTP 426.
- Reconnects after the 60-minute connection-limit error.
- Clears the connection and continuation state after terminal stream failure.
- Permanently switches the Codex session to HTTP after the WebSocket retry budget is exhausted.

### Stream cancellation

The core `ResponseStream` owns a cancellation token. Dropping the consumer cancels the mapper task, so a paused or interrupted turn stops polling the provider stream promptly.

This does not make an in-flight physical socket reusable. The lower-level WebSocket response task may still be completing or tearing down. Connection caching must therefore treat an unfulfilled last-response receiver as non-cacheable.

## `/pause` and `/continue` constraints

This project persists an explicit `PendingContinuation`, emits `TurnPaused` and `TurnContinued`, repairs rollout/history state, and starts continuation through the normal turn sampling loop.

The key invariants are:

- `/pause` does not synthesize a new user prompt.
- `/continue` rebuilds model input from durable completed history.
- Partial text, reasoning deltas, and partially observed tool calls are not a reusable model-response boundary.
- A response ID is safe for logical continuation only after `response.completed` and after the corresponding output is durably integrated.
- A paused, interrupted, cancelled, or failed in-flight response must not be reused as a previous-response chain.
- A physical socket may be reused only when no prior request remains in flight on it.

These invariants make automatic upstream-style caching of the entire `WebsocketSession` too broad for the first backport.

## Corrected problem definition

The remaining high-value work is narrower than the previous proposal:

|Problem|Type|Impact|
|---|---|---|
|Dormant `response.processed` protocol and feature plumbing remains|Protocol cleanup|Maintains an obsolete request shape and lifecycle branch that upstream removed.|
|Request serialization constructs an intermediate JSON tree|Performance bug|Extra traversal and memory proportional to full request/history size.|
|Incremental eligibility clones full requests and history|Performance bug|Repeated O(history) copies on tool-heavy turns.|
|The physical connection is dropped at every logical turn boundary|Feature gap|Repeated handshake latency and loss of connection-level transport state.|
|Turn state is captured at upgrade rather than response-request scope|Reuse prerequisite|A cached physical connection cannot safely carry distinct logical turns until this state is request-scoped.|
|WebSocket TLS ignores the project's custom enterprise CA needs|Environment-specific correctness gap|`wss` can fail behind TLS interception even when the correct CA is configured.|
|Connect timeout is fixed at 10 seconds|Operational gap|Providers cannot tune slow or failing handshake behavior.|

## Goals

- Backport bug fixes without importing the upstream provider/auth/session refactors.
- Preserve the current feature gates and HTTP fallback policy.
- Preserve same-turn incremental behavior.
- Reuse a healthy physical connection across turns without changing durable conversation semantics.
- Make request-scoped turn state a minimal prerequisite rather than porting the full upstream metadata architecture.
- Keep TLS work independent so it does not block protocol and allocation fixes.

## Non-goals

- Cherry-picking upstream modules as-is.
- Removing the project's WebSocket feature gates.
- Porting startup scheduling, request prewarm, or full preconnect orchestration in the first series.
- Porting the upstream auth/provider abstraction.
- Porting the full inference-trace, response-debug, or canonical metadata architecture.
- Reusing `previous_response_id` across logical turns in the first connection-cache patch.
- Changing `/pause` or `/continue` user-visible semantics.
- Broad rate-limit or app-server schema changes unrelated to the transport fixes.
