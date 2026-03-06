# Task

Prepare a drop-in design for carrying forward structured session work notes before auto-compaction.

## Target Base

This plan is written against branch base `fe8b474acd43cb8894d661944c6b5c8db0ef0ad1`.

The relevant surfaces on this base are:

- `core/src/codex.rs`
- `core/src/compact.rs`
- `core/src/compact_remote.rs`
- `core/src/tools/parallel.rs`

## Context

- `auto_compact_limit` is conservative. In practice, `token_limit_reached` means "compaction should happen soon" rather than "the next request is guaranteed to fail."
- The current compaction flow preserves user-visible anchors reasonably well, but it is lossy for the model's in-flight working state.
- The most expensive loss cases are:
  - the model has spent most of the current session doing analysis, validation, elimination, and planning, but has not yet reached a final conclusion.
  - the model has already inspected many files and has learned which files are relevant, irrelevant, or only need a brief reminder instead of a full reread.
  - the model is in the middle of edits, with partially validated hypotheses or partially staged code changes.
- A lossy compact can cause the next session to:
  - re-run hypotheses that were already validated or ruled out.
  - re-open files that were already classified as irrelevant.
  - lose edit intent, edit ordering, or the next concrete step.

## Current Behavior On This Base

### Turn loop

On `fe8b474...`, `run_turn(...)` already has several important behaviors that the design must respect:

- it creates one turn-scoped `ModelClientSession` and reuses it across sampling retries.
- it resolves `turn_metadata_header` once and threads it through sampling requests.
- each loop drains pending input via `sess.get_pending_input()`, replays the drained items into history, and only then builds `sess.clone_history().await.for_prompt()`.
- the post-sampling auto-compact trigger is still the immediate branch:

```rust
if token_limit_reached && needs_follow_up {
    run_auto_compact(&sess, &turn_context).await;
    continue;
}
```

### Local compaction

`core/src/compact.rs`:

- appends a synthetic compact prompt to cloned history.
- runs a separate compacting model pass using the same session-scoped model client stack.
- rebuilds history as:
  - initial context
  - a token-limited subset of collected user messages
  - one summary user message
  - ghost snapshots
- persists `CompactedItem { message, replacement_history: None }` for the local compaction path.

### Remote compaction

`core/src/compact_remote.rs`:

- trims codex-generated tail items to fit the context window using `base_instructions`.
- sends `history.for_prompt()` to `compact_conversation_history(...)`.
- replaces history with the returned transcript plus ghost snapshots.
- persists `replacement_history: Some(new_history)` for remote compaction.

## Objective

When follow-up work is still needed and the token budget is approaching compaction:

- run one additional round inside the same session to generate structured session work notes.
- preserve the prompt prefix as much as possible so the extra round can continue to benefit from prefix caching.
- prevent the note round from causing tool side effects.
- compact from the original work history rather than from the transient note-request round.
- inject the notes verbatim into the post-compaction transcript so the next session inherits them without re-compression.

## Design Constraints

- Keep the change as a drop-in diff on top of the target base.
- Preserve the existing prompt surface for the note round:
  - do not remove tools from the prompt.
  - do not swap base instructions.
  - do not introduce a separate model path just for note capture.
- Avoid broad protocol or schema changes.
- Respect existing `final_output_json_schema` behavior. If a free-form note round would conflict with schema-constrained output, the design must degrade safely.
- Respect the base's pending-input drain/replay behavior. If the user interrupts while notes are being captured, that input must not be lost.
- Keep the change localized so it can be rebased repeatedly onto newer upstream code with minimal conflict pressure.

## Rebase-Sensitive Surfaces On This Base

These are the places most likely to drift on future bases and should be checked first during a rebase:

- `run_turn(...)` in `core/src/codex.rs`
  - ownership of `client_session`
  - ownership of `turn_metadata_header`
  - the pending-input replay logic
  - the exact location of the `token_limit_reached && needs_follow_up` branch
- `run_sampling_request(...)` and `try_run_sampling_request(...)`
  - parameter lists
  - where `ToolCallRuntime` is created
- `ToolCallRuntime::handle_tool_call(...)`
  - return type shape
  - the structure of `FunctionCallOutputPayload`
- `run_compact_task_inner(...)` in `core/src/compact.rs`
  - how history is cloned, summarized, and reconstructed
  - whether local compaction still relies on `replacement_history: None`
- `run_remote_compact_task_inner_impl(...)` in `core/src/compact_remote.rs`
  - whether it still trims against `base_instructions`
  - where ghost snapshots are appended

## Success Criteria

The design is successful if it does all of the following:

- preserves the original context and loss-avoidance goals.
- reads as a standalone plan for the `fe8b474...` code shape.
- remains useful as a future rebase reference by documenting the base-specific drift points and invariants, not just the idealized end state.
