# Turn History Reference Context Backport - Design Proposal

## Design Principle

Patch this branch in place.

Keep these branch-local invariants:

- `core/src/session/turn.rs` remains the main regular-turn loop.
- `/pause` and `/continue` continue to use this branch's continuation cleanup and pending-continuation state.
- Preserved work-notes remain part of this branch's compaction strategy.
- `GhostSnapshot` items remain stored in raw history and stripped from prompt history.
- Existing `CompactedItem.replacement_history` persistence remains the canonical compacted-history replay mechanism.

Use upstream behavior as a reference for durable turn-context state and model-downshift compaction, not as a source for a wholesale module refactor.

## Proposed Target Shape

### New state

Add two durable runtime concepts:

```rust
pub(crate) struct PreviousTurnSettings {
    pub(crate) model: String,
}
```

and a session/history baseline:

```rust
reference_context_item: Option<TurnContextItem>
previous_turn_settings: Option<PreviousTurnSettings>
```

A practical placement for this branch is:

- store `reference_context_item` in `ContextManager`, matching the upstream shape;
- store `previous_turn_settings` in `SessionState`, or expose it through `SessionState` while keeping history-specific baseline state in `ContextManager`.

The split keeps token/history state close to `ContextManager` while avoiding a larger session-state rewrite.

### Turn context serialization helper

Add `TurnContext::to_turn_context_item()` in `core/src/session/turn_context.rs`.

This branch's `protocol/src/protocol.rs` already defines `TurnContextItem` with the fields currently needed by this branch:

- `cwd`
- `approval_policy`
- `sandbox_policy`
- `model`
- `personality`
- `collaboration_mode`
- `effort`
- `summary`
- `user_instructions`
- `developer_instructions`
- `final_output_json_schema`
- `truncation_policy`

Use this helper everywhere the branch currently duplicates construction of `TurnContextItem`, including `core/src/session/turn.rs` and `core/src/compact.rs`.

## Patch Sequence

### Patch 1 - Make preserved work-notes contextual state

Scope:

- `core/src/compact.rs`
- `core/src/event_mapping.rs`
- `core/src/context_manager/history.rs`
- `core/src/session/turn.rs`

Changes:

1. Expose a small predicate for preserved work-notes messages, or move the prefix/predicate to a neutral module if needed to avoid cyclic dependencies.
2. Treat preserved work-notes as contextual user message content.
3. Ensure `is_user_turn_boundary(...)` returns `false` for preserved work-notes.
4. Ensure continuation helpers and user-message collection do not classify preserved work-notes as real user input.

Tests to add:

- `preserved_work_notes_are_not_user_turn_boundaries`
- `drop_last_n_user_turns_skips_preserved_work_notes`
- `history_needs_continuation_ignores_preserved_work_notes`

Rationale:

This is a prerequisite for reliable rollout replay. Work notes are user-role only because they must be model-visible. Semantically they are session state.

### Patch 2 - Persist a real turn context baseline

Scope:

- `core/src/context_manager/history.rs`
- `core/src/state/session.rs`
- `core/src/session/turn_context.rs`
- `core/src/session/mod.rs`
- `core/src/session/handlers.rs`
- `core/src/session/turn.rs`

Changes:

1. Add `reference_context_item` accessors to `ContextManager` and `Session`:

```rust
pub(crate) fn set_reference_context_item(&mut self, item: Option<TurnContextItem>);
pub(crate) fn reference_context_item(&self) -> Option<TurnContextItem>;
```

2. Add `previous_turn_settings` accessors to `SessionState` / `Session`:

```rust
pub(crate) async fn previous_turn_settings(&self) -> Option<PreviousTurnSettings>;
pub(crate) async fn set_previous_turn_settings(&self, value: Option<PreviousTurnSettings>);
```

3. Replace the duplicated `TurnContextItem` construction in `try_run_sampling_request(...)` with `turn_context.to_turn_context_item()`.

4. Stop treating `RolloutItem::TurnContext` emitted immediately before every sampling request as the baseline mechanism. The new baseline should be recorded once per real user turn, before that turn's user input is recorded.

5. Add:

```rust
pub(crate) async fn record_context_updates_and_set_reference_context_item(
    &self,
    turn_context: &TurnContext,
)
```

Behavior:

- if `reference_context_item` is `None`, record full initial context;
- otherwise record only context/settings diffs against the reference item;
- persist `RolloutItem::TurnContext(turn_context.to_turn_context_item())` even when no model-visible diff items were emitted;
- update in-memory `reference_context_item` to the current turn context;
- update `previous_turn_settings` after the regular turn reaches a stable sampling point.

6. In `core/src/session/handlers.rs`, replace the handler-scoped `previous_context` diff source with the session baseline:

Current shape:

```rust
sess.seed_initial_context_if_needed(&current_context).await;
let resumed_model = sess.take_pending_resume_previous_model().await;
let update_items = sess.build_settings_update_items(
    previous_context.as_ref(),
    resumed_model.as_deref(),
    current_context.as_ref(),
);
```

Target shape:

```rust
sess.record_context_updates_and_set_reference_context_item(current_context.as_ref()).await;
```

Keep `previous_context` only where this branch still needs it for non-history workflows, then remove it once no caller depends on it.

### Patch 3 - Convert settings diffs to use `TurnContextItem`

Scope:

- `core/src/session/mod.rs`
- optionally a new `core/src/context_manager/updates.rs`

Changes:

1. Change settings-update builders from `Option<&Arc<TurnContext>>` to `Option<&TurnContextItem>`.
2. Build environment diffs from `TurnContextItem` to current `TurnContext`.
3. Build policy/model/collaboration/personality diffs from `TurnContextItem` and `PreviousTurnSettings`.
4. Keep model-switch instructions before other developer-context diffs.

Suggested shape:

```rust
fn build_settings_update_items(
    &self,
    previous: Option<&TurnContextItem>,
    previous_turn_settings: Option<&PreviousTurnSettings>,
    current_context: &TurnContext,
) -> Vec<ResponseItem>
```

Notes for this branch:

- This branch's `TurnContextItem` does not carry upstream fields such as realtime state, permission profiles, or network domain policy. Do not add unrelated fields in the MVP.
- Use existing current-branch rendering helpers: `DeveloperInstructions`, `EnvironmentContext`, `UserInstructions`, collaboration-mode instructions, and personality messages.
- If a setting cannot be safely diffed from the current `TurnContextItem` fields, clear the reference baseline and force full initial context on the next real user turn rather than emitting a guessed diff.

### Patch 4 - Reconstruct history plus metadata from rollout

Scope:

- new `core/src/session/rollout_reconstruction.rs`
- `core/src/session/mod.rs`
- `core/src/session/handlers.rs`
- `core/src/session/tests.rs`

Replace `reconstruct_history_from_rollout(...) -> Vec<ResponseItem>` with a replay result:

```rust
pub(crate) struct ReconstructedRollout {
    pub(crate) history: Vec<ResponseItem>,
    pub(crate) reference_context_item: Option<TurnContextItem>,
    pub(crate) previous_turn_settings: Option<PreviousTurnSettings>,
    pub(crate) pending_continuation: Option<PendingContinuation>,
}
```

Replay rules:

1. `RolloutItem::ResponseItem(item)` records into the temporary `ContextManager`.
2. `RolloutItem::Compacted(compacted)`:
   - if `replacement_history` exists, replace temporary history with it;
   - if `replacement_history` is missing, rebuild legacy compacted history as today, then clear `reference_context_item` because the exact full context baseline is unknown.
3. `RolloutItem::TurnContext(item)` updates `previous_turn_settings` and, when appropriate, `reference_context_item`.
4. `RolloutItem::EventMsg(ThreadRolledBack { num_turns })` drops surviving history by user turn boundaries and clears/recomputes baseline if the dropped span contained the baseline-defining turn.
5. `RolloutItem::EventMsg(TurnAborted { reason: Interrupted, ... })` updates pending-continuation metadata as today.

The upstream project uses a reverse-scan/lazy replay strategy. This branch can start with a simpler linear replay if tests prove the final metadata and history match expected behavior. If replay cost becomes visible, port the upstream reverse-scan optimization later.

Thread rollback should use this replay path when persisted rollout is available:

1. flush any live rollout items;
2. append a synthetic rollback marker to the replay input;
3. reconstruct history and metadata;
4. replace in-memory history and baseline state;
5. persist the rollback marker;
6. recompute token usage.

Fallback when persisted rollout is unavailable:

- keep the existing in-memory `drop_last_n_user_turns(...)` behavior;
- clear `reference_context_item` and `previous_turn_settings` so the next real user turn reinjects full context.

### Patch 5 - Add reference-aware compaction history replacement

Scope:

- `core/src/compact.rs`
- `core/src/compact_remote.rs`
- `core/src/session/mod.rs`
- `core/src/session/turn.rs`

Add a small enum:

```rust
pub(crate) enum InitialContextInjection {
    DoNotInject,
    BeforeLastUserMessage,
}
```

Use it in local and remote compaction:

|Compaction site|Injection mode|Reference baseline after compaction|
|---|---|---|
|Pre-turn context-limit compaction|`DoNotInject`|`None`; next real user turn reinjects full context.|
|Pre-turn model-downshift compaction|`DoNotInject`|`None`; next real user turn reinjects full context for the new model.|
|Mid-turn compaction before a required follow-up|`BeforeLastUserMessage`|`Some(turn_context.to_turn_context_item())`, because full context is included in replacement history before the last real user message.|
|Manual `/compact` task|`DoNotInject` unless it immediately continues a model turn|`None` by default.|

Add:

```rust
pub(crate) async fn replace_compacted_history(
    &self,
    items: Vec<ResponseItem>,
    reference_context_item: Option<TurnContextItem>,
    compacted_item: CompactedItem,
)
```

Behavior:

- replace in-memory history;
- set `reference_context_item` to the supplied value;
- persist `RolloutItem::Compacted(compacted_item)`;
- if `reference_context_item` is `Some`, persist a matching `RolloutItem::TurnContext(...)` after the compaction item;
- reset WebSocket state through the existing compacted-result path;
- recompute token usage.

Preserved work-notes handling:

- keep extracting and carrying forward preserved notes;
- keep filtering notes from `collect_user_messages(...)`;
- append preserved notes in a deterministic position after the compacted summary and before ghost snapshots;
- do not let preserved notes become the "last real user message" insertion point.

### Patch 6 - Add model-downshift pre-sampling compaction

Scope:

- `core/src/session/turn.rs`
- `core/src/session/turn_context.rs`
- `core/src/session/mod.rs`

Add `TurnContext::with_model(...)` or a narrower helper that resolves a previous model against `ModelsManager` and returns a temporary compaction context.

Pre-sampling logic:

```rust
async fn maybe_run_previous_model_inline_compact(
    sess: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
    total_usage_tokens: i64,
) -> CodexResult<bool>
```

Run it before recording current-turn context updates and user input.

Preconditions:

- `previous_turn_settings` exists;
- previous model slug differs from current model slug;
- previous model effective context window is larger than current model effective context window;
- `total_usage_tokens > current_model.auto_compact_token_limit()`.

Action:

- build a temporary turn context using the previous model;
- compact using `InitialContextInjection::DoNotInject` and reason `ModelDownshift`;
- return `Ok(true)` so caller resets the turn-scoped WebSocket state;
- then continue normal turn flow with the current model.

Work-notes stance for MVP:

- Do not add a second pre-downshift work-notes capture pass.
- Reuse any existing preserved work-notes already present in history.
- The purpose of model-downshift compaction is to avoid over-limit failure before the new smaller model sees the prompt; extra notes capture can be added later if needed.

### Patch 7 - Fix pending-input ordering

Scope:

- `core/src/session/turn.rs`

Use upstream's small state machine without porting the full task loop.

Target behavior:

```rust
let mut can_drain_pending_input = input.is_empty();
```

Then:

- only call `sess.get_pending_input().await` when `can_drain_pending_input` is true and work-notes capture is idle;
- after a successful sampling request, set `can_drain_pending_input = true`;
- after mid-turn compaction when the model still needs a follow-up, keep pending input deferred until the model follow-up is sampled;
- after compaction when the model does not need a follow-up, allow pending input.

This preserves the first sampling request for a fresh explicit prompt.

## Rollout Compatibility

The plan must handle existing persisted sessions:

|Persisted data shape|Expected replay behavior|
|---|---|
|Only `ResponseItem` lines|Replay history. Baseline remains `None`; next real turn injects full context.|
|Legacy `CompactedItem` without `replacement_history`|Rebuild compacted history using existing fallback. Baseline becomes `None`.|
|Modern `CompactedItem` with `replacement_history` but no following `TurnContext`|Replay replacement history. Baseline remains `None`; next turn injects full context.|
|Modern `CompactedItem` followed by `TurnContext`|Replay replacement history and restore the reference baseline.|
|Rollback marker after a baseline-defining turn|Drop the affected turn and clear/recompute baseline from surviving rollout metadata.|

## Risk Controls

|Risk|Control|
|---|---|
|Duplicate full initial context|Only inject full context when `reference_context_item` is `None`; persist baseline after injection.|
|Stale baseline after rollback|Use rollout replay for rollback when possible; otherwise clear baseline.|
|Baseline restored after an imprecise legacy compaction|Clear baseline when replaying compaction without exact `replacement_history`.|
|Work notes counted as user turn|Make work notes contextual before adding replay tests.|
|Model-downshift compaction uses wrong model-specific instructions|Build the temporary compaction context with the previous model and its compatible reasoning effort.|
|Prefix-cache churn from persisted metadata|`RolloutItem::TurnContext` is rollout metadata, not model input; only context diffs/full context affect prompt history.|
|Remote compaction mismatch|Apply the same `InitialContextInjection` and baseline-setting semantics to local and remote compaction paths.|

## Test Matrix

Add unit/integration tests covering:

- `record_context_updates_and_set_reference_context_item_injects_full_context_when_baseline_missing`
- `record_context_updates_and_set_reference_context_item_persists_baseline_without_diff_items`
- `record_context_updates_and_set_reference_context_item_emits_model_switch_before_other_diffs`
- `reconstruct_history_restores_reference_context_item_after_regular_turn`
- `reconstruct_history_clears_reference_context_item_after_legacy_compaction`
- `reconstruct_history_restores_reference_context_item_after_compaction_with_replacement_history_and_turn_context`
- `thread_rollback_recomputes_reference_context_item`
- `thread_rollback_clears_reference_context_item_when_rollback_crosses_baseline`
- `model_downshift_uses_previous_model_for_pre_sampling_compaction`
- `explicit_prompt_samples_before_pending_input`
- `mid_turn_compaction_defers_pending_input_until_model_follow_up_finishes`
- `preserved_work_notes_are_not_user_turn_boundaries`

## Acceptance Criteria

A reviewer should be able to verify from tests that:

1. History replay and live history produce the same prompt input after resume/fork/rollback.
2. The current branch does not emit duplicate full initial context across ordinary turns.
3. A smaller-model turn compacts with the previous larger model before the smaller model is asked to sample.
4. Preserved work-notes survive compaction but do not count as user turns.
5. The first sampling request for explicit input is not polluted by queued pending input.
