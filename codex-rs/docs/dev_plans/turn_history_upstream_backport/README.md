# Turn and History Backport Plan

This directory documents the turn/history behavior in this branch, audits the corresponding upstream branch behavior, and proposes a minimal backport series.

The proposal does **not** transplant the upstream session, protocol, lifecycle, compaction-window, or multi-agent architecture. It ports the behavioral invariants that can be expressed with this branch's existing `RolloutItem`, `/pause`, `/continue`, preserved-work-notes, and compaction machinery.

## Executive decision

|Area|This branch|Upstream branch|Decision|
|---|---|---|---|
|Append-only multi-round prompt construction|Present|Present|Keep|
|Prompt-copy call/output normalization|Present|Present, with more protocol variants|Keep branch-local implementation|
|Modality-aware image stripping|Present|Present|Keep|
|Post-model-tail token accounting|Present|Present|Keep|
|Compaction replacement history|Present|Present|Keep|
|Previous-larger-model downshift compaction|Present|Present|Keep; do not re-port|
|Fresh-input-before-steer ordering|Present|Present|Keep; do not re-port|
|Preserved work notes excluded from user-turn boundaries|Present|Not the same feature upstream|Keep|
|Durable turn-context baseline|Present|Present|Fix replay/commit semantics rather than reintroducing it|
|Rollout rollback keeps history and metadata on the same surviving turns|Not reliable|Fixed upstream|Backport now|
|Bare `TurnContext` ignored unless attached to a real user turn or compacted replacement|Not reliable|Fixed upstream|Backport now|
|Legacy compaction avoids injecting resume-time context into historical history|Not reliable|Fixed upstream|Backport now|
|`previous_turn_settings` committed only at a durable user boundary and preserved through compaction|Not reliable|Fixed upstream|Backport now|
|Invalid-image recovery stops at a real user-turn boundary|Not reliable|Fixed upstream|Backport now|
|Compaction-compatibility hash|Absent|Present|Conditional follow-up|
|Body-after-prefix compaction budget|Absent|Present|Defer; broad dependency surface|
|Persisted turn lifecycle IDs and reverse/lazy reconstruction|Absent|Present|Defer; not required for the minimal fix|

## Recommended patch series

1. Add regression tests for replay, compaction, rollback, continuation, and invalid-image boundaries.
2. Split the live reference-context baseline from `previous_turn_settings` commitment.
3. Replace the independent `context_stack` replay with a branch-local pending-context and replay-epoch state machine.
4. Rebuild legacy compactions without current resume-time initial context.
5. Make invalid-image recovery use `is_user_turn_boundary`.
6. Optionally port `comp_hash` only when the model metadata source used by this branch supplies it.

## Documents

- [`current_workflow.md`](current_workflow.md): current turn loop, history representation, exact preservation/drop rules, and cache behavior.
- [`upstream_audit.md`](upstream_audit.md): classified comparison with the upstream branch.
- [`problem_statement.md`](problem_statement.md): concrete defects and failure scenarios.
- [`design_proposal.md`](design_proposal.md): minimal target semantics and replay state machine.
- [`implementation_plan.md`](implementation_plan.md): patch order, file scope, and acceptance criteria.
- [`test_matrix.md`](test_matrix.md): regression and compatibility cases.
- [`conditional_features.md`](conditional_features.md): deliberately separated upstream features and their minimal prerequisites.

## Primary code paths

This branch:

- `core/src/session/turn.rs`
- `core/src/context_manager/history.rs`
- `core/src/context_manager/normalize.rs`
- `core/src/session/rollout_reconstruction.rs`
- `core/src/session/mod.rs`
- `core/src/state/session.rs`
- `core/src/compact.rs`
- `core/src/compact_remote.rs`
- `core/src/rollout/policy.rs`
- `core/src/client.rs`
- `core/src/event_mapping.rs`
- `docs/dev_plans/pause_continue`

Upstream behavior was inspected in the corresponding paths, especially `core/src/session/rollout_reconstruction.rs` and `core/src/session/rollout_reconstruction_tests.rs`.

## External reference

The prompt-cache discussion uses the agent-loop description in [Unrolling the Codex agent loop](https://openai.com/index/unrolling-the-codex-agent-loop/): later requests carry prior conversation items, and keeping the old prompt as an exact prefix of the new prompt permits prefix caching.
