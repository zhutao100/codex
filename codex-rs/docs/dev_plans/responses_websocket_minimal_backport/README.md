# Responses WebSocket Minimal Backport

## Outcome

This plan updated the prior WebSocket backport proposal after inspecting the implementation in this project, the corresponding implementation in the upstream project, the `/pause` and `/continue` lifecycle, and the current Responses WebSocket contract.

The previous proposal was based on an older intermediate state. Several changes it proposed are already present in this project, while one proposed addition, `response.processed`, has since been removed from the upstream project and is not part of the current public WebSocket contract.

## Implementation status

Completed in this branch:

- Removed obsolete non-create WebSocket request shapes: `response.processed` and `response.append`.
- Serialized WebSocket requests directly to the outbound wire string.
- Replaced clone-heavy incremental request eligibility checks with borrowed comparison and prefix checks.
- Carried `x-codex-turn-state` through request-scoped `response.create.client_metadata`, seeded by `response.metadata` when present.
- Cached only healthy physical WebSocket connections across logical turns, while clearing all logical continuation state at turn boundaries.

Deferred validation work:

- In-flight WebSocket `/pause`/`/continue` E2E coverage with the first connection held open until the WebSocket mock server can accept concurrent scripted connections.
- Full WebSocket compaction same-socket E2E coverage; the cache-specific continuation-clear behavior is unit-covered.

Deferred optional work:

- Shared custom CA and rustls provider parity for secure WebSockets and HTTP.
- Provider-configurable WebSocket connect timeout.

## Recommended sequence

|Order|Change|Status|Why|
|---|---|---|---|
|1|Remove dormant non-create WebSocket request paths|Done|Eliminates obsolete protocol shapes, feature flag plumbing, call sites, and tests.|
|2|Serialize WebSocket requests directly to the wire string|Done|Removes an unnecessary full `serde_json::Value` allocation and second traversal.|
|3|Compare incremental requests by reference|Done|Avoids cloning the full previous request, current request, and history on every tool round trip.|
|4|Move `x-codex-turn-state` to request-scoped WebSocket metadata|Done|A physical connection can span logical turns only if sticky turn state remains turn-scoped.|
|5|Reuse only the physical WebSocket connection across logical turns|Done (runtime)|Removes repeated handshakes while preserving this project's `/pause` and `/continue` durability model. Deferred E2E validation items are listed above.|
|6|Custom CA and rustls provider parity|Deferred optional|Fixes enterprise TLS interception and P-521 certificate-chain failures, but requires a small shared TLS prerequisite.|
|7|Provider-configurable WebSocket connect timeout|Deferred optional|Small operational hardening; not required for the other changes.|

## Deliberate boundary

The first connection-reuse backport should cache only `ResponsesWebsocketConnection`. It should not cache the previous logical request, response ID, or response output across turns.

That split is intentional:

- Same-turn tool round trips retain the current incremental `previous_response_id` behavior.
- A new logical turn reuses the socket but starts with a full `response.create` and no `previous_response_id`.
- A paused or cancelled in-flight stream is never returned to the connection cache.
- A remotely closed cached socket is discarded before a later turn opens a replacement connection.
- `/continue` reconstructs from durable history and reconnects when the paused stream did not complete.
- Standalone compaction can keep a healthy socket while clearing the logical continuation chain.

A later, separately reviewed phase may cache the logical response chain, but only through an explicit durability commit from the turn loop. Automatic cross-turn chain reuse on `Drop` is not safe enough for this project's custom lifecycle.

## Corrected baseline

The following behavior is already implemented and is excluded from new backport work:

- WebSocket send timeout.
- Background ping/pong pump.
- Immediate terminal-error propagation without waiting for a close handshake.
- `permessage-deflate`.
- Handshake and stream server-model events.
- Model verification events.
- Custom-tool input deltas.
- `cyber_policy`, `server_is_overloaded`, and `slow_down` classification.
- Consumer-drop cancellation in the core stream mapper.
- 426 fallback to HTTP.
- 60-minute connection-limit recovery.
- Same-turn incremental `response.create` requests.
- `response.incomplete` and `response.completed.end_turn` parsing.

## Documents

- [`problem_statement.md`](problem_statement.md): inspected architecture, protocol baseline, and corrected problem definition.
- [`inspection_matrix.md`](inspection_matrix.md): feature-by-feature comparison, remaining gaps, and upstream-fixed bugs still present here.
- [`design_proposal.md`](design_proposal.md): minimal patch sequence, ownership model, prerequisites, and deferrals.
- [`validation_plan.md`](validation_plan.md): tests, local command wrappers, and acceptance criteria for each patch.
