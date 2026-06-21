# Turn History and Upstream Backport Plan

## Scope

This plan audits the agent-loop and conversation-history behavior in this branch against the upstream branch, with emphasis on:

- multi-round history carryover within a turn and across turns;
- exact versus lossy preservation during normal sampling, compaction, rollback, resume, fork, pause, and continuation;
- prefix-cache and WebSocket continuation consequences;
- a minimal backport that fixes replay correctness without importing the upstream architecture wholesale.

The primary code paths are:

- `core/src/session/turn.rs`
- `core/src/context_manager/history.rs`
- `core/src/context_manager/normalize.rs`
- `core/src/session/rollout_reconstruction.rs`
- `core/src/session/mod.rs`
- `core/src/compact.rs`
- `core/src/compact_remote.rs`
- `core/src/rollout/policy.rs`
- `core/src/client.rs`

## Executive conclusion

The normal live turn path is already substantially correct. In particular, this branch already:

- records the real user `ResponseItem` before committing `previous_turn_settings`;
- keeps the reference-context baseline separate from `previous_turn_settings`;
- preserves `previous_turn_settings` when compaction clears the reference baseline;
- uses `is_user_turn_boundary` for invalid-image recovery;
- sends fresh explicit input before draining queued steering input;
- creates each sampling request from the current full history;
- persists exact replacement history for new local and remote compactions.

The high-value gap is rollout replay. `core/src/session/rollout_reconstruction.rs` independently counts every `TurnContext` and every real user boundary, then applies the same rollback count to both populations. Those populations are not equivalent. This can hydrate metadata from an incomplete or non-user task, select the wrong metadata after rollback, lose the previous model across manual compaction, and infer a false `/continue` checkpoint from a compaction summary.

## Selected minimal backport

|Area|Decision|Rationale|
|---|---|---|
|Replay metadata checkpoints|Backport now|Align metadata with the same real user boundaries used by history rollback.|
|Compaction replay epochs|Backport now|Preserve previous-turn settings, explicitly clear/re-establish the reference baseline, and avoid pretending compacted history maps one-to-one to original turns.|
|Legacy compaction reconstruction|Backport now|Stop injecting resume-time context at a historical compaction point.|
|Compaction-aware continuation inference|Backport now|Avoid treating a standalone compaction summary as an interrupted regular turn.|
|Persisted lifecycle IDs and reverse replay|Defer|Upstream solution is broader than required and conflicts with branch-local task semantics unless ported as a coherent subsystem.|
|`comp_hash` compatibility|Conditional|Useful only when this branch's model metadata source supplies a stable non-empty hash.|
|History windows, body-after-prefix budgets, remote compaction v2|Defer|Large dependency surface; not required for the confirmed defects.|

No mandatory protocol, rollout-schema, or public event change is proposed.

## Correctness target

Resume, fork, and rollback must reconstruct a coherent pair:

```text
(model-visible history, metadata describing that same surviving history)
```

The pair has two distinct metadata components:

- `reference_context_item`: baseline used to decide which context/settings updates must be appended next;
- `previous_turn_settings`: settings of the newest committed real user turn, used by previous-model compaction and related transition logic.

Compaction may intentionally clear the first while preserving the second.

## Patch series

1. Replace stale replay expectations with regression cases that encode live-state invariants.
2. Introduce branch-local pending-context and real-user metadata checkpoints in `core/src/session/rollout_reconstruction.rs`.
3. Treat each persisted compaction replacement as an opaque replay epoch, preserving previous-turn settings while clearing or re-establishing the reference baseline.
4. Rebuild legacy compactions with no resume-time initial context.
5. Derive pending continuation from incomplete history plus compaction provenance, using committed previous-turn settings for its model.
6. Run focused replay, rollback, compaction, pause/continue, and prompt-cache tests.

## Explicit non-work

The mandatory patch must not:

- change regular `run_turn_inner` sequencing;
- reintroduce a second commitment of `previous_turn_settings` during context recording;
- change `ContextManager::replace_last_turn_images`;
- persist `TurnStarted`, `TurnComplete`, `TurnPaused`, or `TurnContinued` solely for this fix;
- import upstream history windows, realtime state, inter-agent protocol, or response-item expansion;
- alter the branch-local preserved-work-notes or `GhostSnapshot` formats.

## Documents

- [`current_workflow.md`](current_workflow.md): live turn flow, raw history, prompt projection, persistence, preservation, rollback, and cache behavior.
- [`upstream_audit.md`](upstream_audit.md): classified comparison with the upstream branch.
- [`problem_statement.md`](problem_statement.md): confirmed defects, non-defects, and failure traces.
- [`design_proposal.md`](design_proposal.md): target invariants and minimal replay state machine.
- [`implementation_plan.md`](implementation_plan.md): file-level patch sequence and acceptance gates.
- [`test_matrix.md`](test_matrix.md): unit, integration, compatibility, and cache tests.
- [`conditional_features.md`](conditional_features.md): deferred upstream features and their minimal prerequisites.

## External reference

The cache analysis follows [Unrolling the Codex agent loop](https://openai.com/index/unrolling-the-codex-agent-loop/): later requests carry prior conversation items, and prompt caching benefits when the prior request remains an exact prefix of the next request.
