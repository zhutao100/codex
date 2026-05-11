# Responses WebSocket Inspection Matrix

## Compared files

|Purpose|This project|Upstream project|
|---|---|---|
|Wire/API request and event types|`codex-api/src/common.rs`|`codex-api/src/common.rs`|
|WebSocket endpoint implementation|`codex-api/src/endpoint/responses_websocket.rs`|`codex-api/src/endpoint/responses_websocket.rs`|
|Shared Responses stream parser|`codex-api/src/sse/responses.rs`|`codex-api/src/sse/responses.rs`|
|Client transport selection and incremental state|`core/src/client.rs`|`core/src/client.rs`|
|Turn lifecycle call sites|`core/src/session/*`|`core/src/session/*`|
|Remote compaction acknowledgement|`core/src/compact_remote_v2.rs`, if present at target revision|`core/src/compact_remote_v2.rs`|
|WebSocket regression tests|`core/tests/suite/client_websockets.rs`; `core/tests/suite/websocket_fallback.rs`|`core/tests/suite/client_websockets.rs`; `core/tests/suite/websocket_fallback.rs`|

## Already backported in this project

|Feature/fix|Evidence in this project|Action|
|---|---|---|
|V2 `response.create` continuation|`prepare_websocket_request(...)` constructs `ResponsesWsRequest::ResponseCreate` with `previous_response_id` and delta input.|Keep and test; do not re-port.|
|`generate` and `client_metadata` fields|`ResponseCreateWsRequest` has both fields.|Keep and test; do not re-port.|
|Ignore WebSocket-only fields in incremental comparison|`get_incremental_items(...)` clears `generate` and `client_metadata` before non-input comparison.|Keep and test; do not re-port.|
|SSE fixture suppression|`responses_websocket_enabled()` checks `(*CODEX_RS_SSE_FIXTURE).is_none()`.|Keep and test.|
|426 fallback|`stream_responses_websocket(...)` returns `FallbackToHttp` for `StatusCode::UPGRADE_REQUIRED`; `stream(...)` switches transport.|Keep and test.|
|Connect timeout|`tokio::time::timeout(DEFAULT_WEBSOCKET_CONNECT_TIMEOUT, connect)` wraps connect.|Keep and test.|
|Background pump|`WsStream::new(...)` spawns a pump task and responds to ping with pong.|Keep and test.|
|Drop failed stream|`stream_request(...)` takes and drops the failed stream on terminal error.|Keep and test.|
|Wrapped error mapping|`parse_wrapped_websocket_error_event(...)` and `map_wrapped_websocket_error_event(...)` exist.|Keep and test.|
|Connection-limit retry|`websocket_connection_limit_reached` maps to `ApiError::Retryable`.|Keep and test.|
|Header merge precedence|`merge_request_headers(...)` has provider > extra > default behavior.|Keep and test.|
|`response.incomplete`|Shared SSE parser maps `response.incomplete` to a stream error with the incomplete reason.|Keep and test.|
|`end_turn`|`ResponseEvent::Completed` includes `end_turn: Option<bool>` and parser preserves it.|Keep and test.|

## Remaining gaps and minimal backport decision

|Gap|Category|Worth minimal backport?|Notes|
|---|---|---|---|
|Bound send timeout|Correctness|Yes, P0|Small local helper. No upstream refactor needed.|
|`response.processed`|Protocol parity|Yes, P0|Feature-gated; call only after successful completed-response processing.|
|Consumer-drop cancellation|Correctness|Yes, P0|Needed for prompt cleanup after `/pause`, interruption, or cancellation; no protocol changes required.|
|Handshake `openai-model` and stream model metadata|Parser parity|Yes, P1|Add `ServerModel` event. Defer UX.|
|Model verification metadata|Parser parity|Maybe, P1|Useful server signal; may require protocol type.|
|Custom-tool input delta|Parser parity|Maybe, P1|Useful only if a consumer can apply deltas.|
|Provider stream error classification|Error parity|Yes, P1|Classify `cyber_policy`, `server_is_overloaded`, and `slow_down` instead of treating all unknown failures as retryable.|
|`permessage-deflate`|Transport parity|Maybe, P2|Dependency feasibility first; likely needs direct/forked tungstenite support.|
|WebSocket custom CA|Enterprise transport parity|Optional, P2|This project lacks the upstream custom-CA helper, so this should not block other work.|
|Trace/install/window metadata|Observability|Maybe, P3|Trace can be narrow; install/window IDs require extra session state.|
|Preconnect|Latency|Maybe, P4|Safe if turn-scoped and no prompt payload is sent.|
|`generate:false` prewarm|Latency/protocol feature|Maybe, P4|Request field exists; call path must avoid rollout side effects.|
|Cross-turn cached WebSocket session|Latency|Defer, P4/P5|High interaction risk with sticky turn-state, `/pause`, `/continue`, compaction, and rollback.|
|Multi-limit rate-limit schema|Diagnostics|Defer|Not WebSocket-minimal; requires protocol schema expansion.|
|`upstream_request_id` response debug context|Diagnostics|Defer|Useful but tied to broader upstream telemetry plumbing.|

## Upstream tests worth selectively mirroring

|Upstream test intent|Backport patch|
|---|---|
|`responses_websocket_sends_response_processed_when_feature_enabled`|Patch 2|
|`responses_websocket_omits_response_processed_without_feature`|Patch 2|
|`responses_websocket_sends_response_processed_after_remote_compaction_v2`|Patch 2, if remote compaction v2 call site is present|
|send-timeout/terminal-error behavior without close-handshake|Patch 1; terminal-error close-handshake behavior is already backported and should remain covered|
|mapper exits when `ResponseStream` is dropped|Patch 2a|
|server model emitted from handshake/stream metadata|Patch 3|
|model verification field emitted|Patch 4|
|custom-tool input delta emitted|Patch 5|
|`cyber_policy`, `server_is_overloaded`, and `slow_down` are classified|Patch 5a|
|`websocket_config_enables_permessage_deflate`|Patch 6|
|preconnect reuses connection|Patch 9|
|request prewarm sends `generate:false` and chains correctly|Patch 10|
|cross-turn WebSocket reuse and invalidation|Patch 11 only|

## Implementation cautions

- Keep the OpenAI path on `response.create`; do not reintroduce `response.append` construction.
- Keep `ResponseAppend` type definitions only for compatibility until a separate cleanup removes exports safely.
- Do not let `generate`, request-start metadata, or trace metadata block incremental reuse.
- Do not treat warmup completion as assistant output.
- Do not reuse WebSocket previous-response state across `/pause`, `/continue`, stream errors, compaction, rollback, or fallback without explicit tests.
- Do not make custom CA or `permessage-deflate` prerequisites for `response.processed` or send timeout.
- Do not leave a mapped provider stream running after the core response consumer is dropped.
