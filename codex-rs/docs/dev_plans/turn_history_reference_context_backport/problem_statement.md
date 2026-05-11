# Turn History Reference Context Backport - Problem Statement

## Target Base

This proposal targets this project's current branch.

The relevant current-branch files are:

- `core/src/session/turn.rs`
- `core/src/session/mod.rs`
- `core/src/session/handlers.rs`
- `core/src/session/turn_context.rs`
- `core/src/context_manager/history.rs`
- `core/src/context_manager/normalize.rs`
- `core/src/compact.rs`
- `core/src/compact_remote.rs`
- `core/src/state/session.rs`
- `protocol/src/protocol.rs`

The upstream project is used as a behavioral reference. This plan intentionally avoids porting the upstream task/session architecture wholesale.

## Current Branch Inspection Summary

### Fixes already present

The current branch already contains several high-value fixes from the upstream turn/history research.

|Previously identified item|Evidence in this branch|Backport decision|
|---|---|---|
|Token accounting includes local items after the last model response.|`ContextManager` stores per-item estimates and `get_total_token_usage()` includes items after the last model-generated item.|No action. Keep existing tests around post-model tail accounting.|
|Prompt history is modality-aware and strips images for text-only models.|`prepare_items_for_prompt_with_modalities()` normalizes history, strips unsupported images, then drops `GhostSnapshot` items.|No action.|
|Base64 image payloads do not dominate approximate token estimates.|`estimate_response_item_model_visible_bytes()` discounts inline image data URLs to a fixed resized-image estimate.|No action for MVP. Upstream's dynamic original-detail estimate is not directly applicable because this branch's protocol has no `ImageDetail` field.|
|Rollback removes contextual setup items immediately preceding the rolled-back turn.|`drop_last_n_user_turns()` calls `trim_pre_turn_context_updates()`.|Partial. Keep this behavior, but also recompute durable baseline metadata.|
|Compaction persists replacement history.|`CompactedItem` has `replacement_history`; local and remote compaction persist replacements.|No action, but integrate with reference-context state.|
|Compaction resets the turn-scoped WebSocket state.|Callers receive a compacted boolean and call `ModelClientSession::reset_websocket_session()` when the history window was rewritten.|No action.|
|Compaction failure is not silently swallowed.|Auto-compaction errors return from the turn path instead of continuing against over-limit history.|No action.|

### Remaining correctness and performance gaps

The remaining high-value backport is centered on durable context baselines rather than raw history storage.

#### 1. Context diffing is based on an in-memory `previous_context`, not a persisted baseline

`core/src/session/handlers.rs` keeps an in-memory `previous_context: Option<Arc<TurnContext>>` and passes it to `Session::build_settings_update_items(...)`.

This works for a simple live session, but it is not a durable invariant:

- resume/fork/rollback reconstruct raw history but do not reconstruct a stable context-diff baseline;
- `/rollback` mutates in-memory history and persists `ThreadRolledBack`, but the handler's `previous_context` remains whatever it was before rollback;
- settings diffs can therefore be compared against a context that is no longer the last surviving model-visible context;
- when the branch cannot safely diff, it relies on initial-context reseeding, which increases prompt churn and harms prefix-cache stability.

The upstream project fixes this by persisting one `RolloutItem::TurnContext(TurnContextItem)` per real user turn and keeping a `reference_context_item` as the baseline used for future diffs.

#### 2. Rollout reconstruction does not return metadata needed by later turns

`Session::reconstruct_history_from_rollout(...)` currently returns only `Vec<ResponseItem>`.

It does replay `ResponseItem`, `CompactedItem.replacement_history`, and `ThreadRolledBack`, but it does not return:

- the latest surviving turn-context baseline;
- previous-turn settings such as the last surviving model;
- whether a legacy compaction or rollback invalidated that baseline.

This leaves model-switch warnings and later settings updates dependent on separate one-shot helpers such as `last_model_name(...)` and `pending_resume_previous_model`, instead of one canonical replay result.

#### 3. Model-downshift compaction cannot use the previous, larger model

`run_turn_inner(...)` checks the current model's auto-compact limit before sampling. If the user switches from a larger-context model to a smaller-context model, the current branch attempts pre-turn compaction using the new turn context.

That loses the key upstream behavior:

- if the existing history fits the previous model but exceeds the new model's limit, compact with the previous model before sampling with the smaller model;
- do it before recording current-turn context diffs and user input;
- then resume normal turn flow with a compacted history that fits the smaller model.

Without this, model downshifts can fail at exactly the point where compaction would have avoided an over-limit request.

#### 4. Pending input can be merged into the first sampling request of a fresh prompt

Inside the sampling loop, this branch drains `sess.get_pending_input()` whenever pre-compact work-note capture is idle. That means a fresh explicit user prompt and queued pending input can be recorded before the first model request of the turn.

The upstream project uses a narrower rule:

- if the turn starts with explicit input, sample that input first;
- after a successful model response, pending input can drive a follow-up;
- after mid-turn compaction, drain pending input only when the model does not still need a follow-up.

This preserves the user's first prompt shape and makes prompt-cache behavior more deterministic.

#### 5. Preserved work-notes are persisted as user messages but are not user turns

This branch adds preserved work-notes around auto-compaction. `preserved_work_notes_message(...)` currently creates a `role: "user"` message whose text starts with the preserved-work-notes prefix.

That message is intentionally model-visible session state. It should not count as:

- a real user turn boundary for rollback;
- a real user message selected by compaction;
- a user message that triggers continuation semantics.

`collect_user_messages(...)` filters preserved work-notes, but `is_user_turn_boundary(...)` currently only excludes contextual user-instruction/session-prefix/shell-command messages. The backport should make work-notes classification explicit so the reference-context work does not inherit a hidden rollback bug.

## Why This Matters

|Impact area|Failure mode without the backport|Expected improvement|
|---|---|---|
|Correctness after rollback|Settings diffs can be based on a stale in-memory context that survived the rollback.|Rollback replay restores both history and the baseline used for the next turn.|
|Correctness after resume/fork|The branch can warn about model mismatch but cannot reliably reconstruct all baseline metadata for future diffs.|Resume/fork reconstructs history plus previous-turn settings from the same replay path.|
|Model downshift|The branch may ask the smaller model to compact history that only the previous larger model can handle.|Pre-sampling compaction uses the previous larger model, then samples the new model against compacted history.|
|Prompt-cache stability|Duplicated or reordered context messages churn the stable prefix.|Full context is injected only when the durable baseline is missing; otherwise only diffs are appended.|
|Rollback semantics with work notes|Preserved notes can be counted as a user turn boundary.|Work notes are treated as contextual state.|
|Pending-input ordering|Queued input can change the first model request for a fresh explicit prompt.|The first sampling request remains scoped to the explicit prompt.|

## Non-Goals

Do not include these in the MVP:

- porting upstream `RegularTask::run` or the full task/session module split;
- porting upstream `ToolSearchCall` / `ToolSearchOutput` normalization when this branch does not expose those variants;
- porting upstream inter-agent assistant-turn boundaries unless this branch adopts the same persisted inter-agent instruction protocol;
- porting remote compaction v2;
- adding cross-turn WebSocket prewarm/reuse beyond the already planned WebSocket-specific backport work;
- replacing this branch's preserved work-notes workflow.

## Success Criteria

The backport is successful when:

1. A normal real user turn persists one `TurnContextItem` baseline even when no model-visible settings diffs are emitted.
2. Resume, fork, compaction replay, and rollback reconstruct the same prompt history and the same reference-context baseline.
3. A legacy compaction without `replacement_history` clears the baseline so the next real user turn reinjects full context.
4. A pre-turn model downshift from a larger-context model to a smaller-context model compacts with the previous model before sending any request to the smaller model.
5. A fresh explicit user prompt is sampled before pending input is drained.
6. Preserved work-notes are not counted as user turn boundaries.
