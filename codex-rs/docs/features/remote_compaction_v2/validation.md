# Validation

Verification date: 2026-09-06. Implementation, generated artifacts and local acceptance checks are complete.

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
|Notes|Exact multiline notes; transient suffix exclusion; pre-turn schema skip; previous-model capture; pause/restart recovery before sampling; rejection of later work; late undo snapshots.|
|Lifecycle|Matching start/completion; cancellation before installation; forced parent abort while persistence is blocked; healthy socket reuse and unfinished-socket rejection.|
|Replay|Exact replacements; encrypted-only inline versus standalone provenance; completed and incomplete tails; rollback; compaction-relative durable dangling-call cleanup.|
|Legacy|Explicit legacy selection; output placeholder rewrite retaining calls and success flags; base-instruction estimate; unchanged local selection.|

## Results

|Check|Result|
|---|---|
|Core package, final guarded run|1,174 unit tests passed; 4 ignored. Integration: 685 reported passes, including 538 named tests returning at the existing external-network guard; 147 ran past that guard, 11 were ignored and 44 excluded by the nested-sandbox skip list. Five doctests passed.|
|Focused compaction run after adding local-fallback and remote pending-input cases|45 unit tests and 38 integration cases passed. The final package run additionally covers the reset guard that discards unfinished notes sockets after local/legacy compaction. No v2 fixture returned at the network guard.|
|Protocol, telemetry and app-server protocol packages|123 tests passed across nine suites.|
|Schema generation|Config JSON and affected app-server JSON/TypeScript fixtures regenerated; schema tests passed.|
|Formatting/lints/build|`just-local fmt`, scoped `just-local fix`, and `cargo-local build -p codex-cli --bin codex` completed. Existing lint warnings remain in unrelated task/util code and a pre-existing pending-input helper.|

The initial raw-output core run included seven failures in the pre-existing shell/Seatbelt exclusion categories because raw mode bypasses the wrapper's skip list. The subsequent normal-wrapper run applied those exclusions and passed. The complete workspace `--all-features` test suite was not run; scoped Clippy used all features for the affected crates.

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

The rebuilt CLI/app-server smoke timed out during ordinary generation before issuing compaction, both with direct transport and a temporary localhost forwarder. Thus authenticated protocol acceptance is verified separately from the client lifecycle: manual/pre-turn/mid-turn compaction, pause/restart, pending input and replay are verified by executing loopback fixtures. A direct authenticated automatic-compaction/restart run of the rebuilt executable remains unverified.

## Limits

Local fixtures prove client routing and lifecycle behavior, not model summary quality, prompt-cache hits or server latency. Retention uses approximate estimates and cannot account for arbitrary new input after pre-turn compaction. Other provider families and model compatibility across opaque summaries remain outside this backport's eligibility policy.
