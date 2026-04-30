# Responses WebSocket Minimal Backport - Problem Statement

## Target Base

This proposal targets the customized `custom-0.98.0` branch.

The target branch already has fork-local work that matters for this plan:

- Responses-over-WebSocket support behind `enable_responses_websockets` and `enable_responses_websockets_v2`.
- `service_tier` wiring on both HTTP Responses and WebSocket Responses requests.
- `/pause` and `/continue`, with a turn-scoped `ModelClientSession` reused across retries inside a turn.
- Rollout and prompt-history cleanup semantics for continuation from paused or interrupted work.

The upstream reference is the latest upstream branch plus the current OpenAI WebSocket Mode guide.

## Goal

Bring the old branch's Responses WebSocket implementation up to the current contract with minimal surfaced changes.

The port should not cherry-pick the latest implementation wholesale. The latest branch has a broader session/module refactor (`core/src/session/*`, `SharedModelProvider`, richer telemetry/inference tracing, startup prewarm state, window-generation tracking). Those are useful context, not prerequisites by default.

The desired output is a small sequence of back-port patches that:

1. fixes correctness bugs already fixed upstream;
2. aligns the wire protocol with current `response.create` continuation semantics;
3. adds the narrow event/request fields needed by the newer WebSocket contract;
4. preserves the old branch's `/pause` and `/continue` invariants;
5. adds prewarm/reuse only after the correctness and wire-protocol changes are in place.

## Current Contract To Target

The current WebSocket Mode guide describes these externally visible properties:

- Clients connect to `wss://api.openai.com/v1/responses`.
- Each generated turn starts by sending a `response.create` event.
- A warmup request may send `response.create` with `generate: false`; the warmup returns a response ID that can be chained with `previous_response_id`.
- Incremental continuation uses another `response.create` with `previous_response_id` and only new input items.
- The service keeps the most recent previous-response state in connection-local memory for the fast path.
- A single connection handles one in-flight response at a time and has a 60-minute duration limit.
- WebSocket error payloads are normal JSON events with `type: "error"`; examples include `previous_response_not_found` and `websocket_connection_limit_reached`.

The current docs therefore match the latest branch's v2-only `response.create` shape more closely than the old branch's v1 `response.append` path.

## Branch Inspection Summary

### `v0.98` shape

Key files:

- `codex-api/src/endpoint/responses_websocket.rs`
- `codex-api/src/common.rs`
- `codex-api/src/sse/responses.rs`
- `core/src/client.rs`
- `core/src/codex.rs`
- `docs/dev_plans/pause_continue/design_proposal.md`

Observed behavior:

- WebSocket connection is a raw `WebSocketStream<MaybeTlsStream<TcpStream>>` behind `Arc<Mutex<Option<WsStream>>>`.
- The stream loop manually handles `Message::Ping` only while an active response stream is being read.
- Terminal stream errors call `ws_stream.close(None).await` before the error is sent to the consumer.
- Connection setup uses `tokio_tungstenite::connect_async(request)`.
- WebSocket enablement does not suppress the transport when `CODEX_RS_SSE_FIXTURE` is set, so fixture-driven tests can bypass the fixture path.
- The handshake reads `x-reasoning-included`, `x-models-etag`, and `x-codex-turn-state`, but not `openai-model`.
- The request enum supports both `response.create` and `response.append`.
- Incremental v1 sends `response.append`; incremental v2 sends `response.create` with `previous_response_id`.
- There is no WebSocket request `generate` field and no `client_metadata` field.
- There is no WebSocket connect timeout distinct from stream idle timeout.
- There is no request prewarm or startup prewarm.
- `ModelClientSession` is explicitly turn-scoped; a fresh session is created per turn and reused across retries inside that turn.
- `/pause` aborts active work and `/continue` re-enters the sampling loop without recording a new user prompt; only completed model-visible history is durable.

### Latest upstream shape

Key files:

- `codex-api/src/endpoint/responses_websocket.rs`
- `codex-api/src/common.rs`
- `codex-api/src/sse/responses.rs`
- `core/src/client.rs`
- `core/src/session/turn.rs`
- `core/src/session_startup_prewarm.rs`

Observed behavior:

- WebSocket I/O is wrapped by a small `WsStream` pump task that reads continuously, handles ping/pong, and serializes writes via commands.
- Terminal stream errors drop the failed stream instead of waiting for a close handshake.
- Connection setup uses `connect_async_tls_with_config` with an explicit `WebSocketConfig` that enables `permessage-deflate`.
- WebSocket TLS honors Codex custom-CA configuration through a rustls connector.
- WebSocket transport is disabled while `CODEX_RS_SSE_FIXTURE` is set.
- WebSocket connect attempts are bounded by a provider-level connect timeout.
- Default headers are merged with provider and request-specific headers using HTTP-compatible precedence.
- The handshake reads `openai-model` and emits `ResponseEvent::ServerModel`.
- Application-level WebSocket error events are parsed before ordinary Responses stream events.
- `websocket_connection_limit_reached` maps to a retryable error; other non-2xx wrapped statuses map to `TransportError::Http` with JSON-derived headers and body.
- `ResponsesWsRequest` only emits `response.create`.
- `ResponseCreateWsRequest` adds `generate` and `client_metadata`.
- Request metadata carries installation/window/subagent/parent-thread/turn metadata plus W3C trace context in the body.
- A `generate: false` prewarm request can be consumed before the first generated stream.
- A cached `WebsocketSession` can be moved across `ModelClientSession` instances; history/window invalidation resets that cache.
- Shared stream parsing emits server model changes, model verification recommendations, custom-tool input deltas, `response.incomplete` errors, and `response.completed.end_turn`.

## Feature Gaps

| Area | Latest/current behavior | `v0.98` behavior | Impact | Minimal back-port stance |
|---|---|---|---|---|
| SSE fixture suppression | WebSocket transport is disabled when `CODEX_RS_SSE_FIXTURE` is set. | WebSocket enablement ignores the fixture flag; only HTTP SSE checks it. | Fixture-based tests can hit the WebSocket path instead of deterministic fixture input. | Add the enablement condition before other behavior changes. |
| 426 fallback | A WebSocket connect `426 Upgrade Required` immediately switches the session to HTTP fallback. | The handshake failure is treated like an ordinary stream error and can consume retry budget. | A request that should transparently use HTTP can spend extra WebSocket retries or fail noisily. | Add a small local WebSocket outcome enum and switch fallback in `stream(...)`. |
| v2 request protocol | All WebSocket turns and incremental continuations are `response.create`. | v1 mode can emit `response.append`; v2 emits `response.create` only when `enable_responses_websockets_v2` is on. | The old branch can exercise a stale wire path that the current docs no longer describe. | Stop emitting `response.append` for OpenAI Responses WebSocket. Keep the enum temporarily if tests or private providers still need it, but route Codex's OpenAI path through v2 `response.create`. |
| Warmup | `response.create` can include `generate: false` and returns a response ID for later `previous_response_id`. | No `generate` field and no warmup method. | First generated turn misses upstream's lower-latency setup path. | Add `generate: Option<bool>` first, then add an explicit best-effort `prewarm_websocket(...)` path. |
| Connection-local previous-response cache across turns | Latest can cache a `WebsocketSession` in `ModelClient` and move it into the next turn session. | A fresh `ModelClientSession` starts without the previous WebSocket connection. | Later turns cannot use the active-socket previous-response fast path. | Treat as a second-stage acceleration feature. It needs explicit invalidation on compaction, rollback, pause/continue cleanup, and fallback. |
| Wrapped WebSocket errors | `type: "error"` payloads map to HTTP-like or retryable errors. | These payloads fail normal `ResponsesStreamEvent` parsing and are ignored. | Usage-limit, invalid-request, and connection-limit failures may be hidden until close/timeout; retry/fallback decisions are wrong. | Port the parser and mapper directly into `codex-api/src/endpoint/responses_websocket.rs`; no session refactor needed. |
| 60-minute connection limit | `websocket_connection_limit_reached` is retryable and triggers a reconnect. | Error event is ignored or misclassified. | Long sessions can fail instead of reconnecting. | Map the wrapped error code to `ApiError::Retryable`, drop the failed stream, and rely on the existing stream retry loop. |
| Terminal error handling | Failed streams are dropped without waiting for graceful close. | Error path awaits `ws_stream.close(None).await`. | A server that never completes the close handshake can stall the stream and suppress the actual error. | Replace close-await with `guard.take(); drop(guard); drop(failed_stream); send Err(err)`. |
| Ping/pong | Background pump responds to pings while the connection exists. | Ping is handled only inside the active response loop. | Idle or lock-held connections may miss ping/pong liveness work. | Port the small pump wrapper in `codex-api`; keep the core API shape unchanged. |
| Per-message deflate | WebSocket config enables `permessage-deflate`. | Default tungstenite config is used. | Higher bandwidth and possible mismatch with newer service expectations. | Add `websocket_config()` and switch to `connect_async_tls_with_config`. |
| Custom CA parity | WebSocket uses the same custom-CA rustls setup as HTTPS. | WebSocket uses default tungstenite TLS. | Users relying on custom enterprise CAs may have HTTPS work while WebSocket fails. | Add only if the old branch already has the helper available; otherwise make it a small prerequisite or defer from MVP. |
| Default header parity | Provider/request headers are merged with default HTTP headers. | WebSocket connect receives provider headers plus extra headers only. | WebSocket can omit headers sent by HTTP, e.g. user-agent/originator defaults. | Add a local `merge_request_headers` helper and pass default headers into `connect`. |
| Client metadata | Body-level `client_metadata` carries Codex identity and trace context. | Only headers carry turn metadata; no `client_metadata`. | Server-side analytics/routing cannot see the newer metadata envelope on WebSocket requests. | Add `client_metadata: Option<HashMap<String, String>>` to `ResponseCreateWsRequest`; initially include turn metadata and subagent/source fields available in this branch. |
| Incremental comparison with WS-only fields | Latest compares a canonical request shape, not transient WebSocket-only fields. | The old branch compares `ResponseCreateWsRequest` after clearing only `input`. | Adding `generate` or per-request `client_metadata` can accidentally block valid incremental reuse. | Normalize `generate` and `client_metadata` out of the comparison, or compare a canonical request shape if a larger refactor is already being done. |
| Connect timeout | WebSocket connect attempts use a bounded connect timeout. | Only stream idle timeout is configured. | A stalled connect can delay fallback/retry longer than intended. | Add a fixed timeout if no provider config exists; avoid widening config unless needed. |
| Server model reporting | Handshake and event payloads can emit `ResponseEvent::ServerModel`; latest may warn/reroute when server model differs. | `openai-model` is ignored. | Model reroute/account-state signals are invisible. | First port event parsing and logging; add UI events only if the branch wants the newer cyber/trusted-access UX. |
| Model verification recommendations | Latest parses `response.metadata.openai_verification_recommendation` and emits `EventMsg::ModelVerification`. | No protocol or response event. | Backend recommendations are invisible. | Optional surfaced feature; requires a tiny protocol enum/event plus TUI/app-server handling. |
| `response.completed.end_turn` | Latest preserves `end_turn` and treats `false` as follow-up-needed. | `ResponseEvent::Completed` drops it. | Model-directed continuation can be missed. | Add `end_turn: Option<bool>` to `ResponseEvent::Completed` and carry it through `core/src/codex.rs`. |
| `response.incomplete` | Latest turns it into a stream error. | Event is unhandled. | The client can wait for `response.completed` even though the service declared the response incomplete. | Add parser branch in shared SSE/WebSocket event processing. |
| Custom-tool input deltas | Latest emits `ResponseEvent::ToolCallInputDelta` for `response.custom_tool_call_input.delta`. | Deltas are ignored until final output item. | UI cannot stream large custom-tool call input diffs. | Optional UI feature. It depends on the old branch's tool-call-diff consumer shape; avoid porting if no consumer exists. |
| Fallback on `426 Upgrade Required` | Latest switches immediately to HTTP fallback for this session. | The connect error goes through generic retry/fallback. | First turn can waste WebSocket retries before HTTP fallback. | Add the narrow special-case in `stream_responses_websocket`. |

## Upstream-Fixed Bugs Still Present In `v0.98`

### 1. Stream error can hang behind WebSocket close handshake

`v0.98` closes the stream on terminal error before sending the error to the consumer. If the peer does not answer the close handshake, the consumer may not see the failure promptly.

Latest fixes this by taking the stream out of the mutex, dropping it, and sending the original error.

Minimal fix: change only `ResponsesWebsocketConnection::stream_request` in `codex-api/src/endpoint/responses_websocket.rs`.

### 2. Wrapped WebSocket errors are ignored

Current WebSocket errors are JSON messages such as:

```json
{
  "type": "error",
  "status": 400,
  "error": {
    "code": "previous_response_not_found",
    "message": "Previous response with id 'resp_abc' not found.",
    "param": "previous_response_id"
  }
}
```

`v0.98` attempts to deserialize text messages as `ResponsesStreamEvent`. Since `type: "error"` is not a normal Responses stream event, the payload is effectively dropped.

Latest parses wrapped errors before `ResponsesStreamEvent` parsing.

Minimal fix: port the wrapped-error structs and mapping helpers. Do not change the public core API.

### 3. 60-minute connection-limit errors are not retryable

The current service may send:

```json
{
  "type": "error",
  "status": 400,
  "error": {
    "type": "invalid_request_error",
    "code": "websocket_connection_limit_reached",
    "message": "Responses websocket connection limit reached (60 minutes). Create a new websocket connection to continue."
  }
}
```

`v0.98` does not recognize this payload. Latest maps it to `ApiError::Retryable` so the existing stream retry budget can open a new WebSocket connection.

Minimal fix: make this one wrapped-error code retryable and reset/drop the failed connection before retry.

### 4. Idle ping/pong handling is incomplete

`v0.98` handles `Message::Ping` only in the active response-stream read loop. When the connection is idle or when request serialization holds the stream lock, there is no independent read pump.

Latest moves ping/pong into a small background pump owned by the WebSocket connection.

Minimal fix: port the pump wrapper without adopting latest's broader `ModelClient` refactor.

### 5. `response.incomplete` is ignored

Latest treats `response.incomplete` as an error with the service-provided incomplete reason. `v0.98` logs it as an unhandled event and continues waiting for completion.

Minimal fix: add a `response.incomplete` branch to `codex-api/src/sse/responses.rs`.

### 6. Server-overloaded and cyber-policy errors are underclassified

Latest maps `server_is_overloaded` / `slow_down` and `cyber_policy` to explicit error variants. `v0.98` falls through to generic retryable or invalid-request behavior depending on payload shape.

Minimal fix: add explicit `ApiError` variants only if the old branch's user-facing error taxonomy should match latest. Otherwise, keep this out of the first correctness patch and list it as parser parity work.

### 7. `426 Upgrade Required` wastes retry budget

Latest treats a WebSocket handshake `426 Upgrade Required` as a signal to switch to HTTP fallback immediately. `v0.98` sends it through generic stream error mapping and retry/fallback.

Minimal fix: in `ModelClientSession::stream_responses_websocket`, return a local `FallbackToHttp` outcome or directly call `try_switch_fallback_transport(...)` on this status.

## Pause/Continue Constraints

The old branch's `/pause` and `/continue` implementation is a fork-specific constraint.

Back-ported WebSocket changes must preserve these invariants:

- A paused turn must not persist a synthetic user prompt.
- `/continue` must rebuild from durable completed history, not from partial text/reasoning deltas.
- A stream aborted by `/pause` must not leave `websocket_last_response_rx` or `previous_response_id` state that can be reused for a later request.
- If a stream ended before `response.completed`, the client must not assume that the previous response ID is valid.
- Warmup responses with `generate: false` are connection/request setup artifacts, not model-visible rollout content.
- If cross-turn WebSocket session caching is added, pause, interruption, compaction, rollback, and fallback must invalidate cached connection-local previous-response state unless a safe latest-response chain is known.

The safest rule for the first port is:

> WebSocket incremental state is valid only after a completed response. Any pause, stream error, cancellation, compaction, or rollout rewrite clears that incremental state.

## Non-goals

Do not include these in the minimal back-port:

- The latest `core/src/session/*` module refactor.
- The latest model-provider/auth-provider stack rewrite.
- Full startup prewarm scheduling before the first user turn.
- The latest rollout/inference trace subsystem.
- Full model-reroute/trusted-access UX unless the branch explicitly wants that surfaced behavior.
- Realtime/WebRTC changes unrelated to Responses WebSocket.

## Recommended Back-port Levels

| Level | Contents | Rationale |
|---|---|---|
| P0 correctness | drop-on-error, wrapped-error mapping, connection-limit retry, `response.incomplete`, `426` fallback | Fixes observable bugs with low dependency cost. |
| P1 wire parity | SSE fixture suppression, v2 `response.create` only, `generate`, `client_metadata`, header merge, `openai-model` parsing | Aligns request/response surface with current service contract. |
| P2 transport parity | connect timeout, pump-based ping/pong, `permessage-deflate` if dependency-compatible, custom CA only if a small helper exists | Improves connection reliability and enterprise parity without forcing a networking stack upgrade. |
| P3 acceleration | request prewarm, optional cross-turn cached WebSocket session with invalidation | Adds latency features after correctness is stable. |
| P4 optional surfaced UX | model verification, model reroute warnings, custom-tool input deltas | User-facing features that require protocol/UI follow-through. |
