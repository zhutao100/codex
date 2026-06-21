# Problem Statement

## 1. Required invariant

Resume, fork, live rollback, and continuation must reconstruct metadata from the same surviving instruction boundaries as history:

```text
history boundary population == metadata checkpoint population
```

The metadata values are not interchangeable:

```text
reference_context_item
    baseline represented in surviving history for future context diffs

previous_turn_settings
    settings of the newest surviving committed real user turn
```

A compaction can clear `reference_context_item` while preserving `previous_turn_settings`.

## 2. Root cause

`core/src/session/rollout_reconstruction.rs` currently keeps:

```rust
let mut history = ContextManager::new();
let mut context_stack = Vec::<TurnContextItem>::new();
```

It appends every persisted `TurnContext` to `context_stack`. On rollback it asks history to remove `N` real user turns, then removes `N` context records:

```rust
history.drop_last_n_user_turns(rollback.num_turns);
context_stack.truncate(context_stack.len().saturating_sub(turns_to_drop));
```

These are different units. A `TurnContext` can be emitted by an incomplete normal turn, a compaction task, or a context injection adjacent to a replacement history. None necessarily creates a new real user boundary.

At end of replay the last stack item is used for both reference context and previous settings. That collapses two state variables with different lifetimes.

## 3. Defect scenarios

### 3.1 Bare context becomes a completed user turn

A normal turn persists context before recording explicit user input:

```text
context update ResponseItems
TurnContext(model=B)
<dependency resolution, cancellation, or failure>
end of rollout
```

Current replay hydrates:

```text
reference_context_item = B
previous_turn_settings = B
```

No real user item was committed under model B. Upstream replay treats the context as a candidate attached to a non-user segment and does not use it as previous settings.

### 3.2 Rollback removes history and metadata from different turns

```text
TurnContext(A)
user("first")
assistant("first reply")
TurnContext(X)                  # next task/turn never records a user boundary
ThreadRolledBack(1)
```

History removes the only real user turn. `context_stack` removes X and leaves A. The reconstructed metadata claims A survives even though its user turn was rolled back.

A compaction task can create the opposite skew by adding context records that are later cleared at a compaction checkpoint. Numeric stack truncation has no stable relation to the user boundaries in replacement history.

### 3.3 Manual or pre-turn compaction loses the previous model

Live state before compaction:

```text
previous_turn_settings = model A
reference_context_item = context A
```

A standalone/manual or pre-turn compaction uses `InitialContextInjection::DoNotInject` and persists:

```text
TurnContext(compaction task)    # local path only
Compacted(replacement_history)
```

Live `replace_compacted_history` correctly produces:

```text
previous_turn_settings = model A
reference_context_item = None
```

Replay clears `context_stack` at `Compacted`, so both values become `None`. A later switch to a smaller model can skip the previous-model compaction decision because the model that produced the surviving compacted conversation was forgotten.

### 3.4 Post-compaction reference context is mistaken for previous settings

Mid-turn compaction with `BeforeLastUserMessage` persists an exact replacement followed immediately by `TurnContext(current)`:

```text
Compacted(replacement_history)
TurnContext(current)
```

The adjacent context says canonical current context was inserted into the replacement. It re-establishes `reference_context_item`. It does not represent a new user turn and must not overwrite `previous_turn_settings` by itself.

Current replay uses it for both values.

### 3.5 Legacy compaction injects current context into historical history

For `replacement_history: None`, this branch calls:

```rust
compact::build_compacted_history(
    self.build_initial_context(turn_context).await,
    &user_messages,
    &compacted.message,
)
```

The supplied `turn_context` is the resume/fork context, not the historical compaction context. A changed cwd, shell, sandbox policy, approval policy, collaboration mode, personality, or instruction set can therefore appear before older surviving items.

This is historically incorrect and changes a long prompt prefix based on when the thread is resumed.

### 3.6 Continuation model follows the wrong metadata value

Current replay builds `PendingContinuation.model` from `context_stack.last()`, which is also the reconstructed reference baseline. After compaction, the reference can be intentionally absent or can be re-established by a context-only record. The continuation model should instead come from the newest committed real user turn.

### 3.7 Standalone compaction can look like an interrupted regular turn

A replacement history commonly ends with the generated summary encoded as a non-contextual user message. `history_needs_continuation` sees an empty tail after that user boundary and returns `true`.

For mid-turn automatic compaction, that inference can be useful if the process stops before the next assistant item. For a completed standalone/manual compaction, it is a false positive: there was no regular model turn to continue.

The rollout already exposes a minimal discriminator:

- `Compacted` immediately followed by `TurnContext` indicates canonical context was inserted for an active mid-turn replacement;
- `Compacted` with no adjacent post-compaction context is the standalone/pre-turn shape in this branch.

## 4. Cache consequences

The defects are correctness problems first. They can also waste prefix reuse:

- a missing reference baseline causes canonical context to be appended again;
- a stale reference baseline can produce an incorrect delta/full-context choice;
- legacy resume-time context changes history at an old compaction point;
- a wrong previous model can choose an inappropriate compaction path;
- a false continuation can issue an unnecessary request from a synthetic summary tail.

The target is not to keep the pre-compaction prefix. Compaction intentionally rebases history. The target is deterministic reconstruction of the chosen base and append-only behavior thereafter.

## 5. Non-defects

The minimal patch must not “fix” behavior that is already correct:

- live context recording does not commit previous settings;
- user recording commits previous settings after the user `ResponseItem` is durable;
- live compaction preserves previous settings when clearing reference context;
- invalid-image recovery already stops at `is_user_turn_boundary`;
- prompt normalization works on a clone;
- fresh explicit input is sampled before queued steering;
- new compactions persist exact replacement history.

## 6. Constraints

The solution must preserve:

- the existing rollout/protocol schema for the mandatory patch;
- readability of old rollouts;
- local and remote compaction;
- `/pause` and `/continue` without a synthetic user message;
- preserved work notes as contextual state, not a rollback boundary;
- `GhostSnapshot` durability and prompt omission;
- current tool-output truncation behavior;
- current full-history request construction and turn-scoped WebSocket reuse.

## 7. Success criteria

1. A normal `TurnContext` becomes committed metadata only when a real user boundary is recorded.
2. Contextual user items, work notes, and non-user tasks never create metadata checkpoints.
3. Rollback removes the same number of replay checkpoints as real user boundaries removed from post-compaction history.
4. A compaction preserves previous settings but clears reference context unless an adjacent post-compaction context explicitly re-establishes it.
5. Crossing an opaque replacement-history base clears uncertain metadata rather than retaining a stale value.
6. Legacy compaction does not insert resume-time initial context at the historical checkpoint.
7. Pending continuation uses committed previous settings for its model.
8. A standalone/pre-turn compaction summary alone does not create a regular pending continuation.
9. Mid-turn compaction can still be recognized as resumable when no later assistant completion survives.
10. Ordinary prompt-prefix tests remain unchanged.
