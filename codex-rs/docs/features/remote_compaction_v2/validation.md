# Streaming-compaction validation

Select tests by the affected contract. The linked suites are regression entry points, not a statement that a particular build or deployment has passed them.

|Area|Source and cases to preserve|
|---|---|
|Selection and protocol|[Compaction session tests](../../../core/src/session/compaction_tests.rs): provider/master/v2 selection; request-only trigger serialization/exclusion; feature header and applicable schemas.|
|Collector and installation|[V2 unit tests](../../../core/src/compact_remote_v2_tests.rs): singleton completed summary, unrelated output, invalid cardinality, EOF/failure, usage, cancellation and snapshots arriving during the stream.|
|Retention|[History tests](../../../core/src/compact_remote_v2_history_tests.rs): stale context, summaries, Unicode/escaping, newest-first boundaries, image-only messages, atomic labels, no older backfill and intact base64.|
|HTTP transport|[V2 integration](../../../core/tests/suite/compact_remote_v2.rs): ChatGPT/API-key settings, ordinary path/tools/cache parity, retry limits, manual/automatic follow-up, notes and real request counts.|
|Downshift and restart|[Downshift suite](../../../core/tests/suite/compact_remote_v2_downshift.rs): distinct source/destination providers and instructions, failed/paused capture, acknowledged input, explicit recovery selection and durable reference context.|
|Notes recovery|[Recovery suite](../../../core/tests/suite/compact_remote_v2_recovery.rs): failed capture/compaction, Continue/new input, restart and exact notes tails.|
|Local and unary interaction|[Local recovery](../../../core/tests/suite/compact_local_recovery.rs) and [unary integration](../../../core/tests/suite/compact_remote.rs): explicitly select unary, retain call IDs/status during output fitting, keep failed local summarizer output out of history.|
|WebSocket lifecycle|[Client tests](../../../core/src/client.rs) and [WebSocket suite](../../../core/tests/suite/client_websockets.rs): reset logical continuation and reuse only completed healthy sockets.|
|Replay/accounting|[Session compaction tests](../../../core/src/session/compaction_tests.rs), [history tests](../../../core/src/context_manager/history_tests.rs) and [recorder tests](../../../core/src/rollout/tests.rs): exact replacement/reference, covered notes baseline, encrypted-only inline recovery, rollback and durable dangling-call cleanup.|

## Focused commands

```bash
CODEX_SANDBOX_NETWORK_DISABLED=1 cargo-local test -p codex-core --lib compact -- --test-threads=2
CODEX_SANDBOX_NETWORK_DISABLED=1 cargo-local test -p codex-core --test all compact_remote_v2 -- --test-threads=2
```

Use shared wrappers and the existing `skip_if_no_network_unless_localhost!` guard for mock transport. Let the wrapper supply its loopback capability. Do not count an early-return network guard as transport execution. Explicit unary fixtures must disable `RemoteCompactionV2`; otherwise they exercise a different request/response contract.

Regenerate the affected config/protocol artifacts only when their source schemas change. Expand to related suites when changing shared output handling, usage accounting, pause/resume or task-stop behavior.

## High-value interaction cases

- Notes in commentary followed by reasoning and empty/whitespace final output; compare the entire emitted note with follow-up and replay, including text after the closing tag.
- Multipart output with split sentinels/code and plan tags on separate lines; same-line tags do not exercise the plan parser.
- Rejected tool outputs after notes, interruption before rejection recording, and later acknowledged user input outside the bounded capture.
- Failed local summarization attempts followed by Continue/new input and restart; failed output must be absent from recovery requests and replacements.
- Downshift notes reporting little usage, followed by failure/pause and a new recovery destination; compaction still uses the original source while replacement fits the destination.
- Snapshot completion during the remote stream or installation; compare live/replayed history and adjacent reference context.
- A real recorder blocked after memory replacement, stopped through actual pause/interrupt handlers; stopping must wait for installation. Read the durable file without a test-side flush and replay it.
- Long retained notes and local recomputation after restart; the notes must not be charged twice or trigger a spurious repeat compaction.
- Standalone compaction during paused/interrupted review-derived work; `/continue` must preserve the owner, exact review frame and selected model.

## Evidence limits

Local fixtures establish client routing, request shape, state ordering and replay. They do not establish model summary quality, server cache hits, server latency or deployed support for every model/provider. Use [native acceptance](native_acceptance.md) for a synthetic deployed-endpoint check and report it separately. Retention remains approximate and cannot predict arbitrary input appended after pre-turn compaction.
