# Upstream Turn and History Audit

## Scope and comparison method

The comparison focuses on behavior, not textual similarity. The upstream branch has substantial protocol, session, compaction-window, hook, plugin, multi-agent, and client refactors that are not prerequisites for the branch-local fixes.

The most relevant upstream code is:

- `core/src/session/turn.rs`
- `core/src/context_manager/history.rs`
- `core/src/session/rollout_reconstruction.rs`
- `core/src/session/rollout_reconstruction_tests.rs`
- `core/src/session/mod.rs`
- `protocol/src/protocol.rs`
- `protocol/src/openai_models.rs`

## 1. Behavior already present in this branch

These were once plausible backport candidates, but current inspection shows they are already implemented.

|Behavior|This-branch evidence|Decision|
|---|---|---|
|Previous larger-model compaction before downshift|`maybe_run_previous_model_inline_compact` in `core/src/session/turn.rs`|No action|
|Fresh explicit input sampled before queued steer|`can_drain_pending_input = input.is_empty()` and follow-up gating|No action|
|Post-model-tail token estimates|`ContextManager::get_total_token_usage`|No action|
|Text-only model image stripping|`normalize::strip_images_when_unsupported`|No action|
|Replacement history persisted for local/remote compaction|`CompactedItem.replacement_history` and `replace_compacted_history`|No action|
|WebSocket continuation cleared after compaction|`clear_websocket_continuation` and turn-loop reset handling|No action|
|Reference context item persisted and restored|`TurnContextItem`, `reference_context_item`, rollout reconstruction|Keep, fix semantics|
|Preserved work notes are contextual, not real user turns|`is_contextual_user_message_content`|No action|
|Continuation without a new user item|`continue_turn`, history/rollout cleanup|Preserve exactly|
|Normal rollout stores original tool outputs and reconstruction reapplies the active truncation policy|`record_conversation_items` persists the original input slice; replay calls `record_items`|Shared behavior; not an upstream fix|

## 2. Features added by the upstream branch

These are genuine upstream additions, but not all are suitable for the minimal backport.

|Feature|Upstream behavior|Minimal-backport decision|
|---|---|---|
|Persisted lifecycle turn IDs|`TurnStarted`, `TurnComplete`, `TurnAborted`, and `TurnContextItem` can be associated by turn ID|Defer; current rollout policy does not persist start/complete boundaries|
|Reverse segmented rollout reconstruction|Replays newest-to-oldest, skips rolled-back real user lifecycle segments, then materializes only the surviving suffix|Do not transplant; this branch's existing rollback unit is each non-contextual user `ResponseItem`, so reproduce the consistency invariant with a forward branch-local state machine|
|Reshaped `TurnContextItem` schema|Adds turn ID, workspace/date/timezone, permission/network, `comp_hash`, multi-agent, and realtime fields while removing several branch-specific snapshot fields|Do not transplant; add only a field with a concrete branch-local consumer, such as conditional `comp_hash`|
|Compaction compatibility hash|Compacts with the previous model when both models supply different `comp_hash` values, even when the slug/context window test would not trigger|Conditional follow-up|
|`AutoCompactTokenLimitScope::BodyAfterPrefix`|Budgets growth after an established cached prefix while separately enforcing the full context window|Defer; requires config, window-prefill, token-tail, and compaction-window state|
|Auto-compaction window number/ID|Persists compaction window metadata and can explicitly start a new window|Defer|
|History versioning|Bumps a version on whole-history rewrites for newer prompt/guardian cursors|Defer; no current consumer|
|Typed inter-agent history|Treats persisted inter-agent communication as model input and user-turn-equivalent replay segments|Not applicable to this branch's delegate protocol|
|Realtime transition settings|Tracks realtime active state in previous settings and context updates|Not applicable without realtime support|
|Expanded response protocol|Tool search, image generation, encrypted outputs, new compaction items, image detail, and related normalization|Defer unless independently required|
|More accurate image estimates|Decodes image dimensions and caches estimates, including original-detail behavior|Lower priority; current protocol and fixed resized-image estimate are sufficient for this backport|

## 3. Upstream bug fixes still needed in this branch

### A. Rollback couples history and metadata to the same user-turn segments

Upstream reconstruction identifies real user lifecycle segments and applies `ThreadRolledBack` by skipping those segments. `previous_turn_settings`, reference context, replacement-history checkpoint, and compaction metadata are selected from the same surviving segments.

The minimal backport should not copy that exact counting unit: without durable lifecycle IDs, this branch already defines rollback through `ContextManager::drop_last_n_user_turns`, which counts non-contextual user `ResponseItem`s. The portable bug fix is the invariant that history and metadata use the same branch-local boundary population.

This branch instead:

```text
history.drop_last_n_user_turns(N)
context_stack.truncate(context_stack.len() - N)
```

The two structures count different things. A `TurnContext` can be emitted by an incomplete turn or a compaction task without a real user boundary. After such records, rollback can preserve one history turn while selecting metadata from another.

**Classification:** correctness bug; backport now.

### B. Bare `TurnContext` does not hydrate durable settings

Upstream explicitly tests that a `TurnContext` unaccompanied by a real user turn does not become `previous_turn_settings` or the durable reference baseline.

This branch pushes every `TurnContext` onto `context_stack` and unconditionally selects the last one at EOF. The normal live path can persist the record before the user item, and compaction tasks also emit a pre-compaction `TurnContext`.

**Classification:** correctness bug; backport now.

### C. Legacy compaction does not inject current resume-time initial context into historical history

For a legacy `CompactedItem` without `replacement_history`, upstream rebuilds using:

```rust
compact::build_compacted_history(Vec::new(), &user_messages, &compacted.message)
```

It also clears the reference baseline so the next regular turn injects canonical current context at the end of the resumed history.

This branch calls `build_initial_context(turn_context)` while reconstructing the historical compaction point. Resume-time policy, working directory, instructions, personality, or other settings can therefore be inserted into an old position and produce a non-deterministic prompt.

**Classification:** correctness and prompt-shape bug; backport now.

### D. Previous-turn settings are committed at the real user boundary and are independent of the reference baseline

Upstream records context updates first, records accepted input, and only then sets `previous_turn_settings`. Its compacted-history replacement updates history/reference state without deriving previous settings from the compacted reference item.

This branch sets previous settings before the user item and resets them inside `replace_compacted_history` from `reference_context_item`. A `DoNotInject` compaction therefore clears knowledge of the latest real user model even though compaction did not roll back that user turn.

**Classification:** correctness bug affecting downshift compaction and resume metadata; backport now.

### E. Invalid-image recovery stops at a real user boundary

Upstream's `replace_last_turn_images` reverse scan uses `is_user_turn_boundary(item)`. This branch stops at every user-role message. A contextual user item after a tool image can prevent sanitization of the image that actually belongs to the current real turn.

**Classification:** narrow correctness bug; backport now.

## 4. Upstream chores not needed for this branch

|Upstream change|Why it is not a prerequisite here|
|---|---|
|Move truncation helpers to `codex_utils_output_truncation`|Source-layout migration; current helpers already implement required behavior|
|`ContextManager::history_version`|Serves newer prompt/guardian cursor architecture absent here|
|`ContextManager::into_raw_items`|Convenience API, not a semantic requirement|
|Mixed initial-context developer-bundle rollback repair|Upstream can place contextual and non-contextual fragments in one developer message; this branch emits separate messages from `build_initial_context`|
|New tool-search/image-generation normalization arms|Variants do not exist in this branch's current protocol|
|Typed inter-agent boundary rules|This branch's delegate workflow has different persisted shapes|
|Realtime transition fields|No matching runtime feature|
|Remote compaction v2 and compaction-window plumbing|Independent upstream architecture|
|Hook/plugin/session-start integration|Not required to fix turn/history replay|
|Item-ID assignment during replacement history|Independent protocol/client feature|
|Eliding an unchanged `TurnContextItem`|Upstream can return early when the full snapshot equals the reference; this branch's per-turn record is useful to the minimal ID-free association strategy and should remain|

## 5. Classification summary

|Category|Backport now|Conditional|Defer/not applicable|
|---|---|---|---|
|Upstream features|None required for core fix|`comp_hash` compatibility trigger|Lifecycle IDs, reverse lazy replay, body-after-prefix scope, window IDs, history version, realtime, typed inter-agent|
|Upstream bug fixes|Replay alignment, bare context, legacy compaction, previous-settings timing/decoupling, image boundary|None|Mixed developer-bundle repair is structurally unnecessary here|
|Upstream chores|None|None|Shared-crate moves, protocol expansion, hooks/plugins, remote compaction v2|
