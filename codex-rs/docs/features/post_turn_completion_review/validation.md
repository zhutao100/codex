# Verification

## Deterministic Coverage

|Suite|Assertions|
|---|---|
|[State tests](../../../core/src/completion_review_tests.rs)|Ten same-request completions; frozen scope/reference; replacement candidates record their actual task directory through replay; genuine prior user rounds; consumed steering; rejection of consumption before message installation; stale tickets; allowance/replay; branch generations and unknown metadata.|
|[Session tests](../../../core/src/session/completion_review_tests.rs)|Authoritative final text; late input; accepted/installing/recorded/consumed crash boundaries; distinct identical amendments; acknowledgement before main startup; single handoff claim; prepared versus started recovery; rollback/fork invalidation; encrypted compaction across resume/fork with missing, version 1, unknown or trusted metadata; discarded empty/encrypted/summary histories; manual snapshot/ticket reservation joins blocked finalization; model changes retarget only regular work and durably preserve frozen snapshots; CWD mismatch preserves pending review-derived work without restricting ordinary continuation; input-installation cancellation and actual pause/interrupt/shutdown use a real recorder and replay without a test-side flush.|
|[Review task tests](../../../core/src/tasks/post_turn_completion_review.rs)|Exact CDATA data, deduplication, optional-reference-first budgets, mandatory overflow, nontext/legacy coverage, invalid evaluations, host-owned prompt contracts and rejection of a current ticket paired with a different candidate.|
|[HTTP integration](../../../core/tests/suite/completion_review.rs)|One real request and three reviews; exact initial/latest packets without prior verdicts; custom-prompt framing; cap/manual behavior; invalid output; restart without persisted completion events; encrypted legacy/version 1 resume followed by referential input stays unavailable without a reviewer POST; running-review replacement; prepared/started crash-boundary fixtures; accepted or partially installed amendments reach the model and next reviewer once; clarification answers explicitly prevent incomplete full-coverage claims; actual budget stop followed by compaction or a larger model; paused/interrupted main work survives standalone compaction and shutdown/restart; recovered outbound requests retain exact mandatory context and the selected destination.|
|Directory ownership HTTP cases|Paused and budget-stopped follow-ups reject a CWD change before task startup, both live and after restart. No new model POST or snapshot change occurs on rejection. Returning to the original CWD resumes the saved work, and a subsequent reviewer receives that project's instructions and the exact replacement candidate.|
|[Model-downshift integration](../../../core/tests/suite/compact_remote_v2_downshift.rs)|Pending notes keep their source provider, model and effective instructions while an explicit recovery-time override changes the ordinary follow-up and replacement budget to a different destination, immediately or after pause/restart.|
|[Existing review integration](../../../core/tests/suite/review.rs)|Real user rounds, read-only configuration, provider selection, host/project instruction profiles, review pause/continue and generic `/review` behavior.|
|[Recorder tests](../../../core/src/rollout/tests.rs)|Continuation cleanup retains host metadata while removing dangling work and interrupt markers.|

Mock-only tests use the repository's localhost-aware guard. Test workspaces are owned by retained temporary homes so project docs and restart CWDs survive builder teardown.

## Commands

Run through `cargo-local` and `just-local`, with bounded build/test parallelism and `CODEX_SANDBOX_NETWORK_DISABLED=1` for tests:

```bash
export CARGO_BUILD_JOBS=2
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
|Core, protocol and state crate suites|2,042 passed; 16 ignored; 44 filtered; nine suites; two test threads.|
|Completion/review-focused tests|92 passed and one opt-in exporter ignored across four suites, including real finalization/stop/reservation joins, unknown/trusted provenance, budget recovery, directory ownership, explicit destination selection and replay.|
|Mock HTTP completion-review integration|Sixteen tests passed; 2.01 seconds test execution, 2.42 seconds command wall time on the validation host.|
|Compaction-focused tests|127 reported passes across four suites; eleven source/destination recovery cases include explicit model changes before Continue, with acknowledged input and notes source preserved. Legacy early-return guards are not additional transport coverage.|
|End-to-end request shape|One genuine user request, three reviewers and two main follow-ups produced six Responses requests. Later reviewers received the stable initial reference and exact latest candidate, never preceding verdicts.|
|Scoped Clippy fix and consumer compilation|Passed for changed core code and TUI/exec/app-server consumers; existing warnings remain in unrelated code.|
|Live behavioral set|All four expected review verdicts matched in two runs, with two generated replacement answers per run and unchanged artifacts; manual assessment and limits below.|
|Documentation|Completion-review and compaction documents; Markdown fences, relative links, unpadded tables and local-path redaction checked.|

The HTTP timings describe local mock transport and harness overhead, not live-model latency. The loopback fixtures executed actual transport; early-return network guards elsewhere in the broader suites are not additional transport coverage. No workspace-wide all-feature run was performed.

Reservation, provenance, recovery and directory-ownership corrections reran deterministic and transport coverage above. They do not change prompts or answer contracts; the live behavioral results below remain the earlier evaluation, not a new live run of those lifecycle corrections.

## Live Behavioral Evaluation

[Evaluation runner](../../../scripts/evaluate-completion-review.py) exports the production reviewer prompt, host contracts, advisory framing and exact context renderer through an opt-in Rust test. It then uses fresh native `codex-exec` sessions, read-only workspaces and real model responses. Answers and verdicts are not scripted. Each reviewer executes actual artifact-inspection commands; artifact bytes are checked before and after. Authentication/configuration copies are temporary, refresh is disabled in the copied credential, and host configuration is not modified.

```bash
CARGO_BUILD_JOBS=2 cargo-local build --quiet -p codex-exec --bin codex-exec
python3 scripts/evaluate-completion-review.py "$CODEX_EXEC_BINARY" --model "$EVAL_MODEL" --review-model "$EVAL_REVIEW_MODEL"
```

`CODEX_EXEC_BINARY` is the rebuilt executable in the target reported by `cargo-local metadata`. Choose model identifiers from the live catalog for the evaluation credential, not merely the bundled presets. Native instruction overrides use the supported `model_instructions_file` configuration field, not the programmatic-only `Config.base_instructions` field.

|Case|Observed Result In Both Runs|
|---|---|
|Delta-only answer, correct artifact|Reviewer requested a replacement because "Fixed X." omitted the complete values and verification; it did not mistake the correct artifact for missing implementation.|
|Rejected findings, no edits|Main rejected the incompatible x=99 suggestion, retained x=2/y=3 in a complete replacement, performed inspection, and changed no artifact; reviewer advised no further action.|
|Contradictory initial reference|Reviewer accepted the complete current x=2/y=3 candidate rather than restoring historical x=99/y=98 claims.|
|Cumulative corrections|Main retained the earlier x=2 correction while reporting y=3, inspection and no edits in its latest standalone answer; reviewer advised no further action.|

The live catalog exposed `codex-auto-review`, used for both roles in separate fresh sessions. Each complete run made six model sessions: four reviewers and two main follow-ups. Run wall times, including fixture export and file inspection, were 30.26 and 26.67 seconds. The configured `gpt-6.1-sol` was rejected for the evaluation credential, so no coverage of that model or a provider/model matrix is claimed.

## Evidence Boundary

HTTP fixtures establish request contents, lifecycle/event ordering, exact candidate identity, durable recovery and fixed attempt counts. The live set separately exercises the four requested behavioral scenarios using production prompt/packet construction, not a live automatic-review/restart workflow.

Manual assessment confirmed that the generated replacements retained both requested values and stood alone. Verification wording still needs judgment: one rejected-findings sample said "reading and parsing the JSON" after executing only `cat result.json`, and the reviewer accepted it. This is evidence of direct inspection, not an executed parser/assertion check. Successful verdicts do not certify every verification claim.

This small synthetic set is not a universal semantic-completeness guarantee, a full model evaluation, or a substitute for the transport/lifecycle regressions.
