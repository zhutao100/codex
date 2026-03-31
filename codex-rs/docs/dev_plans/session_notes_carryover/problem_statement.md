# Task

Prepare a drop-in design for carrying forward structured session work notes before auto-compaction.

This dev plan was originally authored for `custom-0.98.0` and is re-introduced on `custom-0.114.0` to guide the port and future rebases.

## Target Base (Original)

The relevant surfaces on that base were:

- `core/src/codex.rs`
- `core/src/compact.rs`
- `core/src/compact_remote.rs`
- `core/src/tools/parallel.rs`

## Target Base (Port)

This port targets `custom-0.114.0`.

The same primary surfaces still apply, plus:

- `core/src/context_manager/history.rs` (`for_prompt` normalizes history and strips ghost snapshots)
- `core/src/compact.rs` (`InitialContextInjection` behavior for pre-turn vs mid-turn compaction)

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

## Rebase-Sensitive Surfaces

These are the places most likely to drift on future bases and should be checked first during a rebase:

- `run_turn(...)` in `core/src/codex.rs`
  - where the post-sampling `token_limit_reached && needs_follow_up` branch lives
  - how pending input is drained and replayed
  - lifetime/ownership of the turn-scoped `ModelClientSession`
- `run_sampling_request(...)` / `try_run_sampling_request(...)` in `core/src/codex.rs`
  - parameter lists (especially anything related to tool runtime construction)
  - where `ToolCallRuntime` is created
- `ToolCallRuntime::handle_tool_call(...)` in `core/src/tools/parallel.rs`
  - response item shapes for `FunctionCallOutput`, `CustomToolCallOutput`, and `McpToolCallOutput`
- `ContextManager::for_prompt(...)` in `core/src/context_manager/history.rs`
  - what is filtered/normalized before sending to the model (notably ghost snapshots)
- `run_compact_task_inner(...)` in `core/src/compact.rs`
  - how history is cloned/trimmed before the compaction-model pass
  - where replacement history is constructed and persisted
- `run_remote_compact_task_inner_impl(...)` in `core/src/compact_remote.rs`
  - the trim-before-submit behavior
  - where initial context injection happens for mid-turn compaction

## Success Criteria

The design is successful if it does all of the following:

- preserves the original context and loss-avoidance goals.
- reads as a standalone plan for the current code shape.
- remains useful as a future rebase reference by documenting the drift points and invariants, not just the idealized end state.

## Implementation Notes (Pitfalls)

- Auto-compaction can trigger multiple times in one session (pre-sampling and mid-turn). Work notes must refresh every time, not just the first.
- Do not compact until a valid work-notes assistant message exists (tool-call-first / non-message outputs can otherwise lead to stale preserved notes).
- While waiting for work notes, avoid draining pending user input; defer it until after compaction so no input is lost or re-ordered.
