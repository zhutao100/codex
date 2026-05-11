# Turn History Reference Context Backport - Implementation Plan

## Phase 0 - Lock Existing Fixes With Tests

Before changing behavior, keep or add focused tests for already-landed fixes so the backport does not regress them.

Validation targets:

- `core/src/context_manager/history_tests.rs`
- `core/src/context_manager/normalize.rs`
- `core/src/compact.rs`
- `core/src/compact_remote.rs`
- `core/src/session/tests.rs`

Commands:

```bash
CODEX_SANDBOX_NETWORK_DISABLED=1 scripts/cargo-local test -p codex-core context_manager::history_tests
CODEX_SANDBOX_NETWORK_DISABLED=1 scripts/cargo-local test -p codex-core compact
CODEX_SANDBOX_NETWORK_DISABLED=1 scripts/cargo-local test -p codex-core session::tests::record_initial_history
CODEX_SANDBOX_NETWORK_DISABLED=1 scripts/cargo-local test -p codex-core session::tests::thread_rollback
```

Expected status after this phase:

- token-tail accounting remains covered;
- text-only prompt image stripping remains covered;
- replacement-history compaction replay remains covered;
- rollback contextual-trim behavior remains covered.

## Phase 1 - Classify Preserved Work-Notes As Contextual State

### Steps

1. Move or expose the preserved-work-notes predicate so `event_mapping` and `history` can use it without an awkward dependency direction.
2. Update contextual user-message classification to include preserved work-notes.
3. Update user-turn-boundary and continuation helpers to ignore preserved work-notes.
4. Add tests before touching rollout reconstruction.

### Files

- `core/src/compact.rs`
- `core/src/event_mapping.rs`
- `core/src/context_manager/history.rs`
- `core/src/session/turn.rs`

### Tests

```bash
CODEX_SANDBOX_NETWORK_DISABLED=1 scripts/cargo-local test -p codex-core preserved_work_notes
CODEX_SANDBOX_NETWORK_DISABLED=1 scripts/cargo-local test -p codex-core drop_last_n_user_turns
CODEX_SANDBOX_NETWORK_DISABLED=1 scripts/cargo-local test -p codex-core history_needs_continuation
```

## Phase 2 - Add Turn Context Helpers And Baseline State

### Steps

1. Add `TurnContext::to_turn_context_item()`.
2. Replace duplicated `TurnContextItem` construction in compaction and sampling code.
3. Add `reference_context_item` to `ContextManager` and expose it through `Session`.
4. Add `PreviousTurnSettings` and accessors to session state.
5. Add `record_context_updates_and_set_reference_context_item(...)`.
6. Switch the regular user-turn path to use the session baseline instead of handler-local `previous_context` for model-visible context diffs.
7. Defer removal of `initial_context_seeded` until the replay path is complete; during transition, ensure it does not double-inject full context.

### Files

- `core/src/session/turn_context.rs`
- `core/src/context_manager/history.rs`
- `core/src/state/session.rs`
- `core/src/session/mod.rs`
- `core/src/session/handlers.rs`
- `core/src/session/turn.rs`
- `core/src/compact.rs`

### Tests

```bash
CODEX_SANDBOX_NETWORK_DISABLED=1 scripts/cargo-local test -p codex-core record_context_updates_and_set_reference_context_item
CODEX_SANDBOX_NETWORK_DISABLED=1 scripts/cargo-local test -p codex-core build_settings_update_items
CODEX_SANDBOX_NETWORK_DISABLED=1 scripts/cargo-local test -p codex-core turn_context
```

## Phase 3 - Convert Settings Diff Builders To `TurnContextItem`

### Steps

1. Change builder signatures from `Option<&Arc<TurnContext>>` to `Option<&TurnContextItem>`.
2. Add conversion helpers on current context structs where needed, especially environment diffing.
3. Use `PreviousTurnSettings` for model-switch diffs.
4. Keep model-switch developer instructions before other developer diffs.
5. Clear the reference baseline if a diff cannot be safely derived from persisted fields.

### Files

- `core/src/session/mod.rs`
- optional new `core/src/context_manager/updates.rs`
- existing context-rendering modules used by `EnvironmentContext` and `DeveloperInstructions`

### Tests

```bash
CODEX_SANDBOX_NETWORK_DISABLED=1 scripts/cargo-local test -p codex-core build_settings_update_items
CODEX_SANDBOX_NETWORK_DISABLED=1 scripts/cargo-local test -p codex-core model_switch
CODEX_SANDBOX_NETWORK_DISABLED=1 scripts/cargo-local test -p codex-core collaboration_mode
CODEX_SANDBOX_NETWORK_DISABLED=1 scripts/cargo-local test -p codex-core personality
```

## Phase 4 - Add Rollout Reconstruction Metadata

### Steps

1. Add `core/src/session/rollout_reconstruction.rs`.
2. Replace `reconstruct_history_from_rollout(...) -> Vec<ResponseItem>` with a result that includes:
   - reconstructed history;
   - `reference_context_item`;
   - `previous_turn_settings`;
   - pending continuation.
3. Replay `CompactedItem.replacement_history` exactly when available.
4. Clear baseline for legacy compactions without replacement history.
5. Apply `ThreadRolledBack` during replay.
6. Update `record_initial_history(...)` for new/resumed/forked sessions.
7. Update rollback handler to reconstruct from rollout when possible, and to clear baseline on fallback in-memory rollback.

### Files

- `core/src/session/rollout_reconstruction.rs`
- `core/src/session/mod.rs`
- `core/src/session/handlers.rs`
- `core/src/session/tests.rs`

### Tests

```bash
CODEX_SANDBOX_NETWORK_DISABLED=1 scripts/cargo-local test -p codex-core rollout_reconstruction
CODEX_SANDBOX_NETWORK_DISABLED=1 scripts/cargo-local test -p codex-core record_initial_history_reconstructs_resumed_transcript
CODEX_SANDBOX_NETWORK_DISABLED=1 scripts/cargo-local test -p codex-core record_initial_history_reconstructs_forked_transcript
CODEX_SANDBOX_NETWORK_DISABLED=1 scripts/cargo-local test -p codex-core thread_rollback
```

## Phase 5 - Reference-Aware Compaction Replacement

### Steps

1. Add `InitialContextInjection`.
2. Add `Session::replace_compacted_history(...)`.
3. Change local compaction to build replacement history with either:
   - no full context, clearing baseline; or
   - full context inserted before the last real user message, setting baseline.
4. Apply equivalent baseline semantics to remote compaction.
5. Preserve work-notes and ghost snapshots in deterministic positions.
6. Keep existing WebSocket reset behavior after any history rewrite.

### Files

- `core/src/compact.rs`
- `core/src/compact_remote.rs`
- `core/src/session/mod.rs`
- `core/src/session/turn.rs`

### Tests

```bash
CODEX_SANDBOX_NETWORK_DISABLED=1 scripts/cargo-local test -p codex-core compact
CODEX_SANDBOX_NETWORK_DISABLED=1 scripts/cargo-local test -p codex-core replacement_history
CODEX_SANDBOX_NETWORK_DISABLED=1 scripts/cargo-local test -p codex-core preserved_work_notes
CODEX_SANDBOX_NETWORK_DISABLED=1 scripts/cargo-local test -p codex-core ghost_snapshot
```

## Phase 6 - Model-Downshift Pre-Sampling Compaction

### Steps

1. Add `TurnContext::with_model(...)` or an equivalent narrower helper.
2. Add `maybe_run_previous_model_inline_compact(...)`.
3. Call it before recording current-turn context updates and user input.
4. Use `InitialContextInjection::DoNotInject`.
5. Reset the turn-scoped WebSocket state when compaction ran.
6. Add telemetry/log fields for previous model, current model, old/new context windows, total usage tokens, and compact decision.

### Files

- `core/src/session/turn_context.rs`
- `core/src/session/turn.rs`
- `core/src/session/mod.rs`

### Tests

```bash
CODEX_SANDBOX_NETWORK_DISABLED=1 scripts/cargo-local test -p codex-core model_downshift
CODEX_SANDBOX_NETWORK_DISABLED=1 scripts/cargo-local test -p codex-core previous_turn_settings
CODEX_SANDBOX_NETWORK_DISABLED=1 scripts/cargo-local test -p codex-core auto_compact
```

## Phase 7 - Pending-Input Ordering

### Steps

1. Add `can_drain_pending_input` initialized to `input.is_empty()`.
2. Gate `sess.get_pending_input().await` on that flag and work-notes capture state.
3. After a successful sampling response, allow pending input.
4. After mid-turn compaction, defer pending input when the model still needs a follow-up.
5. Keep existing `/pause` and `/continue` behavior unchanged.

### Files

- `core/src/session/turn.rs`

### Tests

```bash
CODEX_SANDBOX_NETWORK_DISABLED=1 scripts/cargo-local test -p codex-core pending_input
CODEX_SANDBOX_NETWORK_DISABLED=1 scripts/cargo-local test -p codex-core continue
CODEX_SANDBOX_NETWORK_DISABLED=1 scripts/cargo-local test -p codex-core pause
```

## Phase 8 - End-To-End Regression Pass

Run targeted tests first, then a broader core test pass.

```bash
CODEX_SANDBOX_NETWORK_DISABLED=1 scripts/cargo-local test -p codex-core context_manager::history_tests
CODEX_SANDBOX_NETWORK_DISABLED=1 scripts/cargo-local test -p codex-core compact
CODEX_SANDBOX_NETWORK_DISABLED=1 scripts/cargo-local test -p codex-core session::tests::thread_rollback
CODEX_SANDBOX_NETWORK_DISABLED=1 scripts/cargo-local test -p codex-core session::tests::record_initial_history
CODEX_SANDBOX_NETWORK_DISABLED=1 scripts/cargo-local test -p codex-core session::tests::pending
CODEX_SANDBOX_NETWORK_DISABLED=1 scripts/cargo-local test -p codex-core
```

Manual scenario checks:

|Scenario|Expected result|
|---|---|
|New session, first prompt|Full context is injected once before the first user prompt; a baseline is persisted.|
|Second prompt with no settings changes|No duplicate full context and no empty diff message.|
|Model switch to same-context or larger-context model|No pre-downshift compaction unless ordinary token limit requires it.|
|Model switch to smaller-context model with over-limit history|Compaction uses the previous model, then current model samples compacted history.|
|Rollback one turn after settings changes|Next turn's context diff is based on surviving baseline, or full context is re-injected if baseline was removed.|
|Resume after compaction with replacement history|Prompt history and baseline match the persisted replacement history.|
|Resume after legacy compaction without replacement history|Baseline is cleared; next turn re-injects full context.|
|Auto-compaction with preserved work-notes|Notes remain model-visible but are not user turn boundaries.|
|Fresh prompt while pending input exists|The fresh prompt is sampled first; pending input is handled as follow-up.|

## Review Checklist

- [ ] No user-facing docs or comments refer to runtime extraction paths.
- [ ] No new code path treats `RolloutItem::TurnContext` as model input.
- [ ] Rollout metadata remains backward compatible with old sessions.
- [ ] Existing custom features remain intact: `/pause`, `/continue`, preserved work-notes, post-turn review, delegate workflows, model overlay/provider override.
- [ ] Every history rewrite recomputes token usage.
- [ ] Every history rewrite that can invalidate WebSocket incremental state resets the turn-scoped WebSocket session.
- [ ] Tests cover both local and remote compaction baseline semantics.
