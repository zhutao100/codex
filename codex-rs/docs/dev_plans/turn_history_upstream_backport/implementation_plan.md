# Implementation Plan

## Patch 0: Freeze target behavior with tests

Add tests with the production patches so stale documentation cannot drive accidental regressions. The tests should fail against the pre-fix implementation, but do not land a deliberately failing commit in this drop-in branch.

### Files

- `core/src/session/tests.rs` for reconstruction coverage; split a dedicated module only if the test file becomes unwieldy
- `core/src/context_manager/history_tests.rs`
- pause/continue tests near the existing continuation coverage

### Required regression tests

1. Bare trailing `TurnContext` does not hydrate reference or previous settings.
2. Rollback after a non-user compaction context record restores history and metadata from the same surviving user turn.
3. Legacy compaction does not include `build_initial_context` from the resume turn.
4. `DoNotInject` compacted replacement clears reference but preserves previous settings.
5. A contextual user message does not stop invalid-image sanitization.
6. Continuation does not create a new replay checkpoint.
7. Existing reconstruction assertions that treat resume-time context as legacy-compaction history, or derive previous settings from an adjacent compacted baseline, are updated to the new invariants rather than duplicated.

## Patch 1: Fix the invalid-image boundary

### File

- `core/src/context_manager/history.rs`

### Change

Use `is_user_turn_boundary` in `replace_last_turn_images`.

### Tests

- offending tool image followed by a contextual user item is replaced;
- offending tool image before a real user boundary is not crossed;
- user-supplied images remain untouched.

This patch is independent and can land first.

## Patch 2: Split reference baseline from previous-turn commitment

### Files

- `core/src/state/session.rs`
- `core/src/session/mod.rs`
- `core/src/session/turn.rs`
- compaction tests in `core/src/session/tests.rs` or `core/src/compact.rs`

### Changes

1. Add or expose a narrow setter for `previous_turn_settings`.
2. Remove previous-settings assignment from `record_context_updates_and_set_reference_context_item`.
3. Commit previous settings immediately after `record_user_prompt_and_emit_turn_item` succeeds on the regular explicit-input path.
4. Leave the continuation path unchanged; it has no new explicit input.
5. Stop deriving previous settings in `replace_compacted_history`.
6. Split `clear_turn_context_baseline` semantics so callers can clear only the reference baseline or all reconstructed metadata.

### Invariants to assert

- cancellation before user recording does not advance previous settings;
- normal user recording does advance them;
- compacting with `reference_context_item = None` preserves them;
- a compact task without a user boundary never advances them.

## Patch 3: Replace `context_stack` with replay epochs

### File

- `core/src/session/rollout_reconstruction.rs`

### Suggested private helpers

```rust
fn commit_user_boundary(state: &mut ReplayState);
fn apply_compaction(state: &mut ReplayState, ...);
fn attach_compaction_reference(state: &mut ReplayState, item: &TurnContextItem);
fn apply_rollback(state: &mut ReplayState, num_turns: u32);
fn reconstructed_continuation_model(state: &ReplayState) -> Option<String>;
```

Keep replay state private to this module. Do not add protocol fields.

### Processing order details

For each item:

1. Determine whether it is the directly adjacent post-compaction `TurnContext` before clearing the adjacency flag.
2. Materialize history using the existing truncation policy.
3. Commit metadata only on `is_user_turn_boundary_response_item`.
4. Ignore the duplicate `EventMsg::UserMessage` for turn counting.
5. Discard `pending_context` at every rollback and at `Compacted`; also clear pending continuation and the post-compaction attachment flag on rollback.
6. Keep existing normal-output persistence/retruncation and replacement-checkpoint installation semantics unchanged.

### Conservative fallback

If rollback crosses the current epoch base, set:

```rust
reference_context_item = None;
previous_turn_settings = None;
pending_context = None;
epoch = ReplayEpoch::from_base(ReplayMetadata::default());
```

Resetting the epoch is required so a later real user turn and a second rollback operate on the already-truncated state rather than stale pre-rollback checkpoints. Do not guess from the last remaining user-role item in replacement history.

## Patch 4: Correct legacy compaction reconstruction

### File

- `core/src/session/rollout_reconstruction.rs`

### Change

Use an empty initial context when rebuilding a `CompactedItem` with no `replacement_history`.

### Result

- historical content is stable across resume settings;
- the reconstructed reference baseline is cleared;
- the next regular turn appends canonical current context.

### Compatibility test

Reconstruct the same legacy rollout under two different resume-time working directories/policies and assert that reconstructed history is identical before the next turn injects context.

## Patch 5: Integrate rollback and resume paths

### Files

- `core/src/session/mod.rs`
- `core/src/session/handlers.rs`
- `core/src/session/rollout_reconstruction.rs`

### Checks

1. `record_initial_history` installs reconstructed history, reference, previous settings, pending continuation, and token state together.
2. `reconstruct_for_thread_rollback` uses the same replay path before persisting/emitting `ThreadRolledBack`.
3. The no-rollout fallback truncates history and clears all uncertain metadata.
4. `initial_context_seeded` follows reference-baseline presence, not previous-settings presence.
5. Pending continuation selects its model only from committed metadata.

## Patch 6: Cache-shape and continuation regression checks

No cache protocol change is proposed, but tests should verify the backport does not alter request construction.

### Files

- `core/src/client.rs` tests
- turn/continuation integration tests

### Assertions

- a normal second sampling request remains previous input + returned model items + new tool/pending items;
- unchanged non-input properties still permit incremental WebSocket requests;
- compaction still clears WebSocket continuation;
- `/continue` cleanup still removes the incomplete tail before prompt construction;
- no replay metadata item becomes a model `ResponseItem`;
- same-policy resume reproduces live output truncation, while changed-policy behavior remains explicitly covered rather than accidentally altered;
- replacement-history checkpoints remain exempt from item-by-item retruncation.

## Patch 7: Optional `comp_hash` feature

Implement only after the mandatory series is stable and only if the active model metadata endpoint supplies meaningful hashes. See `conditional_features.md`.

## File-level change summary

|File|Mandatory change|
|---|---|
|`core/src/context_manager/history.rs`|Real-user boundary for invalid-image recovery|
|`core/src/state/session.rs`|Clarify/split metadata setters if needed|
|`core/src/session/turn.rs`|Commit previous settings after user input|
|`core/src/session/mod.rs`|Preserve previous settings during compaction; install reconstruction coherently|
|`core/src/session/rollout_reconstruction.rs`|Pending context, epochs, checkpoints, conservative rollback, legacy compaction fix|
|`core/src/session/handlers.rs`|Use coherent reconstruction/fallback metadata clearing|
|Tests near the files above|Regression and old-rollout compatibility coverage|

## Verification commands

Run from the repository root. Use the branch-local Cargo wrapper and the network-disabled test environment:

```bash
CODEX_SANDBOX_NETWORK_DISABLED=1 scripts/cargo-local test -p codex-core context_manager
CODEX_SANDBOX_NETWORK_DISABLED=1 scripts/cargo-local test -p codex-core rollout_reconstruction
CODEX_SANDBOX_NETWORK_DISABLED=1 scripts/cargo-local test -p codex-core pause
CODEX_SANDBOX_NETWORK_DISABLED=1 scripts/cargo-local test -p codex-core continue
CODEX_SANDBOX_NETWORK_DISABLED=1 scripts/cargo-local test -p codex-core thread_rollback
CODEX_SANDBOX_NETWORK_DISABLED=1 scripts/cargo-local test -p codex-core compact
CODEX_SANDBOX_NETWORK_DISABLED=1 scripts/cargo-local test -p codex-core websocket
CODEX_SANDBOX_NETWORK_DISABLED=1 scripts/cargo-local clippy -p codex-core --all-targets --all-features -- -D warnings
just fmt
```

Use the exact package/test filters available in this branch if individual names differ. The final mandatory verification is:

```bash
CODEX_SANDBOX_NETWORK_DISABLED=1 scripts/cargo-local test -p codex-core
```

## Review gates

- No mandatory patch changes the serialized rollout schema.
- No persisted `UserMessage` event is counted in addition to its `ResponseItem`.
- No compaction operation updates previous-turn settings by itself.
- No bare `TurnContext` hydrates previous settings at the end of replay.
- No current resume-time context is inserted into a historical legacy compaction.
- `/continue` remains user-boundary-free.
- A conservative baseline clear is preferred over stale metadata when rollback crosses compacted history.
