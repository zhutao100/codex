# Responses WebSocket Minimal Backport - Validation Plan

## Validation principles

- Test observable wire requests and handshake counts, not private implementation details alone.
- Keep all WebSocket server scripts sequential because the production connection does not multiplex responses.
- Assert full-create fallback whenever state is ambiguous.
- Cover `/pause`, `/continue`, compaction, stream error, and HTTP fallback explicitly.
- Run allocation-oriented unit tests alongside existing integration tests; performance fixes must preserve exact wire JSON.

## Patch 1 - Protocol cleanup

|Check|Expected result|
|---|---|
|`rg -e ResponseAppend -e ResponseProcessed -e send_response_processed -e responses_websocket_response_processed`|No matches in source, tests, or schema.|
|Successful WebSocket turn|Exactly the expected `response.create` requests; no post-completion request.|
|Feature parsing|The removed under-development feature key is no longer accepted/generated.|
|Existing incremental tests|Unchanged behavior.|

Run a workspace-wide symbol search for both removed request shapes so the public API cleanup remains explicit.

## Patch 2 - Direct serialization

### Unit test

Construct a request containing:

- Model and instructions.
- A user message with content.
- A tool schema.
- Reasoning options.
- Include fields.
- Service tier and prompt cache key.
- `generate: false`.
- Multiple `client_metadata` entries.

Assert:

```rust
serde_json::from_str::<serde_json::Value>(&serialize_websocket_request(&request)?)?
    == serde_json::to_value(&request)?
```

### Regression checks

- The existing mock server can deserialize the request exactly as before.
- Send timeout and telemetry callbacks still run once per request.
- Serialization failure remains an `ApiError::Stream` with the existing message prefix.

## Patch 3 - Borrowed incremental comparison

### Property matrix

|Mutation from previous request|Incremental eligible?|
|---|---|
|Append new input after previous input and returned output items|Yes|
|No new input and `allow_empty_delta = true`|Yes|
|No new input and `allow_empty_delta = false`|No|
|Change `client_metadata` only|Yes|
|Change `generate` only|Yes|
|Change `previous_response_id` field in the comparison fixture|Ignored by logical comparison|
|Change model|No|
|Change instructions|No|
|Change tool schema or order|No|
|Change tool choice|No|
|Change parallel-tool setting|No|
|Change reasoning|No|
|Change store or stream|No|
|Change include list|No|
|Change service tier|No|
|Change prompt cache key|No|
|Change text controls|No|
|Remove, rewrite, or reorder prior input|No|
|Reorder returned response items|No|

### Existing integration tests to retain

- Prefix sends incremental create.
- Non-prefix sends full create.
- Changed non-input fields send full create.
- Error clears the previous-response chain.
- Turn metadata appears on both initial and same-turn incremental requests.

## Patch 4 - Request-scoped turn state

Use a mock server sequence:

1. Accept one handshake.
2. Receive first `response.create`; assert no `x-codex-turn-state` client metadata.
3. Send `response.metadata` with `headers: {"x-codex-turn-state": "state-a"}`.
4. Complete the first response.
5. Receive a same-turn second `response.create`; assert metadata contains `state-a`.
6. Send metadata with `state-b` and complete.
7. Assert a third same-turn request still contains `state-a`.

Then start a new logical turn on the same physical socket:

- Its first request must not contain `state-a`.
- Metadata from that turn may establish a fresh value.

Also test case-insensitive header names and array-valued JSON header representations if the parser supports them.

## Patch 5 - Physical connection cache

### Core handshake/request matrix

|Scenario|Handshakes|First request of later logical turn|Connection cached?|
|---|---:|---|---|
|Two successful ordinary turns|1|Full create, no previous response ID|Yes|
|First turn has multiple tool round trips, then second turn|1|Full create; same-turn follow-up was incremental|Yes|
|Pause while first response is in flight, then `/continue`|2|Full create from durable history|No after pause|
|Consumer drops after provider stream error|2 on next turn|Full create|No|
|Server sends connection-limit error|2 within retry flow|Full create after reconnect|Old connection no|
|HTTP 426 on handshake|1 attempted|HTTP request|No|
|WebSocket retry budget exhausted|No later WebSocket attempt|HTTP request|No|
|Compaction after completed response|1|Full compacted input, no previous response ID|Yes|
|Server closes cached completed connection before next turn|2|Full create, no previous response ID|Stale slot dropped before reuse|
|Provider override changes endpoint|2|Full create|No cross-provider adoption|
|Auth mode changes ChatGPT/API endpoint|2|Full create|No cross-mode adoption|

### `/pause` integration test shape

The mock server should deliberately keep the first response open after sending a non-terminal delta. The test should:

1. Start a turn with WebSockets enabled.
2. Wait until the first request is received.
3. Issue pause and wait for `TurnPaused`.
4. Issue continue.
5. Assert a second handshake occurs.
6. Assert the continued request contains reconstructed durable history, has no `previous_response_id`, and does not include partial response deltas from the paused stream.

Do not let the first mock connection close before the pause; otherwise the test proves reconnect-after-close rather than the cache safety rule.

Current validation note: `responses_websocket_pause_continue_reconnects_after_in_flight_response` covers this with a concurrent scripted WebSocket server and a first connection that remains open until test shutdown.

### Drop-state unit tests

Directly exercise the cacheability helper:

- No request and an adopted connection: cacheable.
- Completed receiver: cacheable.
- Empty receiver: not cacheable.
- Closed receiver: not cacheable.
- Fallback active: not cacheable.
- Clearing logical continuation before drop returns the physical socket to the cache.
- Cached provider/auth-mode mismatches are rejected without adopting the socket.
- HTTP fallback clears the shared cached socket.

### Concurrency

Create two `ModelClientSession`s from one `ModelClient` before either returns a connection. Verify the one-slot cache is taken atomically and a connection is never shared concurrently. It is acceptable for the second session to open another socket.

Also verify provider overrides use a separate connection and the one-slot cache does not cross provider boundaries.

### Compaction integration test shape

The WebSocket compaction integration test should drive:

1. A completed ordinary turn that returns a physical WebSocket connection to the cache.
2. A manual or automatic compaction turn that reuses that physical socket.
3. A later ordinary turn that reuses the same physical socket, sends a full compacted input, and omits `previous_response_id`.

Current validation note: `responses_websocket_compacted_followup_reuses_connection_with_full_create` covers same-socket reuse and full post-compaction input at the WebSocket client layer. `clear_websocket_continuation_keeps_physical_connection_cacheable` covers the cache-specific continuation clear used by session compaction, and the existing compact suites cover history replacement separately.

## Deferred cross-turn logical chain tests

Do not enable cross-turn `previous_response_id` reuse until these pass:

- Successful turn commits a durable chain and the next turn sends only new input.
- Pause before completion never commits a chain.
- Pause after `response.completed` but before turn persistence does not commit prematurely.
- Compaction clears the chain but retains the physical connection.
- Rollback, review rewrite, and rollout reconstruction reject a stale chain.
- A failed continuation evicts the chain and the retry sends full context.
- A prefix mismatch sends full context on the same physical connection.

## Optional TLS validation

### Unit

- `CODEX_CA_CERTIFICATE` takes precedence over `SSL_CERT_FILE`.
- Empty values are treated as unset.
- Multiple PEM certificate blocks are loaded.
- Non-certificate PEM sections do not become roots.
- Missing file, unreadable file, malformed PEM, and rejected certificate errors are distinguishable.
- The installed rustls provider advertises `ECDSA_NISTP521_SHA512`.

### Hermetic transport

Run a local HTTPS/WebSocket server with a certificate signed by a test CA not present in native roots.

|Configuration|HTTP|WebSocket|
|---|---|---|
|No custom CA|Fails trust validation|Fails trust validation|
|`SSL_CERT_FILE` points to test CA|Succeeds|Succeeds|
|`CODEX_CA_CERTIFICATE` points to test CA|Succeeds|Succeeds|
|Both set to different CAs|Uses `CODEX_CA_CERTIFICATE` for both|Uses `CODEX_CA_CERTIFICATE` for both|

## Optional timeout validation

- Omitted provider value uses the selected default.
- Explicit low timeout fails a stalled handshake with `TransportError::Timeout`.
- Explicit higher timeout allows a deliberately delayed local handshake.
- Timeout failure clears the current session connection and does not populate the shared cache.

## Suggested commands

Run formatting and the focused crates first:

```shell
just fmt
CODEX_SANDBOX_NETWORK_DISABLED=1 scripts/cargo-local test -p codex-api
CODEX_SANDBOX_NETWORK_DISABLED=1 scripts/cargo-local test -p codex-core --test all client_websockets
CODEX_SANDBOX_NETWORK_DISABLED=1 scripts/cargo-local test -p codex-core --test all websocket_fallback
CODEX_SANDBOX_NETWORK_DISABLED=1 scripts/cargo-local test -p codex-core --test all abort_tasks
```

Then run the broader affected workspace tests required by the branch's normal validation policy. Regenerate `core/config.schema.json` through the project's schema command when feature or provider configuration changes.

## Documentation checks

Before landing:

- Verify every path in this plan is project-relative.
- Verify the plan does not claim already-present behavior is missing.
- Verify `response.processed` is described as removal, not addition.
- Verify physical and logical reuse remain separate phases.
- Verify pause/cancel with an in-flight receiver is a cache miss.
