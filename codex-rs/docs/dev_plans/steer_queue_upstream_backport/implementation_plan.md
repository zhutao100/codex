# Implementation Plan

## Status

Completed in this branch:

- P0-A: exact no-preemption request-prefix regression.
- P0-B: typed pending input plus task-finish commit lifecycle.
- P0-C: dedicated turn-id-checked steer operation and structured race errors.
- P0-D: queued plain/slash/shell action preservation.
- P1-A: rejected steers merge into one follow-up turn.
- P1-B: plan-stream and user-shell queue guards.
- P1-C: abort cleanup ordering, empty-active-turn pending preservation, and queue autosend suppression.
- P2-A: client-generated user-message ids for pending-steer commit and error correlation.

Not implemented here:

- Optional interrupt-and-immediately-resubmit behavior for explicit Ctrl+C.

## Baseline: already implemented

Do not repeat these phases:

- pending-steer storage and commit-later rendering;
- rich-payload render after committed user-message events;
- structured rejection for review, post-turn review, compact, and standalone user-shell tasks;
- rejected-steer priority over ordinary queue;
- `user_turn_pending_start` queue gate;
- pending-steer restoration on pause/interruption;
- `/continue` submission without a user prompt.

Before changing code, retain the existing tests around those behaviors in `tui/src/chatwidget/tests.rs`.

## P0-A: Lock the request-prefix invariant

### Files

- `core/tests/suite/pending_input.rs`
- `core/tests/suite/snapshots/` if snapshot helpers are adopted

### Work

1. Port the upstream no-preemption scenario with a streamed reasoning item, tool call, assistant answer, tool output, and mid-stream steer.
2. Parse the first and second request bodies.
3. Assert exact equality between the first request `input` array and the same-length prefix of the second.
4. Assert the ordered suffix contains the durable response items and then the steer.
5. Keep compaction tests separate and explicitly state that exact prefix is not expected after history rewriting.
6. Compare selected request-level cache-key fields, such as model and tool schema, to ensure the test does not report an input-prefix success while another accidental setting change invalidates cache reuse.

### Acceptance

- The test fails if a steer is inserted before prior response/tool items.
- The test fails if previous input items are removed, reordered, or rewritten in the stable path.
- Existing mid-turn compaction tests still pass.

## P0-B: Preserve typed pending user input and commit leftovers

### Files

- `core/src/state/turn.rs`
- `core/src/session/mod.rs`
- `core/src/session/turn.rs`
- `core/src/tasks/mod.rs`
- `core/src/session/tests.rs`
- protocol event types only if client ids require extension

### Work

1. Introduce branch-local `TurnInput::UserInput { content, client_id }` and `TurnInput::ResponseItem`.
2. Change `TurnState::pending_input` and helper methods to use `TurnInput`.
3. Make `Session::steer_input(...)` enqueue typed user input.
4. Refactor normal pending-input drain to a shared `record_pending_input(...)` helper and drain only from the turn state captured by the running task.
5. In `on_task_finished(...)`, capture the completed turn state, drain leftovers from that state, and use the same helper before `TurnComplete`.
6. Preserve pause semantics: pending input discarded by pause is recovered by the TUI, not committed.

### Tests

Port/adapt upstream `task_finish_emits_turn_item_lifecycle_for_leftover_pending_user_input`:

- include a `TextElement` marker;
- accept steer input into a never-ending regular task;
- finish the task before normal drain;
- assert history persistence;
- assert `RawResponseItem`, `ItemStarted`, `ItemCompleted`, legacy `UserMessage`, then `TurnComplete`;
- assert the original text element survives; if client-id plumbing is included in this patch, assert the id survives as well.
- assert a replacement active turn's pending input is not drained through the old turn state.

Add a TUI/core integration regression:

- submit active-turn steer;
- complete the task through the leftover path;
- deliver commit and completion events;
- assert no rejected steer and no second `Op::UserTurn` is emitted.

### Acceptance

- Every accepted steer is committed once or restored once.
- No natural-completion path silently persists user input.
- Text elements are preserved.

## P0-C: Add turn-id-safe direct steering

### Files

- `protocol/src/protocol.rs`
- `core/src/session/handlers.rs`
- `core/src/session/mod.rs`
- `tui/src/chatwidget.rs`
- `tui/src/chatwidget/protocol_requests.rs`
- `tui/src/chatwidget/input_submission.rs`
- `tui/src/chatwidget/input_restore.rs`
- `tui/src/chatwidget/turn_runtime.rs`
- `tui/src/chatwidget/tests.rs`

### Work

1. Add `Op::SteerInput` with `expected_turn_id`, items, and optional client id.
2. Track the live active turn id from the event envelope on `TurnStarted`.
3. Clear it on all terminal/pause paths.
4. Route active regular submissions through `Op::SteerInput`; route idle submissions through `Op::UserTurn`.
5. Validate expected id and task kind atomically in core.
6. Add structured `NoActiveTurnToSteer` and `ExpectedTurnMismatch` error data.
7. On no-active race, reclassify the pending steer as next-turn input without duplicate rendering.
8. On mismatch, either queue immediately or retry once with the structured actual id. Do not parse error text.
9. Prevent collaboration-mode changes while a turn is running.
10. Remove settings application from accepted steer handling.

### Tests

- matching expected id accepts input;
- mismatched id leaves both the old and replacement turns unmodified;
- no-active race starts/queues exactly one new turn;
- review/compact replacement returns structured non-steerable rejection;
- active mode change is rejected without mutating session state;
- steer does not change cwd, approval, sandbox, model, effort, personality, service tier, or collaboration settings;
- replayed `TurnStarted` does not create a live active id.

### Acceptance

- A steer cannot land on a turn other than the one the TUI targeted.
- A steer cannot change new-turn settings.
- Existing idle turn submission remains unchanged.

## P0-D: Preserve queued action semantics

### Files

- `tui/src/bottom_pane/chat_composer.rs`
- `tui/src/chatwidget/user_messages.rs`
- `tui/src/chatwidget/input_queue.rs`
- `tui/src/chatwidget/input_flow.rs`
- `tui/src/chatwidget/input_submission.rs`
- `tui/src/chatwidget/slash_dispatch.rs`
- queue popup/edit code and tests

### Work

1. Add `QueuedInputAction::{Plain, ParseSlash, RunShell}`.
2. Extend `InputResult::Queued` and `QueuedUserMessage` with action metadata; keep pending-paste metadata as a reserved field for future deferred commands that intentionally preserve placeholders.
3. Move the active-task queue branch before bare/inline slash dispatch while preserving idle slash dispatch and paste-burst newline handling.
4. Defer slash validation while queueing.
5. Expand large-paste placeholders before enqueue for current deferred commands. Preserve unexpanded placeholders only if a future deferred command requires placeholder identity.
6. Add dequeue handlers for queued slash and shell actions.
7. Convert `maybe_send_next_queued_input()` into a bounded drain loop that stops after starting a turn/task or opening a blocking UI.
8. Keep queue ids, reordering, editing, local images, text elements, mention paths, and model/reasoning overrides intact for plain queued prompts.
9. Disable model/reasoning override display and editing for shell and slash command actions.

### Tests

At enqueue time while a task runs:

- `/compact`, `/review check regressions`, `/prompts:*`, a settings command, and an unknown slash command produce `ParseSlash` with no immediate dispatch/error or prompt expansion;
- `!echo hi` produces `RunShell` with no execution;
- leading-space slash produces `Plain`;
- queue editing/reordering preserves the action.

At dequeue time:

- `/compact` executes only after idle;
- an informational command can run and allow the next queued item to be considered;
- an unknown slash diagnostic does not turn into literal model input;
- a queued shell command runs once;
- a plain prompt still applies its stored model/reasoning overrides;
- queued shell and slash commands do not display or accept per-message model/reasoning overrides;
- at most one turn/task starts per drain call.

### Acceptance

Using the queue key never causes immediate command execution or validation.

## P1-A: Merge rejected steers

### Files

- `tui/src/chatwidget/input_restore.rs`
- `tui/src/chatwidget/input_flow.rs`
- `tui/src/chatwidget/user_messages.rs`
- `tui/src/chatwidget/tests.rs`

### Work

1. Drain all rejected steers when selecting the next input.
2. Merge them in submission order.
3. Rebase text elements, image placeholders, and mention paths.
4. Submit the merged correction before ordinary queued input.

### Tests

- two rejected steers produce one `Op::UserTurn` containing both texts in order;
- normal queued input remains behind the merged correction;
- images and text-element ranges remain valid after merge.

## P1-B: Avoid impossible steering states

### Files

- `tui/src/chatwidget/input_flow.rs`
- `tui/src/chatwidget/streaming.rs`
- user-shell runtime tracking
- TUI tests

### Work

1. Add `is_plan_streaming_in_tui()` using `plan_stream_controller.is_some()`.
2. Queue ordinary Enter submissions while a plan item is streaming.
3. Add `only_user_shell_commands_running()` or the branch-equivalent predicate.
4. Queue ordinary model input while only user-shell work is active.

### Tests

- Enter during plan stream enters ordinary queue and creates no pending steer;
- Enter during standalone user-shell work enters ordinary queue;
- bang-shell behavior follows existing shell concurrency rules;
- after the stream/shell task ends, the queue drains normally.

## P1-C: Harden abort and transition cleanup

### Files

- `core/src/tasks/mod.rs`
- `core/src/session/tests.rs`
- `tui/src/chatwidget/input_queue.rs`
- `tui/src/chatwidget/input_restore.rs`
- `tui/src/chatwidget/turn_runtime.rs`

### Work

1. Cancel and clean up a running task before clearing pending state.
2. Clear pending input only for a turn that actually had an aborted task.
3. Preserve pending input held by an empty active-turn shell.
4. Add `suppress_queue_autosend` around pause/interrupt/replay restoration windows.
5. Perform one explicit drain check after state restoration completes.
6. Move pending steers to the rejected queue before draining after terminal model-cap and policy errors.

### Tests

- adapt upstream `abort_empty_active_turn_preserves_pending_input`;
- cancellation does not surface a model-visible approval rejection before `TurnAborted`;
- pause restores pending steers and leaves ordinary queue unchanged;
- terminal model-cap and policy errors submit any pending steer once after the turn ends;
- queue does not auto-send during intermediate restore state.

## P2-A: Exact pending-steer correlation

### Files

- TUI pending-steer state
- `Op::SteerInput`
- core typed input
- `UserMessageEvent` and item lifecycle mapping

### Work

Completed:

1. Generate a client user-message id per TUI steer submission.
2. Preserve it through core pending input, item lifecycle, and committed `UserMessageEvent`.
3. Return it on structured steer rejection `ErrorEvent`s so late errors remove the matching pending steer instead of the oldest pending steer.
4. Match pending steers by id first.
5. Keep flattened text/image-count matching only as a compatibility fallback for replay and old events without ids.

### Tests

- accepted-A/rejected-B interleaving requeues only B by client id while A remains pending for its commit event;
- leftover pending input commit lifecycle preserves the client id through `ItemStarted`, `ItemCompleted`, and legacy `UserMessage`;
- replay and old live events without client ids still use the compatibility fallback.

## P2-B: Optional interrupt-and-submit

Adapt only the useful part of upstream behavior:

- explicit Ctrl+C with pending steers can request immediate resubmission after abort;
- merge only pending steers;
- preserve ordinary queue and current composer draft;
- never activate this path for `/pause`.

## Suggested commit sequence

1. Add exact-prefix regression tests without behavior changes.
2. Introduce typed pending input and task-finish lifecycle parity.
3. Add dedicated steer operation and expected-turn errors.
4. Switch the TUI active path to dedicated steering.
5. Add queued action data types and composer defer logic.
6. Add dequeue command/shell processing and queue tests.
7. Merge rejected steers and add plan/shell guards.
8. Harden abort cleanup and queue suppression.
9. Add client ids and optional interrupt behavior.

Each commit should keep the workspace compiling and preserve current pause/continue tests.

## Validation commands

Run at minimum:

```bash
just fmt
CODEX_SANDBOX_NETWORK_DISABLED=1 scripts/cargo-local test -p codex-core --test all pending_input
CODEX_SANDBOX_NETWORK_DISABLED=1 scripts/cargo-local test -p codex-core session::tests::task_finish
CODEX_SANDBOX_NETWORK_DISABLED=1 scripts/cargo-local test -p codex-core session::tests::abort
CODEX_SANDBOX_NETWORK_DISABLED=1 scripts/cargo-local test -p codex-tui chat_composer
CODEX_SANDBOX_NETWORK_DISABLED=1 scripts/cargo-local test -p codex-tui chatwidget::tests
just write-app-server-schema
CODEX_SANDBOX_NETWORK_DISABLED=1 scripts/cargo-local test -p codex-app-server-protocol
CODEX_SANDBOX_NETWORK_DISABLED=1 just fix -p codex-core
CODEX_SANDBOX_NETWORK_DISABLED=1 just fix -p codex-tui
```

Use the package/test selectors available in the branch if exact names differ.

## Completion criteria

The backport is complete when all of the following hold:

- a queued slash/shell action never executes at enqueue time;
- an accepted steer cannot target a stale/replacement turn;
- active steering applies no new-turn settings;
- accepted leftover steer input commits before `TurnComplete` and is never resubmitted;
- the stable follow-up request preserves the previous request input exactly as a prefix;
- rejected steers are submitted once as one correction before ordinary queue;
- plan-stream and user-shell-only states queue ordinary input;
- pause/continue and editable queue behavior remain unchanged.
