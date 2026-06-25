# Implementation Plan

## Status

Completed in this branch. The implementation kept the mandatory patch inside replay reconstruction, plus tests and removal of a stale test-only continuation reconstruction helper in `core/src/session/mod.rs`.

## 1. Patch boundaries

### Mandatory source files

- `core/src/session/rollout_reconstruction.rs`
- `core/src/session/mod.rs` for removing the obsolete test-only continuation reconstruction helper
- `core/src/session/tests.rs`
- `core/src/session/turn.rs` only if a small helper/signature change is needed for compaction-aware continuation derivation

### Optional organization-only file

- `core/src/session/rollout_reconstruction_tests.rs` if moving the growing replay test set improves maintainability; do not require the move for correctness.

### Files inspected but not otherwise changed

- `core/src/context_manager/history.rs`
- `core/src/context_manager/normalize.rs`
- `core/src/compact.rs`
- `core/src/compact_remote.rs`
- `core/src/rollout/policy.rs`
- `core/src/client.rs`
- `protocol/src/protocol.rs`
- `protocol/src/openai_models.rs`

A mandatory change to any protocol or rollout item should trigger scope review before implementation.

## 2. Phase 0: lock current live invariants

Update or add tests that demonstrate behavior already present:

- context recording sets the reference baseline but not previous settings;
- recording a real user commits previous settings;
- `replace_compacted_history(..., None, ...)` clears reference context but preserves previous settings;
- invalid-image recovery ignores contextual user messages;
- work notes are not real user boundaries.

These tests prevent the replay fix from accidentally changing the live path to resemble stale assumptions.

## 3. Phase 1: introduce replay metadata types

In `core/src/session/rollout_reconstruction.rs`:

1. Add private `ReplayMetadata` and `MetadataCheckpoint` types.
2. Add a private replay-epoch type or equivalent local variables.
3. Replace `context_stack` with:
   - `pending_context`;
   - `current_metadata`;
   - `epoch_base_metadata`;
   - post-base checkpoint vector;
   - compaction adjacency/provenance flags.
4. Add small helper methods for:
   - committing a real user boundary;
   - applying a compaction checkpoint;
   - restoring metadata after rollback;
   - recognizing adjacent post-compaction context.

Keep history materialization in the existing forward pass. Do not port upstream reverse replay in this phase.

## 4. Phase 2: commit metadata only at real user boundaries

For each `RolloutItem::ResponseItem`:

- record it through `ContextManager` exactly as today;
- use the same `is_user_turn_boundary_response_item`/`is_user_turn_boundary` predicate as history rollback;
- commit `pending_context` and push one checkpoint only at that boundary;
- carry existing metadata for a later steering boundary without a new `TurnContext`.

For ordinary `TurnContext`:

- replace the pending candidate;
- do not update reconstructed state immediately.

Acceptance gate:

- a rollout containing only `TurnContext` hydrates neither reference context nor previous settings;
- `TurnContext + real user` hydrates both;
- contextual user messages and work notes do not commit the candidate.

## 5. Phase 3: add compaction replay epochs

At `Compacted`:

1. Materialize exact `replacement_history` when present.
2. Clear pending context.
3. Preserve previous settings.
4. Clear reference context.
5. reset post-base checkpoints;
6. mark the replacement as opaque;
7. arm one-record adjacency recognition only when `replacement_history` is present.

For an immediately following `TurnContext`:

- re-establish reference context in both current and epoch-base metadata;
- leave previous settings unchanged;
- mark the replacement as mid-turn/injected-context provenance.

Any other rollout item disarms adjacency. Legacy compactions without `replacement_history` also do not arm adjacency, because their rebuilt history intentionally lacks canonical context. This matters for standalone compaction followed by a later normal turn: the next turn appends context response items before persisting its `TurnContext`, so it is not mistaken for same-batch mid-turn context injection.

Acceptance gate:

- standalone/manual compaction resumes with old previous settings and no reference baseline;
- mid-turn compaction resumes with old/current committed previous settings and the adjacent reference baseline;
- the adjacent context does not create a fake rollback turn.

## 6. Phase 4: align rollback with checkpoints

Replace numeric truncation of context records with checkpoint restoration.

Algorithm:

```text
history.drop_last_n_user_turns(N)

if N <= post_base_checkpoints.len:
    remove N checkpoints
    restore last checkpoint or epoch base
else if epoch is opaque:
    clear metadata conservatively
    start a new opaque epoch from surviving history
else:
    clear metadata
```

Clear `pending_context` and stale continuation hints after rollback.

Acceptance gate:

- stray/bare contexts never consume rollback slots;
- rollback within post-compaction appended turns restores exact metadata;
- rollback crossing replacement history returns `None` metadata rather than a stale model/context;
- repeated rollback markers remain cumulative.

## 7. Phase 5: fix legacy compaction reconstruction

Change only the legacy fallback from resume-time initial context to an empty initial context:

```rust
compact::build_compacted_history(Vec::new(), &user_messages, &compacted.message)
```

Update the current test that expects resume-time context injection.

Acceptance gate:

- reconstructing the same legacy rollout under two different current cwd/policy/instruction contexts yields the same historical compacted prefix;
- reference context is clear afterward;
- previous settings survive when a prior committed user turn supplies them.

## 8. Phase 6: make continuation compaction-aware

Keep `history_needs_continuation` focused on raw history. In reconstruction, suppress its fallback when the latest relevant tail is a standalone/pre-turn compaction with no later real user boundary.

Use `current_metadata.previous_turn_settings.model` for `PendingContinuation.model`.

Acceptance gate:

- manual compaction does not hydrate a regular continuation;
- mid-turn compaction interrupted before assistant completion does;
- explicit interrupted user/tool tails continue to work;
- `/continue` still performs no context/user/skill reinjection.

## 9. Phase 7: integration and cache checks

Run the focused tests in `test_matrix.md`, then the surrounding suites. At minimum:

```bash
just fmt
CODEX_SANDBOX_NETWORK_DISABLED=1 cargo-local test -p codex-core rollout_reconstruction
CODEX_SANDBOX_NETWORK_DISABLED=1 cargo-local test -p codex-core thread_rollback
CODEX_SANDBOX_NETWORK_DISABLED=1 cargo-local test -p codex-core history_needs_continuation
CODEX_SANDBOX_NETWORK_DISABLED=1 cargo-local test -p codex-core prompt_caching
CODEX_SANDBOX_NETWORK_DISABLED=1 cargo-local test -p codex-core compact
```

Test filters may need adjustment to match the workspace harness. Also run the full `codex-core` test target used by this branch before merge.

Inspect request bodies in prompt-cache tests rather than relying only on token counters:

- ordinary second-turn input starts with first-turn input;
- settings updates append after the reusable prefix;
- compaction establishes a new deterministic base;
- replay does not inject current context before an old legacy summary.

## 10. Review checklist

### Correctness

- [x] Same predicate defines history and metadata user boundaries.
- [x] Bare context remains uncommitted.
- [x] Post-compaction adjacent context changes reference only.
- [x] Compaction preserves previous settings.
- [x] Rollback never decrements metadata for a non-user task.
- [x] Opaque-base crossing clears uncertain metadata.
- [x] Legacy compaction is independent of resume-time context.
- [x] Continuation model comes from committed previous settings.

### Branch compatibility

- [x] Work notes remain contextual.
- [x] `GhostSnapshot` remains retained raw and omitted from prompts.
- [x] `/pause` and `/continue` event behavior is unchanged.
- [x] Local and remote compaction produce equivalent replay metadata semantics.
- [x] Post-turn review/delegate contexts do not commit previous-turn settings; review synthetic output only carries current metadata into rollback checkpoints.
- [x] Old rollout JSON parses without migration.

### Scope control

- [x] No lifecycle event persistence added.
- [x] No protocol fields added.
- [x] No upstream response variants imported.
- [x] No cache-key or request-shape change.
- [x] No change to live previous-setting commitment order.

## 11. Rollout strategy

Land as one correctness series with tests adjacent to each behavioral change. Avoid a mechanical upstream cherry-pick. The branch-local implementation should remain easy to remove later if a complete lifecycle/reverse-replay subsystem is intentionally ported.

A useful internal metric after deployment is the count of resumes where:

- no reference baseline is reconstructed and full context is appended;
- a pending continuation is inferred from history rather than an explicit interrupt;
- rollback crosses an opaque compaction base.

Metrics are optional and must not become prerequisites for the fix.
