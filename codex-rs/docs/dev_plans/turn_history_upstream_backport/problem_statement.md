# Problem Statement

## Goal

Make resume, fork, rollback, compaction, and continuation reconstruct one coherent pair:

```text
(model-visible history, metadata describing that same surviving history)
```

The current raw-history implementation is generally sound. The remaining defect is metadata association: rollout reconstruction counts `TurnContext` records independently from real user turns.

## Defect 1: history and context metadata use independent rollback counters

Current reconstruction in `core/src/session/rollout_reconstruction.rs` maintains:

```rust
let mut history = ContextManager::new();
let mut context_stack = Vec::<TurnContextItem>::new();
```

A rollback applies:

```rust
history.drop_last_n_user_turns(rollback.num_turns);
context_stack.truncate(context_stack.len().saturating_sub(turns_to_drop));
```

`history` counts only `is_user_turn_boundary` messages. `context_stack` counts every persisted `TurnContext`. Those populations are not equivalent.

### Concrete divergences

A bare context candidate can consume the metadata rollback count even though it never created a history turn:

```text
TurnContext(A)
user(A)
assistant(A)
TurnContext(X)       // next turn began, but no real user item was recorded
ThreadRolledBack(1)
```

History removes user turn A. Stack truncation removes only X and leaves A, so reconstruction reports metadata for a turn that no longer survives.

Compaction can fail in the opposite direction:

```text
TurnContext(A)
user(A)
assistant(A)
TurnContext(compaction task)
Compacted(replacement, no adjacent TurnContext)
TurnContext(B)
user(B)
ThreadRolledBack(1)
```

`Compacted` clears `context_stack`. Rolling back B then removes the only remaining stack entry, although the compacted base still represents surviving work from A. The reconstructed history and previous-turn metadata no longer describe the same surviving state.

The invariant is broken in both cases because the same numeric rollback is applied to unlike units.

## Defect 2: a bare `TurnContext` is accepted as a completed turn

The live normal-turn path persists `TurnContext` before it records the explicit user item. Dependency resolution, connector access, cancellation, or another early return can leave:

```text
[context update items]
TurnContext(X)
end of rollout
```

The local compaction task also persists a `TurnContext` before making the compaction request. On resume this branch unconditionally uses `context_stack.last()` for both:

- `reference_context_item`;
- `previous_turn_settings`.

The result can claim model X handled the latest real user turn when no such boundary exists.

## Defect 3: legacy compaction is rebuilt with current settings in a historical position

For a legacy compaction without `replacement_history`, current reconstruction calls:

```rust
compact::build_compacted_history(
    self.build_initial_context(turn_context).await,
    &user_messages,
    &compacted.message,
)
```

`turn_context` is the context at resume/fork time, not necessarily the context at compaction time. This can insert current values for:

- working directory and shell environment;
- sandbox and approval policy;
- developer/user instructions;
- collaboration mode and personality;
- model-related context.

The insertion occurs at the old compaction point, before later surviving items. That is both historically inaccurate and unfriendly to deterministic prompt-prefix reuse.

## Defect 4: previous-turn settings are committed too early and cleared by context-clearing compaction

`record_context_updates_and_set_reference_context_item` currently sets:

```rust
state.previous_turn_settings = Some(PreviousTurnSettings { model: ... });
```

before the real user item is recorded.

`replace_compacted_history` later derives previous settings from the replacement's `reference_context_item`. With `InitialContextInjection::DoNotInject`, the reference is `None`, so previous settings are cleared even though the latest real user turn still exists semantically in the compacted summary/history.

Consequences:

- incomplete turns can become the apparent previous model;
- after resume or compaction, a later model-downshift check can lose or skip the model that should perform compaction;
- resume and rollback can report metadata from a non-user operation;
- context-baseline clearing becomes incorrectly equivalent to forgetting the latest real user model.

## Defect 5: invalid-image recovery uses every user-role item as a boundary

`ContextManager::replace_last_turn_images` searches backward for either a function output or any `role == "user"` message.

This branch intentionally stores contextual user-role items, including user instructions, skill instructions, session prefixes, shell wrappers, and preserved work notes. Such an item is not a new real user turn, but it can stop the scan before the offending tool image is found.

## Prefix-cache impact

These defects are primarily correctness issues, but they also alter cache behavior:

- a missing or stale reference baseline can cause full context to be appended again;
- resume-time context inserted at a historical compaction point changes a long prefix;
- metadata selected from the wrong surviving turn can emit incorrect model-switch/settings deltas;
- unnecessary rewrites or reinjections consume context budget even when an older prefix remains cacheable.

The target is not to preserve every historical token indefinitely. Compaction and rollback are intentional rewrites. The target is to ensure metadata always describes the chosen rewritten history and that ordinary turns remain append-only.

## Constraints

The solution must preserve:

- the existing `RolloutItem` schema for the minimal patch;
- old rollout readability;
- `/pause` and `/continue` without a synthetic user message;
- preserved-work-notes behavior;
- local and remote replacement-history compaction;
- current delegate and post-turn review workflows;
- current full-history request construction and turn-scoped WebSocket reuse.

## Success criteria

1. A `TurnContext` becomes previous-turn settings only when a real user boundary commits it.
2. A `TurnContext` directly attached to compacted replacement history can re-establish the reference baseline without creating a fake user turn.
3. Rollback removes metadata checkpoints corresponding to the same post-checkpoint user turns removed from history.
4. Crossing an opaque compaction checkpoint clears uncertain metadata rather than retaining a stale value.
5. Legacy compaction reconstruction does not inject current initial context into historical history.
6. Compaction can clear the reference baseline while preserving previous-turn settings.
7. Continuation adds no user-turn checkpoint.
8. Invalid-image recovery ignores contextual user-role messages as turn boundaries.
