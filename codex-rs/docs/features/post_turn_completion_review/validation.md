# Verification

## Deterministic Coverage

|Suite|Assertions|
|---|---|
|[State tests](../../../core/src/completion_review_tests.rs)|Ten same-request completions; frozen scope/reference; replacement candidates record their actual task directory through replay; genuine prior user rounds; consumed steering; recovery selection survives steering but clears on completion, a new root request or fork; rejection of consumption before message installation; stale tickets; allowance/replay; branch generations and unknown metadata.|
|[Session tests](../../../core/src/session/completion_review_tests.rs)|Authoritative final text; late input; accepted/installing/recorded/consumed crash boundaries; distinct identical amendments; acknowledgement before main startup; single handoff claim; prepared versus started recovery; metadata without an independent recovery model retains its Started-handoff fallback; rollback/fork invalidation; fork recovery skips excluded-next-user metadata but stops at exactness loss in the same generation; encrypted compaction across resume/fork with missing, unsupported or trusted metadata; discarded empty/encrypted/summary histories; manual reservation distinguishes missing candidates, pending input, unavailable provenance and active tasks while still joining blocked finalization; explicit model changes retarget only regular work, before or after steering, and durably preserve exact snapshots; CWD mismatch preserves pending review-derived work without restricting ordinary continuation; input-installation cancellation and actual pause/interrupt/shutdown use a real recorder and replay without a test-side flush.|
|[Review task tests](../../../core/src/tasks/post_turn_completion_review.rs)|Unversioned model-facing framing; exact CDATA data, deduplication, optional-reference-first budgets, mandatory overflow, nontext/legacy coverage, invalid evaluations, enforced host contracts and rejection of a current ticket paired with a different candidate.|
|[HTTP integration](../../../core/tests/suite/completion_review.rs)|One real request and three reviews; exact initial/latest packets without prior verdicts; custom-prompt framing; cap/manual behavior; invalid output; restart without persisted completion events; encrypted histories with missing or unsupported metadata followed by referential input stay unavailable without a reviewer POST and retain their actionable provenance diagnostic after another restart; running-review replacement; prepared/started crash-boundary fixtures; accepted or partially installed amendments reach the model and next reviewer once; clarification answers explicitly prevent incomplete full-coverage claims; actual budget stop followed by compaction or a larger model; paused/interrupted main work survives standalone compaction and shutdown/restart; recovered outbound requests retain exact mandatory context and the selected destination. CWD-only, approval-only and effort-only overrides through the real submission handler preserve that destination after restart with a smaller configured default.|
|Directory ownership HTTP cases|Paused and budget-stopped follow-ups reject a CWD change before task startup, both live and after restart. No new model POST or snapshot change occurs on rejection. Returning to the original CWD resumes the saved work, and a subsequent reviewer receives that project's instructions and the exact replacement candidate.|
|Amendment crash HTTP case|A real started handoff accepts steering and records its Installing/body prefix. Restart without the subsequent Recorded snapshot rejects a different directory, resumes in the original directory, sends the amendment exactly once and reviews the complete amended scope.|
|Clarification/fork HTTP cases|A human clarification during initial main work or a review-derived follow-up reaches the main model and completes normally. Full-history forks through `ThreadManager::fork_thread` retain unavailable coverage, cannot restore an older pre-clarification candidate, and reject manual review without another reviewer request.|
|Steered recovery HTTP cases|A real review-derived follow-up accepts steering, then pauses or stops on its payload budget with no advisory handoff remaining. A larger-model selection survives shutdown and restart under the smaller default. Continue sends that selected model, a deep-equal complete amended request frame and the replacement-answer contract; the next reviewer receives the original scope, exact amendment and completed replacement.|
|Failed persistence handlers|A real durable rollout prefix is reopened through a read-only writer. Failed reservation and claim flushes emit the persistence warning without reviewer/main startup or modification of that prefix; manual reservation also reports the storage failure rather than a missing candidate. Failed owner writes on actual paused Started handoffs, with and without installed encrypted compaction, stop before TurnStarted or sampling and preserve history, continuation ownership and the durable prefix. After writer repair, missing mandatory context rejects retry before task registration or persistence; replay of the untouched prefix restores the original exact snapshots.|
|[Model-downshift integration](../../../core/tests/suite/compact_remote_v2_downshift.rs)|Pending notes keep their source provider, model and effective instructions while an explicit recovery-time override changes the ordinary follow-up and replacement budget to a different destination, immediately or after pause/restart.|
|[Existing review integration](../../../core/tests/suite/review.rs)|Real user rounds, read-only configuration, provider selection, host/project instruction profiles, review pause/continue and generic `/review` behavior.|
|Reviewer tracking policy|A real recorder and shutdown/replay preserve the exact ordinary request and evaluation without input revisions, candidate capture or review metadata. Scripted HTTP covers both review entry points; a positive post-turn verdict still claims the parent's handoff and produces a complete replacement, while the reviewer's rollout contains its exact packet and evaluation but no completion-review state.|
|Checkpoint coalescing|Replay each actual emitted input prefix: accepted with owner, installing before body, body before consumption, and consumed. Every prefix recovers the same complete request frame and one canonical body. Failed combined candidate/automatic-ticket persistence leaves no eligible ticket and preserves the prior file. A two-request HTTP fixture verifies all five clean-episode checkpoints through real shutdown, with no reviewer-owned snapshots.|
|[Recorder tests](../../../core/src/rollout/tests.rs)|Continuation cleanup retains host metadata while removing dangling work and interrupt markers. Pending and acknowledged snapshot deduplication preserve `A -> B -> A`; recorder replacement resets the baseline, including at the same path. Unchanged snapshots still flush ordinary transcript writes. Read-only files and a closed socket peer exercise real buffered-write failures, including after a cached snapshot; flush/shutdown acknowledgements propagate those errors.|

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
|Core crate suite|1,992 passed; 16 ignored; 44 filtered; four suites; two test threads; 48.15 seconds test execution. Includes cause-specific manual reservation and unavailable-provenance restart coverage.|
|Completion/review-focused tests|85 passed; one opt-in exporter ignored. Coverage includes snapshot deduplication, coalesced input-prefix recovery, failed combined capture, real finalization/stop/reservation joins, unknown/trusted provenance, budget recovery, amendment directory ownership, failed resumed-owner persistence, repaired-writer rejection of unavailable snapshots, explicit destination selection before or after steering, non-model overrides, clarification/fork barriers and replay.|
|Mock HTTP completion-review integration|Passed in the core suite. The dedicated five-checkpoint case also passed with successful-test output enabled, without an early-return network skip: 0.16 seconds test execution and 0.51 seconds command wall time.|
|Compaction-focused tests|127 reported passes across four suites; eleven source/destination recovery cases include explicit model changes before Continue, with acknowledged input and notes source preserved. Legacy early-return guards are not additional transport coverage.|
|Recorder tests|Passed in the core suite, including pending/flushed deduplication, writer replacement and error-bearing acknowledgements. The cached-snapshot/closed-peer fault also passed with successful-test output enabled, without an early-return network skip.|
|End-to-end request shape|One genuine user request, three reviewers and two main follow-ups produced six Responses requests. Later reviewers received the stable initial reference and exact latest candidate, never preceding verdicts.|
|Reviewer persistence end-to-end|Three scripted Responses requests completed the main answer, positive reviewer verdict and replacement answer. The child retained its exact packet and evaluation with zero completion-review state records. Parent request scope, initial reference and replacement candidate survived shutdown; dedicated recorder shutdown/replay also passed without a test-side flush.|
|Clean automatic-review footprint|Two actual Responses POSTs produced five parent recovery states through shutdown: accepted input with owner, installing, consumed, completed candidate with automatic ticket, and finished review. No standalone Recorded state, duplicate owner/cleanup snapshots or reviewer-owned state records. Five rather than nine review-state records; ordinary transcript records remain unchanged.|
|Evaluation fixture export|Four production cases exported as JSON; parsed packet roots have no format attributes, prompts/contracts expose no storage versions, and no live-model calls are involved.|
|Scoped Clippy fix and consumer compilation|Passed for changed core code and TUI/exec/app-server consumers; existing warnings remain in unrelated code.|
|Live behavioral set|All four expected review verdicts matched in two runs, with two generated replacement answers per run and unchanged artifacts; manual assessment and limits below.|
|Documentation|Completion-review and compaction documents; Markdown fences, relative links, unpadded tables and local-path redaction checked.|

The broader HTTP suite includes a deliberate five-second mocked response delay used to install steering before a budget stop; the dedicated five-checkpoint case does not. These timings do not measure live-model latency. The loopback fixtures executed actual transport; early-return network guards elsewhere in the broader suites are not additional transport coverage. No workspace-wide all-feature run was performed.

Snapshot suppression and checkpoint coalescing reran the deterministic and transport coverage above. The live behavioral results below remain a separate earlier evaluation, not a new live run of these changes.

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
