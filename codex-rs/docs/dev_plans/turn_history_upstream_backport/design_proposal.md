# Design Proposal

## Status

Implemented in this branch. The design remains the durable reference for the replay state machine and selected non-goals.

## 1. Design objective

Repair replay semantics with the smallest branch-appropriate change:

- no new persisted lifecycle events;
- no rollout schema migration;
- no change to normal live-turn ordering;
- no upstream window/realtime/inter-agent prerequisites;
- preserve branch-local `/pause`, `/continue`, work notes, and `GhostSnapshot` behavior.

The replay implementation must stop treating `TurnContext` as a user-turn counter. It should instead treat it as a candidate that becomes committed only at a real user boundary, except for the explicit post-compaction baseline record.

## 2. Target metadata model

Introduce internal replay-only state in `core/src/session/rollout_reconstruction.rs`.

```rust
#[derive(Clone)]
struct ReplayMetadata {
    reference_context_item: Option<TurnContextItem>,
    previous_turn_settings: Option<PreviousTurnSettings>,
}

#[derive(Clone)]
struct MetadataCheckpoint {
    after_user_boundary: ReplayMetadata,
}

struct ReplayEpoch {
    base_metadata: ReplayMetadata,
    checkpoints: Vec<MetadataCheckpoint>,
    replacement_is_opaque: bool,
}
```

The exact names are not important. The invariants are:

- `pending_context` is not committed metadata;
- every checkpoint corresponds to one `is_user_turn_boundary` observed after the current opaque replacement base;
- `base_metadata` is the state at the latest compaction/rewrite epoch;
- previous settings and reference context are stored independently.

## 3. Replay state

The forward scan should maintain:

```text
history
current_metadata
pending_context
current_epoch
awaiting_adjacent_post_compact_context
latest_compaction_tail_kind
explicit_interrupt_hint
```

Suggested compaction-tail classification:

```rust
enum CompactionTailKind {
    None,
    StandaloneOrPreTurn,
    MidTurnWithInjectedContext,
}
```

This is internal provenance, not a persisted protocol field.

## 4. Event semantics

### 4.1 `RolloutItem::ResponseItem`

Always feed the item through `ContextManager::record_items`, preserving existing filtering and truncation.

If `is_user_turn_boundary(response_item)` is false:

- do not commit `pending_context`;
- do not push a metadata checkpoint;
- contextual user messages, preserved work notes, developer updates, assistant output, reasoning, calls, outputs, and `GhostSnapshot` remain non-boundaries.

If it is true:

1. If `pending_context` exists, set:

   ```text
   current.reference_context_item = pending_context
   current.previous_turn_settings.model = pending_context.model
   ```

2. If no pending context exists, carry current metadata forward. This covers queued steering within an already committed turn context and legacy/malformed records without context evidence.
3. Push a checkpoint containing the resulting metadata.
4. Clear `pending_context`.
5. Mark that real user work occurred after the latest compaction.
6. Clear any older interruption hint that predates this user boundary.

The `ResponseItem` is the commitment evidence because live code persists it before committing `previous_turn_settings` and before emitting `UserMessage`. A crash between those steps must still reconstruct the accepted user boundary.

### 4.2 Ordinary `RolloutItem::TurnContext`

When the item is not immediately adjacent to a `Compacted` record:

```text
pending_context = item
```

Do not immediately update current metadata. If another ordinary context appears before a real user boundary, the newest candidate wins. This matches the latest context that would have applied to that user item while preventing task-only or cancelled contexts from becoming previous settings.

### 4.3 Adjacent post-compaction `TurnContext`

`replace_compacted_history` persists `Compacted` and the optional reference context in one ordered batch. Therefore a `TurnContext` immediately following `Compacted` with no intervening rollout item has distinct semantics:

- canonical context was inserted into the replacement history;
- set `current.reference_context_item` to that item;
- update `current_epoch.base_metadata.reference_context_item` to the same item;
- do **not** change `previous_turn_settings`;
- do not push a user checkpoint;
- classify the compaction tail as `MidTurnWithInjectedContext`.

Any intervening rollout record cancels the adjacency interpretation. A standalone/pre-turn compaction followed by a later normal user turn is therefore not ambiguous: the normal turn appends context response items before its `TurnContext`, so replay treats that `TurnContext` as a pending candidate for the later user boundary.

### 4.4 `RolloutItem::Compacted`

History:

- if `replacement_history` exists, replace history exactly;
- otherwise collect historical user messages and rebuild with `Vec::new()` initial context plus the persisted summary.

Metadata:

```text
current.reference_context_item = None
current.previous_turn_settings = previous committed value
pending_context = None
current_epoch.base_metadata = current
current_epoch.checkpoints.clear()
current_epoch.replacement_is_opaque = true
awaiting_adjacent_post_compact_context = true
latest_compaction_tail_kind = StandaloneOrPreTurn
```

The replacement is opaque because its retained user messages and summary do not provide a bijection to original metadata checkpoints. This is why rollback can be exact only for user boundaries appended after that base.

### 4.5 `ThreadRolledBack(N)`

First call the existing history operation:

```rust
history.drop_last_n_user_turns(N);
```

Then update metadata using the post-base checkpoint count.

#### Rollback contained within current epoch

When `N <= checkpoints.len()`:

1. Remove the newest `N` checkpoints.
2. Restore current metadata from the new last checkpoint, or from `base_metadata` when no checkpoint remains.
3. Clear `pending_context` and interruption hints.

History and metadata now remove the same post-base user boundaries.

#### Rollback crosses the opaque replacement base

When `N > checkpoints.len()` and the epoch has an opaque replacement:

- history has removed one or more user boundaries represented only inside replacement history;
- exact historical metadata cannot be recovered from this branch's existing rollout schema;
- clear both reference context and previous settings conservatively;
- clear checkpoints and pending context;
- treat the surviving rewritten history as a new opaque epoch.

Retaining a guessed value is worse than appending canonical context and re-establishing metadata on the next real user turn.

#### Rollback with no compaction base

When no opaque base exists and `N` removes all known checkpoints, restore the initial empty metadata.

### 4.6 Interruption and user events

`EventMsg::TurnAborted(Interrupted)` records an interruption hint. `EventMsg::UserMessage` may clear an older hint, but it must not be required to commit metadata because it can be missing after a crash that occurred after the user response item was persisted.

Other event messages do not affect replay metadata unless already handled by existing history/continuation logic.

## 5. Continuation derivation

Keep `history_needs_continuation` as the raw-history predicate. Add replay provenance around it.

```text
incomplete = history_needs_continuation(reconstructed_history)
standalone_compaction_only =
    latest compaction tail is StandaloneOrPreTurn
    && no real user boundary was appended after that compaction

pending = incomplete && !standalone_compaction_only
```

When pending:

- source remains `Interrupted` for compatibility with the branch-local continuation type;
- model comes from `current_metadata.previous_turn_settings`, not from the reference baseline;
- target remains `Regular`;
- explicit interruption evidence can determine source priority but must still satisfy the incomplete-history check.

This yields the required cases:

|Tail|Result|
|---|---|
|Ordinary user with no final assistant|Pending continuation.|
|Interrupted dangling call|Pending continuation after cleanup.|
|Standalone/manual compaction summary only|No regular continuation.|
|Pre-turn compaction with no later user|No regular continuation.|
|Mid-turn compaction plus adjacent context and no later assistant|Pending continuation.|
|Any tail ending in a final assistant message|No continuation.|

## 6. Legacy compaction behavior

For `replacement_history: None`, reconstruct only evidence known at the historical point:

```rust
let rebuilt = compact::build_compacted_history(
    Vec::new(),
    &user_messages,
    &compacted.message,
);
```

Then:

- clear reference context;
- preserve committed previous settings;
- allow the next normal turn to append current canonical context at the end;
- do not try to synthesize a historical `TurnContext` from the resume-time `TurnContext`.

This matches upstream's deterministic fallback while avoiding its broader replay infrastructure.

## 7. Why not persist lifecycle events in the mandatory patch

Persisted lifecycle IDs would produce a cleaner long-term model, but adding them here is not a narrow prerequisite. It would require defining lifecycle semantics for:

- explicit user turns with queued steering;
- standalone local and remote compaction;
- inline auto-compaction before and during turns;
- `/pause` and `/continue`;
- post-turn review and delegate workflows;
- interrupted tasks and old rollouts without IDs.

The proposed checkpoint design uses evidence already persisted and directly matches this branch's existing history rollback unit.

## 8. Prefix-cache properties of the design

The design does not alter ordinary prompt construction. It improves cache behavior indirectly:

- a valid post-compaction reference avoids redundant full-context reinjection;
- clearing uncertain metadata after an opaque rollback avoids emitting a wrong delta against a stale baseline;
- deterministic legacy reconstruction prevents resume-time settings from changing an old prefix;
- false compaction continuation requests are eliminated.

Compaction and rollback remain intentional rebases. No design should claim to preserve the pre-rewrite exact prefix.

## 9. Compatibility

- Existing rollout JSON remains valid.
- No new required field is introduced.
- Old replacement-bearing compactions become more accurately replayed.
- Legacy replacement-less compactions change reconstructed prompt shape intentionally by removing historically inaccurate current context.
- In-memory live behavior remains unchanged.
- `/continue` continues to operate on completed durable items only.

## 10. Failure policy

When evidence is ambiguous, prefer conservative metadata clearing over a stale association. The next normal turn can safely append a full canonical context and commit new previous settings after its user boundary. This costs tokens once but preserves correctness.
