# Verification

## Deterministic Coverage

|Suite|Assertions|
|---|---|
|[State tests](../../../core/src/completion_review_tests.rs)|Ten same-request completions; frozen scope/reference; genuine prior user rounds; consumed steering; accepted-but-unconsumed input; stale tickets; allowance/replay; branch generations and unknown metadata.|
|[Session tests](../../../core/src/session/completion_review_tests.rs)|Authoritative final text; no progress-as-completion inference; late input; single handoff claim; compaction preservation; prepared versus started recovery; rollback/fork invalidation; actual task-stop persistence with a real recorder and no test-side flush.|
|[Review task tests](../../../core/src/tasks/post_turn_completion_review.rs)|Exact CDATA data, deduplication, optional-reference-first budgets, mandatory overflow, nontext/legacy coverage, invalid evaluations and host-owned prompt contracts.|
|[HTTP integration](../../../core/tests/suite/completion_review.rs)|One real request and three reviews; exact initial/latest packets without prior verdicts; custom-prompt framing; cap/manual behavior; invalid output; restart without persisted completion events; running-review replacement; prepared/started crash-boundary fixtures and no duplicate advice.|
|[Existing review integration](../../../core/tests/suite/review.rs)|Real user rounds, read-only configuration, provider selection, host/project instruction profiles, review pause/continue and generic `/review` behavior.|
|[Recorder tests](../../../core/src/rollout/tests.rs)|Continuation cleanup retains host metadata while removing dangling work and interrupt markers.|

Mock-only tests use the repository's localhost-aware guard. Test workspaces are owned by retained temporary homes so project docs and restart CWDs survive builder teardown.

## Commands

Run through `cargo-local` and `just-local`, with bounded build/test parallelism and `CODEX_SANDBOX_NETWORK_DISABLED=1` for tests:

```bash
just-local fmt
CODEX_SANDBOX_NETWORK_DISABLED=1 cargo-local test --quiet -p codex-core 'review::' -- --test-threads=2
CODEX_SANDBOX_NETWORK_DISABLED=1 cargo-local test --quiet -p codex-core -- --test-threads=2
CODEX_SANDBOX_NETWORK_DISABLED=1 cargo-local test --quiet -p codex-protocol -p codex-state -- --test-threads=2
just-local fix -p codex-core -p codex-protocol -p codex-state
cargo-local check --quiet -p codex-tui -p codex-exec -p codex-app-server
```

Workspace-wide testing remains a separately authorized VM workload. The repository wrapper's exclusions are not executed coverage.

## Recorded Verification

|Check|Result|
|---|---|
|Core, protocol and state crate suites|2,017 passed; 15 ignored; 44 filtered; nine suites; two test threads.|
|Completion/review-focused tests|69 passed across four suites, including actual task-stop installation cancellation and replay.|
|Mock HTTP completion-review integration|Six tests passed; 0.64 seconds test execution, 1.02 seconds command wall time on the validation host.|
|End-to-end request shape|One genuine user request, three reviewers and two main follow-ups produced six Responses requests. Later reviewers received the stable initial reference and exact latest candidate, never preceding verdicts.|
|Scoped Clippy fix and consumer compilation|Passed for core/protocol/state and TUI/exec/app-server consumers; existing warnings remain in unrelated code.|
|Documentation|Three feature documents; 14 relative links; Markdown fences, unpadded tables and local-path redaction checked.|

The HTTP timings describe local mock transport and harness overhead, not live-model latency. The loopback fixtures executed actual transport; early-return network guards elsewhere in the broader suites are not additional transport coverage. No workspace-wide all-feature run or live-model semantic evaluation was performed.

## Evidence Boundary

HTTP fixtures establish request contents, lifecycle/event ordering, exact candidate identity, durable recovery and fixed attempt counts. They do not establish that a live model always produces a complete answer.

Behavioral evaluations should separately assess: a delta-only answer with correct artifacts; rejected findings requiring an unchanged full answer; stale initial claims contradicted by current evidence; and cumulative X/Y corrections across successive replacement answers. No live-model semantic-completeness guarantee is inferred from scripted JSON verdicts.
