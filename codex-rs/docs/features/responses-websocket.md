# Responses WebSocket transport

Read this when changing request construction, client lifetime, retry/fallback, or compaction/continuation transport state. [Pause and continue](pause-continue.md), [history replay](history-replay.md), and [review request prefixes](post_turn_completion_review/request_prefix.md) define the history the transport must carry.

## Activation and wire contract

WebSockets require a Responses provider with `supports_websockets` and either `features.responses_websockets` or `features.responses_websockets_v2`. Both feature keys select the same transport implementation and default to disabled. An SSE fixture disables WebSockets. These flags do not change `wire_api`.

The outbound operation is `response.create`. Requests are serialized directly to the wire string; there is no `response.append` or `response.processed` path. Complete local history remains available for full requests and HTTP fallback.

## Incremental requests

A completed response permits the next request in the same `ModelClientSession` to send `previous_response_id` plus only new input when all of these conditions hold:

- The prior response ID is nonempty and the stream mapper supplied a completed response.
- The current full input begins with the prior full request input followed by the exact output items returned by that response.
- The compared request properties match: model, instructions, tools, tool choice, parallel-tool setting, reasoning, storage, streaming, included fields, service tier, prompt cache key, and text/output-schema settings.

The comparison borrows the request and uses `strip_prefix`; it does not clone complete histories merely to test eligibility. An empty delta is allowed. `client_metadata` and `generate` are excluded from the property comparison; ordinary generation leaves `generate` unset.

If any condition fails, send a full `response.create` with no previous response ID. Do not infer eligibility from matching user text, token counts, or the presence of a cached socket. Server prompt caching and transport continuation are separate: an appended prompt can reuse a token prefix even when this client deliberately starts a new logical chain.

## State ownership

|State|Lifetime and rule|
|---|---|
|Local conversation history|Session-owned; required for restart and full requests.|
|Physical socket|May be cached between client sessions, scoped by provider configuration and auth mode. A closed cached socket is replaced before use.|
|Last full request and completed response ID/output|Client-session scoped; discarded at a logical turn boundary.|
|`x-codex-turn-state`|Fresh `OnceLock` per client session. Capture from response header/metadata and send through request-scoped `response.create.client_metadata`; never carry the prior turn's token into a new turn.|
|HTTP fallback selection|Session-owned and sticky once activated.|

Create a fresh `ModelClientSession` for each logical turn. Reuse it for sampling rounds and retries within that turn. Across turns, reuse only a healthy physical connection; the next turn's first request is full. Cross-turn response-chain caching would require an explicit history/durability commit contract and is not implemented.

## Lifecycle and fallback

|Event|Required behavior|
|---|---|
|Normal completed response|Permit same-turn incremental requests; permit physical caching when the client session ends.|
|Pause/cancel while response is in flight, or mapper ends without completion|Do not return that connection to the cache. `/continue` starts a new client session and sends full surviving history.|
|Pause after protocol completion during local processing|A healthy socket may survive; its logical request/response chain still does not cross into `/continue`.|
|Successful compaction|Clear the logical chain; keep the socket only if its last response completed. The replacement becomes the next full request.|
|History rewrite or changed request properties|Fail incremental eligibility and send full input.|
|HTTP 426 during upgrade|Switch to HTTP.|
|Retryable stream errors exhaust provider retry budget|Switch once to HTTP, reset retries, and keep subsequent turns on HTTP.|
|Connection lifetime-limit error|Treat as retryable, discard the closed connection, and reconnect.|

A connection is cacheable only when no request is outstanding or its completion receiver yields a finished response. Both an empty receiver and a closed receiver without a result are non-cacheable. Transport completion makes socket reuse possible; it does not prove task finalization or rollout persistence succeeded.

The endpoint handles bounded sends, idle ping/pong, per-message deflate, server-model and verification events, and immediate terminal-error propagation. The common response parser also handles incomplete responses; do not bypass it when modifying the WebSocket path.

## Known configuration limits

The connect timeout is a fixed 10 seconds in `core/src/client.rs`; provider configuration does not expose `websocket_connect_timeout_ms`. The secure-WebSocket call passes no custom TLS connector, and there is no shared custom-CA environment policy for HTTP and WebSockets. If custom-CA support is needed, implement one shared certificate/provider policy for both transports and test the same chain against each; HTTP success alone does not establish secure-WebSocket compatibility.

## Implementation and checks

|Concern|Entry point|
|---|---|
|Eligibility, state cache, sticky fallback|[`core/src/client.rs`](../../core/src/client.rs): `prepare_websocket_request`, `get_incremental_items`, `response_create_properties_match`, `reset_after_compaction`, `ModelClientSession::drop`|
|Wire types and connection I/O|[`codex-api/src/common.rs`](../../codex-api/src/common.rs), [`responses_websocket.rs`](../../codex-api/src/endpoint/responses_websocket.rs)|
|Shared event semantics|[`codex-api/src/sse/responses.rs`](../../codex-api/src/sse/responses.rs)|
|Retry budget and cancellation|[`core/src/session/turn.rs`](../../core/src/session/turn.rs), [`core/src/client_common.rs`](../../core/src/client_common.rs)|

[`client_websockets.rs`](../../core/tests/suite/client_websockets.rs) contains real local-server tests for incremental/full request selection, response-metadata turn state, cross-turn physical reuse, in-flight pause/continue, compaction, and provider isolation. [`websocket_fallback.rs`](../../core/tests/suite/websocket_fallback.rs) covers upgrade failure, retry exhaustion, and sticky fallback. Use the existing localhost-capable test guard; an early-return network test is not transport coverage. These tests inspect request shape and lifecycle, not live-service latency or cache-hit rates.
