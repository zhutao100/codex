# Design Proposal

## 1. Design principles

1. Port semantics, not upstream architecture.
2. Keep the existing rollout wire format in the mandatory patch.
3. Use the same non-contextual user `ResponseItem` boundaries as `ContextManager::drop_last_n_user_turns`; do not invent lifecycle segmentation that this branch cannot persist.
4. Treat `TurnContext` before a user boundary as a candidate, not a committed user turn.
5. Treat a `TurnContext` immediately following `Compacted` as compacted-history reference metadata, not previous-turn settings.
6. Preserve `/continue` as a continuation of the existing user turn.
7. Prefer conservative full reinjection over a stale baseline when an old compacted prefix makes association unprovable.

## 2. Required invariant split

Maintain two independent metadata values:

```rust
reference_context_item: Option<TurnContextItem>
previous_turn_settings: Option<PreviousTurnSettings>
```

### Reference context

Meaning: the settings snapshot represented by model-visible context messages in the current history and safe to use as the next diff baseline.

It may be established by:

- context updates preceding a real user turn;
- full context inserted into compacted replacement history.

It may be cleared by:

- compaction with `InitialContextInjection::DoNotInject`;
- legacy compaction whose historical context cannot be reconstructed;
- conservative rollback across an opaque compacted prefix.

### Previous-turn settings

Meaning: settings from the latest surviving real user turn.

It changes only when:

- a real user boundary commits a pending `TurnContext`;
- rollback restores an older committed user-turn checkpoint;
- reconstruction cannot prove an older value and clears it.

Compaction alone must not update it.

## 3. Live-path changes

### 3.1 Keep context records before user input

Do not move model-visible context update messages after the user prompt. Their current order is intentional:

```text
context differences
TurnContext metadata
real user message
```

The `TurnContext` remains a durable candidate that replay later associates with the following real user boundary.

### 3.2 Stop committing previous settings in the context-update helper

Change `record_context_updates_and_set_reference_context_item` so it:

- computes and records full context or differences;
- persists `RolloutItem::TurnContext`;
- advances the live `reference_context_item`;
- does **not** set `previous_turn_settings`.

After `record_user_prompt_and_emit_turn_item` completes, commit:

```rust
sess.set_previous_turn_settings(Some(PreviousTurnSettings {
    model: turn_context.model_info.slug.clone(),
}))
.await;
```

This matches the durable user boundary while retaining current prompt ordering.

### 3.3 Preserve previous settings through compaction

Change `replace_compacted_history` so replacement updates:

- raw history;
- `reference_context_item`;
- `initial_context_seeded`;

but leaves `previous_turn_settings` unchanged.

Split the current all-in-one baseline clear into explicit operations, for example:

```rust
clear_reference_context_baseline()
clear_all_reconstructed_turn_metadata()
```

The fallback rollback path may use the second. Normal `DoNotInject` compaction uses only the first through replacement state.

## 4. Branch-local replay state machine

This branch cannot directly adopt upstream reverse segmentation because it does not durably persist `TurnStarted`/`TurnComplete` and its `TurnContextItem` has no turn ID.

Use a forward state machine over existing records.

### 4.1 Replay state

Conceptual structures:

```rust
#[derive(Clone)]
struct ReplayMetadata {
    reference_context_item: Option<TurnContextItem>,
    previous_turn_settings: Option<PreviousTurnSettings>,
}

struct ReplayEpoch {
    base: ReplayMetadata,
    committed_user_turns: Vec<ReplayMetadata>,
}

struct ReplayState {
    metadata: ReplayMetadata,
    pending_context: Option<TurnContextItem>,
    epoch: ReplayEpoch,
    turn_context_may_attach_to_compaction: bool,
}
```

An epoch begins at session start or immediately after each `Compacted` record. Replacement history is an opaque base: it may contain selected old user messages and a summary, but this branch lacks enough identifiers to reconstruct per-original-turn metadata inside it.

### 4.2 Event rules

|Rollout item|History action|Metadata action|
|---|---|---|
|Non-boundary `ResponseItem`|Record with current truncation policy|Clear the immediate-compaction attachment flag|
|Real user-boundary `ResponseItem`|Record as above|Clear the attachment flag; commit `pending_context`; push one user-turn checkpoint; clear pending continuation|
|`TurnContext(ctx)` immediately after `Compacted`|No model-input action|Set reference baseline to `ctx`; update epoch base; do not change previous settings|
|Other `TurnContext(ctx)`|No model-input action|Replace `pending_context` with `ctx`|
|`Compacted` with replacement|Replace raw history|Discard pending context, clear reference, preserve previous settings, start new epoch, permit only the directly following item to attach as compacted baseline|
|Legacy `Compacted`|Rebuild compacted history without initial context|Same metadata reset as replacement compaction|
|`ThreadRolledBack(N)`|Call `drop_last_n_user_turns(N)`|Discard `pending_context`, pending continuation, and the attachment flag; pop up to N checkpoints from the current epoch; restore the matching snapshot or epoch base; if N crosses the epoch base, clear uncertain metadata and reset the epoch|
|Interrupted `TurnAborted`|No history action here|Set pending continuation using committed previous settings, then reference model as fallback|
|Persisted `UserMessage` event|No history action|May clear pending continuation; never count it as a second user boundary|
|Other events/session metadata|No turn-count action|Do not preserve the immediate-compaction attachment flag unless the implementation proves such records can be interposed safely|

### 4.3 Real user-boundary commit

On `ResponseItem` satisfying `is_user_turn_boundary`:

```text
if pending_context exists:
    reference_context_item = pending_context
    previous_turn_settings.model = pending_context.model

push snapshot(reference_context_item, previous_turn_settings)
pending_context = None
```

Push a checkpoint even for old rollouts where no candidate exists. The checkpoint keeps rollback counts aligned, while metadata remains unchanged or unknown.

### 4.4 Direct post-compaction attachment

Current `replace_compacted_history` persists:

```text
Compacted
TurnContext   // only when replacement history contains injected initial context
```

as adjacent records in one persistence call. That exact adjacency is the only safe branch-local signal that the context item describes replacement history without a new user turn. This relies on the existing full initial-context builder emitting at least one `ResponseItem` before a normal turn's `TurnContext`; preserve that ordering and cover it with a regression test.

Do not leave the attachment flag active across a `ResponseItem`. This distinguishes:

```text
Compacted
TurnContext         // compacted replacement baseline
```

from:

```text
Compacted
context ResponseItem
TurnContext
user ResponseItem   // next normal turn candidate
```

A pre-compaction task `TurnContext` is discarded when `Compacted` is processed.

### 4.5 Rollback within and across epochs

Let `K` be the number of committed real-user checkpoints after the latest compaction.

- `N < K`: pop N checkpoints and restore the new last checkpoint.
- `N == K`: restore the epoch base.
- `N > K`: history rollback crosses into the opaque compacted base. Apply the history truncation, clear pending context plus both committed metadata fields, and reset the epoch to an empty/unknown base because this branch cannot prove which historical context belongs to the remaining selected messages.

The next regular turn then injects full current context. This is intentionally conservative and backward compatible. A second rollback operates on the reset epoch and the already-truncated history, never on stale checkpoints from before the first rollback.

## 5. Legacy compaction reconstruction

Replace resume-time initial-context injection with:

```rust
let rebuilt = compact::build_compacted_history(
    Vec::new(),
    &user_messages,
    &compacted.message,
);
```

After any such legacy compaction:

- `reference_context_item = None`;
- the next regular turn performs full context injection;
- `previous_turn_settings` remains the latest committed real-user setting only when replay can still prove it; otherwise it is cleared conservatively.

This keeps current settings at the current end of history rather than inserting them at an old checkpoint.

## 6. Pending continuation

Reconstruction should choose the continuation model from committed metadata:

```text
previous_turn_settings.model
or reference_context_item.model
or None
```

An uncommitted `pending_context` must never choose the continuation model.

A `/continue` execution records no new user boundary and therefore does not append a checkpoint. Its later completed items remain part of the original user turn's durable history.

## 7. Invalid-image boundary fix

Change the reverse stop condition in `ContextManager::replace_last_turn_images` from:

```rust
matches!(item, ResponseItem::Message { role, .. } if role == "user")
```

to:

```rust
is_user_turn_boundary(item)
```

No other upstream history-version or image-detail machinery is required.

## 8. Backward compatibility

|Rollout shape|Result|
|---|---|
|Current rollout with `TurnContext` before every user|Fully associated by forward commit|
|Bare trailing `TurnContext`|Ignored for durable settings at end of replay|
|Old rollout with user messages but no `TurnContext`|History restored; metadata remains previous/unknown; next turn safely reinjects if needed|
|Replacement compaction followed by adjacent `TurnContext`|Reference baseline restored; previous settings unchanged|
|Replacement compaction without `TurnContext`|Reference cleared|
|Legacy compaction without replacement|Rebuilt without current context; reference cleared|
|Continuation records without a new user|No new turn checkpoint|
|Rollback crossing replacement-history base|History still rolled back; uncertain metadata cleared|

## 9. Non-goals

- Adding persisted lifecycle turn IDs in the mandatory patch.
- Porting upstream reverse/lazy rollout readers.
- Changing response item schemas.
- Replacing the branch's `/pause` or preserved-work-notes workflows.
- Changing compaction summary selection.
- Making WebSocket continuation cross logical turns.
- Porting body-after-prefix compaction accounting.
- Changing normal rollout output persistence or resume-time retruncation semantics.
- Making invalid-image recovery rewrite already persisted rollout records.
