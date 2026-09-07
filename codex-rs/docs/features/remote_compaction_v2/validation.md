# Validation

Updated: 2026-09-07. Implementation, generated artifacts and guarded checks are complete. Authenticated client acceptance passed through the temporary HTTPS forwarder; direct native HTTPS remains unresolved.

## Reproducible checks

Use `CODEX_SANDBOX_NETWORK_DISABLED=1` for tests and the shared wrappers. Builds/tests are limited to two jobs/test threads and low scheduling priority.

```bash
just-local fmt
just-local fix -p codex-core -p codex-protocol -p codex-otel --allow-staged
just-local write-config-schema
just-local write-app-server-schema
CODEX_SANDBOX_NETWORK_DISABLED=1 cargo-local test -p codex-core -- --test-threads=2
CODEX_SANDBOX_NETWORK_DISABLED=1 cargo-local test -p codex-protocol -p codex-otel -p codex-app-server-protocol -- --test-threads=2
CODEX_SANDBOX_NETWORK_DISABLED=1 cargo-local test -p codex-core --test all compact_remote_v2 -- --test-threads=2
CODEX_SANDBOX_NETWORK_DISABLED=1 cargo-local test -p codex-core --test all measures_text_and_image_compaction_roundtrips -- --show-output --test-threads=2
```

The new transport fixtures use the loopback-aware guard; the wrapper supplies its capability flag after enforcing localhost-only networking. Tests using the older network-disabled guard elsewhere in the repository can report success after an early return. Distinguish their package totals from the explicitly executed v2 fixture coverage.

## Covered behaviors

|Area|Coverage|
|---|---|
|Protocol/selection|Exact trigger serialization; ordinary history, UI mapping and rollout exclusion; provider/master/v2 truth table; stable feature header; generated schemas.|
|Collector|Complete singleton summary; unrelated tool output ignored; missing/multiple outputs; incomplete/failed stream; usage before validation; attempt-local retry state.|
|HTTP|ChatGPT and API-key request construction; backend base path; ordinary tools/settings/cache parity; no legacy route; permanent 404 and configured retry budgets; manual and automatic follow-ups.|
|Retention|Mixed stale context; old summaries; interruption markers; newest-first boundaries; escaped Unicode; image-only histories; atomic labels; no older backfill; base64 preservation; fixed-context limits.|
|Notes|Exact multiline notes; bounded capture exclusion; pre-turn schema skip; cross-provider previous-model capture and missing-provider errors; interruption/new-input and restart recovery; preservation of later acknowledged input; rejection of unidentified work inside capture.|
|Lifecycle|Matching start/completion; failed compaction followed by immediate Continue, acknowledged steering and restart; cancellation before installation; forced parent abort while persistence is blocked; snapshots completing during the remote stream; healthy socket reuse and unfinished-socket rejection.|
|Replay|Exact replacements; encrypted-only inline versus standalone provenance; completed and incomplete tails; rollback; compaction-relative durable dangling-call cleanup.|
|Legacy|Explicit legacy selection; output placeholder rewrite retaining calls and success flags; base-instruction estimate; unchanged local selection.|

## Results

|Check|Result|
|---|---|
|Core package, final guarded run|1,178 reported unit passes; 4 ignored. Integration: 691 reported passes, 11 ignored and 44 excluded by the nested-sandbox skip list. Five reported doctest passes. Existing sandbox/network early returns are included in these package totals.|
|Focused compaction run|50 unit tests and 44 integration cases passed, including failure/Continue/restart with acknowledged steering, interrupted notes followed by new input, late snapshots, cross-provider downshift and missing-provider errors. None returned at a network guard.|
|Protocol, telemetry and app-server protocol packages|123 tests passed across nine suites.|
|Schema generation|Config JSON and affected app-server JSON/TypeScript fixtures regenerated; schema tests passed.|
|Formatting/lints/build|`just-local fmt`, scoped `just-local fix`, and `cargo-local build -p codex-cli --bin codex` completed. Existing lint warnings remain in unrelated task/util code and a pre-existing pending-input helper.|

The final package log contains 538 standard external-network guard notices across unit tests, integration tests and doctests, plus two alternate network notices and ten Seatbelt notices. One test can emit multiple notices; these are not counts of distinct tests. The focused compaction log contains no such notices. Package totals therefore must not be reported as executed transport coverage.

The initial raw-output core run included seven failures in the pre-existing shell/Seatbelt exclusion categories because raw mode bypasses the wrapper's skip list. The subsequent normal-wrapper runs applied those exclusions and passed. The complete workspace `--all-features` test suite was not run; scoped Clippy used all features for the affected crates.

## End-to-end fixture measurements

Same fixture source/settings before and after compaction; all traffic used the normal Responses path. These are debug-build localhost measurements, not server latency or allocation benchmarks.

|Fixture|Initial request bytes|Compaction request bytes|Follow-up request bytes|Local compaction time|Retained images|
|---|---|---|---|---|---|
|Text history|462,271|462,397|284,051|76.793 ms|0|
|50 remote-image inputs|35,829|35,955|33,554|4.701 ms|34|

Byte counts describe decoded JSON request bodies, not compressed wire bytes. Image URLs are small on the wire but each consumes the retained-image budget. The three requests in each fixture were initial work, compaction and follow-up.

## Authenticated deployment check

An authenticated minimal HTTP probe using `gpt-6-astra` on the reported ChatGPT deployment completed:

|Request|Result|Elapsed|
|---|---|---|
|Ordinary Responses generation|HTTP 200 and `response.completed`.|1.940 s|
|Responses input ending in `compaction_trigger`|HTTP 200, completion, exactly one encrypted compaction item.|4.139 s|
|Follow-up containing retained input, encrypted summary and the branch's exact work-notes wrapper after the summary|HTTP 200; recalled the synthetic codeword.|2.231 s|

The probe used only synthetic content and existing access credentials without refreshing or modifying the original credential store. It exercised the stable feature header, compact metadata, branch-version header and notes-after-summary ordering.

The rebuilt CLI's app-server also passed authenticated lifecycle acceptance against the same deployment:

|Scenario|Observed result|
|---|---|
|Manual compaction and follow-up|One completed compaction; subsequent ordinary turn recalled the synthetic codeword.|
|Automatic compaction|An actual tool round with 8,000 inert reference lines and a 30,000-token automatic threshold triggered one notes-bearing compaction, then completed with the expected codeword.|
|Exact notes and persistence|The notes embedded in the replacement matched the emitted assistant notes byte-for-byte; no trigger appeared in replacement history.|
|Restart/resume|A new app-server process resumed the persisted automatic-compaction thread and recalled the codeword in a new turn.|

The final automatic sequence sent 408,833 bytes initially, 411,599 bytes for notes capture, 410,913 bytes for compaction and 122,222 bytes for the first follow-up. The resumed request was 123,608 bytes. Counts are decoded JSON bodies.

This client acceptance used a temporary provider URL on localhost. A forwarder passed the actual client's request bodies and authentication headers unchanged to the ordinary ChatGPT Responses endpoint with curl, then returned the completed SSE body to the client. Thus the shipped client's compaction state machine, request construction, tool selection, history replacement and persisted resume all ran against real backend responses. It does not validate native TLS, network-stream timing or WebSocket transport. Loopback fixtures separately exercise incremental delivery, failure and cancellation.

Direct native HTTPS from the rebuilt executable still timed out during ordinary generation before compaction; TCP connected, while curl successfully reached the same deployment. The native transport cause is unresolved. No production transport workaround or URL rewrite was added. The harness needs an enlarged stdout reader for large user-message notifications; acceptance used an 8 MiB limit and drained subprocess output during shutdown.

Acceptance used a temporary state directory and synthetic workspace with shell execution, undo and remote-model refresh disabled. It copied existing access credentials without refreshing or modifying the original credential store. The temporary forwarder, harness and copied state were removed after validation.

## Limits

Local fixtures prove client routing and lifecycle behavior, not model summary quality, prompt-cache hits or server latency. Retention uses approximate estimates and cannot account for arbitrary new input after pre-turn compaction. Other provider families and model compatibility across opaque summaries remain outside this backport's eligibility policy.
