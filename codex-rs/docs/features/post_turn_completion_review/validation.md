# Completion-review validation

Use focused tests for the contract being changed. This page indexes regression locations and repeatable checks; it is not a record of prior runs.

## Test entry points

|Contract|Source and cases to preserve|
|---|---|
|Bounded episodes and tickets|[State tests](../../../core/src/completion_review_tests.rs): initial/latest slots, consumed steering, actual candidate CWD, exactness, attempt allowance, stale tickets and recovery selection.|
|Durable transitions and provenance|[Session tests](../../../core/src/session/completion_review_tests.rs): accepted/installing/body/consumed prefixes, identical amendments, prepared/started claims, fork barriers, clarification invalidation and directory ownership.|
|Packet and output semantics|[Review task tests](../../../core/src/tasks/post_turn_completion_review.rs): exact text/CDATA, unversioned framing, optional reference omission, mandatory overflow, custom-prompt contracts and ticket/candidate mismatch.|
|Main/reviewer integration|[HTTP suite](../../../core/tests/suite/completion_review.rs): exact initial/latest packets, finite automation, parent-owned reviewer recovery, unavailable coverage, real continue/compact/model-selection paths and cause-specific errors.|
|Request prefix|[WebSocket suite](../../../core/tests/suite/client_websockets.rs) and [prefix contract](request_prefix.md): repeated follow-up tool rounds keep `previous_response_id` chaining; HTTP replay preserves full prefixes.|
|Usage and replay|[History tests](../../../core/src/context_manager/history_tests.rs), HTTP suite and session tests: covered history, frame credit, reasoning inclusion, missing usage/failed streams, empty responses and local baselines.|
|Persistence and stop races|Session/HTTP suites and [recorder tests](../../../core/src/rollout/tests.rs): real buffered-write failures, pending/flushed deduplication, recorder replacement, finalization joins and resumed-owner failures before sampling.|
|Cross-feature behavior|[Review suite](../../../core/tests/suite/review.rs) and [downshift suite](../../../core/tests/suite/compact_remote_v2_downshift.rs): instruction/provider resolution, separate notes source and recovery destination.|

For cancellation/persistence changes, block a real recorder and invoke the actual pause/interrupt/shutdown handler. Read the durable result without a test-side flush, then compare replay with live state. Include a failed writer on a paused Started handoff, with and without compaction. For recovery selection, include steering that removes the advisory, restart under another default, and CWD/approval/effort-only updates that must not retarget the saved model.

## Focused commands

Run from the Rust workspace using the shared wrappers:

```bash
CODEX_SANDBOX_NETWORK_DISABLED=1 cargo-local test -p codex-core completion_review -- --test-threads=2
CODEX_SANDBOX_NETWORK_DISABLED=1 cargo-local test -p codex-core --test all responses_websocket_completion_review_followup_uses_incremental_create -- --test-threads=2
```

Choose additional affected state/recorder/compaction tests from the table. Use `skip_if_no_network_unless_localhost!` for mock HTTP/WebSocket fixtures and let the wrapper declare loopback capability. An early-return network guard, ignored test or filtered test is not executed transport coverage. Broaden testing only for affected integration boundaries.

## Live behavioral evaluation

[The evaluation runner](../../../scripts/evaluate-completion-review.py) exports production prompts, contracts and context rendering through an opt-in Rust test, then uses fresh native `codex-exec` sessions and real model responses. It inspects synthetic artifacts and checks their bytes before/after. Authentication copies and workspaces are temporary; copied refresh credentials are disabled.

```bash
cargo-local build -p codex-exec --bin codex-exec
python3 scripts/evaluate-completion-review.py "$CODEX_EXEC_BINARY" --model "$EVAL_MODEL" --review-model "$EVAL_REVIEW_MODEL"
```

Set `CODEX_EXEC_BINARY` to the rebuilt executable and choose model IDs available to the evaluation credential. The runner requires Python 3.11+ and the shared Cargo wrapper for fixture export. Its prompt overrides use `model_instructions_file`.

The synthetic cases cover a delta-only answer with a correct artifact, rejected findings with no edits, an incorrect initial reference, and cumulative corrections. Review the generated answer and executed commands as well as the boolean verdict: reading a file is not an executed parser or assertion check. This runner exercises production packet construction and semantic behavior, not the complete automatic-review/pause/restart lifecycle.

Report scripted transport results separately from live behavior. Neither a small synthetic set nor a clean review certifies general completeness, model/provider compatibility, cache performance or every verification claim.
