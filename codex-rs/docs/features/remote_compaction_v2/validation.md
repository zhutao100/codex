# Validation

Updated: 2026-09-07. Implementation, generated artifacts and guarded checks are complete. Authenticated native client acceptance passed in a clean macOS 15.6.1 VM, including manual/automatic compaction, exact notes, follow-up and restart. [Repeatable native acceptance](native_acceptance.md).

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
|Notes|Exact multiline/multipart notes, including split sentinels/code and standalone multiline plan tags; live notes versus recorded notes and replayed replacement; bounded capture exclusion; matching function/MCP/custom rejections before or after notes; interrupted capture cleanup; pre-turn schema skip; source-model metadata and malformed metadata rejection; distinct previous/destination instructions and final overrides; missing-provider errors; preservation of later acknowledged input.|
|Lifecycle/accounting|Matching start/completion; failure/Continue/steering/restart; eleven downshift recovery cases covering failure/pause, Continue/new input, restart, older captures and explicit destination-model changes; standalone compaction preserves pending main ownership; cancellation before installation; actual pause/interrupt handlers aborting a parent blocked on a real recorder; snapshots completing during the stream; healthy socket reuse; exact local token baselines; long notes below the limit do not trigger another pre-turn compaction after restart.|
|Replay|Exact durable replacement and adjacent reference read without a test-side flush; live/replayed history equality; source/destination selection before and after installation; encrypted-only inline versus standalone provenance; completed and incomplete tails; rollback; compaction-relative durable dangling-call cleanup.|
|Local/legacy|Explicit legacy selection; output placeholder rewrite retaining calls and success flags; base-instruction estimate; local fallback with two failed summarizer attempts, Continue/new input and restart; failed output absent from recovery requests and durable replacement.|

## Results

|Check|Result|
|---|---|
|Core, protocol and state recovery rerun|2,038 reported passes across nine suites; 16 ignored and 44 filtered. Existing sandbox/network early returns are included in these package totals.|
|Focused compaction rerun|127 reported passes across four suites, including all eleven downshift recovery cases. The v2 and local recovery fixtures use the loopback-aware guard; unchanged legacy guards elsewhere may still return early and are not additional transport coverage.|
|Protocol, telemetry and app-server protocol packages|123 tests passed across nine suites.|
|Schema generation|Config JSON and affected app-server JSON/TypeScript fixtures regenerated; schema tests passed.|
|Formatting/lints/build|`just-local fmt`, scoped `just-local fix`, and `cargo-local build -p codex-cli --bin codex` completed. Existing lint warnings remain in unrelated task/util code and a pre-existing pending-input helper.|
|Native checker|Python syntax and the actual multipart output-text extraction expression passed synthetic split-sentinel/code and input-text exclusion checks.|

Package totals include existing external-network and Seatbelt early returns and must not be reported as executed transport coverage. The dedicated v2 fixtures all executed past their guards.

Executing the older localhost fixtures exposed stale assumptions about work-notes requests, JSON-escaped notes and canonical-context placement. Those fixtures now include the notes phase and compare actual retained history and notes rather than removing fixed-position context entries.

Multiline plan-note and local-fallback regressions reproduced three failures before their fixes: standalone plan blocks were stripped, and unfinished local summary output blocked both Continue and new input. Restart cases already passed because the failed output was confined to live memory. Output handlers now share the capture parsing decision, and local attempts keep their output private until completion.

The initial raw-output core run included seven failures in the pre-existing shell/Seatbelt exclusion categories because raw mode bypasses the wrapper's skip list. The subsequent normal-wrapper runs applied those exclusions and passed. The complete workspace `--all-features` test suite was not run; scoped Clippy used all features for the affected crates.

## Notes Response-Tail Recovery

Verified 2026-11-01 in a disposable macOS 15.6.1 VM with two build jobs and two test threads.

- Recorded failure: completed notes in commentary contained formatting self-commentary after the closing tag, then a separate reasoning item and an empty final-answer message. The scanner rejected the reasoning as later work before making a remote compaction request; `/continue` reached the same guard. The suffix inside the original notes message was not the rejected item.
- Contract: accept trailing reasoning and empty/whitespace-only output-text assistant messages without changing exact captured notes or counting another attempt. Reject nonempty later messages, non-text content and unmatched tool activity without discarding history. Both live capture paths use the recovery scanner, so whitespace-only finals cannot hide earlier notes.
- Full core verification: `CODEX_SANDBOX_NETWORK_DISABLED=1 cargo-local test --quiet -p codex-core -- --test-threads=2` reported 1,983 passes, 16 ignored and 44 filtered across four suites, with 49 seconds of test execution. Existing network-guard early returns remain included in aggregate package totals.
- Formatting and lint: `just-local fmt`, scoped `just-local fix -p codex-core` and document-reference checks passed. Clippy retained existing warnings in unmodified code and made no source changes. All 38 v2 integration cases, including the three new response-tail cases, were included in the core run; the workspace-wide all-features suite was not run.
- Executed loopback regressions: inline empty final, inline whitespace-only final and pre-turn whitespace-only final each assert exactly four Responses requests (work, notes, compaction, follow-up), no capture suffix in compaction input and exact whole-message notes in follow-up, including text after the closing tag. Existing failed-compaction, Continue, steering, interruption, restart and local-fallback fixtures also use the trailing-reasoning/empty-final response shape.
- Replay/rejection coverage: completed notes and their response tail remain pending after rollout reconstruction; genuine later input remains in the source; nonempty assistant text, image content, input-text content and later tool calls remain rejected.
- Prompt guidance now requests final-answer placement, both delimiters and no outside text. This is deterministic transport/recovery coverage, not a new live-model evaluation or a claim that prompt wording prevents malformed output.

## End-to-end fixture measurements

Same fixture source/settings before and after compaction; all traffic used the normal Responses path. These are debug-build localhost measurements, not server latency or allocation benchmarks.

|Fixture|Initial request bytes|Compaction request bytes|Follow-up request bytes|Local compaction time|Retained images|
|---|---|---|---|---|---|
|Text history|462,271|462,397|284,051|76.793 ms|0|
|50 remote-image inputs|35,829|35,955|33,554|4.701 ms|34|

Byte counts describe decoded JSON request bodies, not compressed wire bytes. Image URLs are small on the wire but each consumes the retained-image budget. The three requests in each fixture were initial work, compaction and follow-up.

The baseline downshift recovery matrix sends 630,000 bytes of user text to a source model with a 200,000-token window, then selects a destination with a 100,000-token window and a 40,000-token automatic threshold. Notes report only ten tokens. Nine cases require recovery to use the source provider/model and ordinary follow-up to use the destination; each follow-up's approximate serialized input/instructions/tools cost remains below 40,000 tokens. Final overrides distinguish both models across resume without changing the existing session-instruction precedence.

Two further cases explicitly select a different recovery destination immediately after failure or after pause/restart. That destination has a 160,000-token window and an 80,000-token automatic threshold. Compaction still uses the original source provider/model/instructions; the follow-up uses the new destination and its final override, stays below 80,000 approximate serialized tokens, and records the destination in the adjacent replacement reference context.

## Authenticated deployment check

The client recorded in `4daffadda2` completed native HTTP acceptance against the reported ChatGPT deployment using `gpt-6-astra`, the built-in OpenAI provider and default request compression. The client ran in macOS 15.6.1 (24G90) with ordinary guest networking and no HTTP/TLS forwarder. Subsequent recovery refinements were verified with the guarded core package, loopback recovery matrix and real-recorder tests; authenticated acceptance was not repeated for those refinements.

|Scenario|Observed result|
|---|---|
|Ordinary generation|Passed in 2.71 seconds; pre-backport baseline `1c74f68455` passed in 2.39 seconds in the same VM.|
|Manual compaction and follow-up|One completed compaction; subsequent ordinary turn recalled the synthetic codeword.|
|Automatic compaction|An actual tool round with 8,000 inert reference lines and a 30,000-token automatic threshold triggered one notes-bearing compaction, then completed with the expected codeword.|
|Exact notes and persistence|The notes embedded in the replacement matched the emitted assistant notes byte-for-byte; no trigger appeared in replacement history.|
|Restart/resume|A new app-server process resumed the persisted automatic-compaction thread and recalled the codeword in a new turn.|

The retained acceptance script uses an 8 MiB stdout reader for large notifications, bounded turns, isolated state/workspace directories and copied authentication without refreshing the source credentials. Shell execution, undo, auto-rename, completion review and remote-model refresh are disabled. The VM's input was read-only and exported logs were writable; copied credentials were removed and the VM/configuration restored after verification. WebSocket lifecycle behavior is covered by loopback fixtures.

On the development host, both the baseline and backport timed out before ordinary generation. Independent standard-library TCP and Tokio/reqwest HTTP probes also timed out, including requests unrelated to compaction; stock curl succeeded. The same binaries succeeded in the clean VM. This isolates the remaining host connectivity problem from the backport; its host-specific cause is unresolved, and no speculative transport change was added.

## Limits

Local fixtures prove client routing and lifecycle behavior, not model summary quality, prompt-cache hits or server latency. Retention uses approximate estimates and cannot account for arbitrary new input after pre-turn compaction. Other provider families and model compatibility across opaque summaries remain outside this backport's eligibility policy.
