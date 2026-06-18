# Problem Statement

## Current baseline

This branch already implements the first generation of the upstream steer backport:

- `tui/src/chatwidget/input_submission.rs` stores active-turn submissions as `PendingSteer` and does not render them immediately.
- `tui/src/chatwidget.rs::on_committed_user_message(...)` renders a matching pending steer exactly once after core commits it.
- `core/src/session/mod.rs::steer_input(...)` accepts only regular active turns.
- `tui/src/chatwidget/input_restore.rs` recovers uncommitted steers across interruption and pause.
- `tui/src/chatwidget/input_flow.rs` gates queue draining on `user_turn_pending_start || task_running`.

The proposal must not reimplement or regress those behaviors.

## Problems addressed by this backport

### 1. Queue intent is lost before the item enters the queue

`tui/src/bottom_pane/chat_composer.rs::handle_submission_with_time(...)` parses bare and inline slash commands before it checks `should_queue`.

Consequences while a task is running:

- Tab on `/compact` or `/review ...` reports that the command is unavailable instead of queueing it for later.
- Slash commands that are available during a task can run immediately even though the user used the queue key.
- Unknown slash commands are validated at enqueue time rather than dequeue time.
- Queue state records only text, so it cannot distinguish a literal prompt from deferred slash or shell execution.

Upstream fixes this by queueing first and storing `QueuedInputAction::{Plain, ParseSlash, RunShell}`.

### 2. A late accepted steer can be persisted without being committed to the TUI

Core pending input in this branch is stored as `Vec<ResponseInputItem>`. If a regular task finishes after accepting a steer but before the turn loop drains it, `core/src/tasks/mod.rs::on_task_finished(...)` writes the converted response item to history but emits no user-message item lifecycle.

The TUI still has the corresponding `PendingSteer`. When `TurnComplete` arrives, it moves that pending steer to the rejected queue and submits it as a new turn. The same user input can therefore be persisted once and submitted again.

Upstream keeps `TurnInput::UserInput { content, client_id }` and, on task completion, emits the same raw/item-started/item-completed/legacy-user-message lifecycle used by the normal drain path before `TurnComplete`.

### 3. Start and steer share one operation and one settings path

The TUI sends `Op::UserTurn` for both a new turn and an active-turn steer. `core/src/session/handlers.rs::user_input_or_turn(...)` calls `new_turn_with_sub_id(...)` and applies `SessionSettingsUpdate` before it knows whether the input will steer the existing turn.

This creates three correctness risks:

- a stale TUI running flag can steer whichever turn happens to be active rather than the turn the user saw;
- a steer can mutate session settings even though the active turn continues with its existing frozen `TurnContext`;
- `submit_user_message_with_mode(...)` can switch collaboration mode during an active turn before submitting the steer.

Upstream's active TUI path uses app-server `turn/steer` with a required `expectedTurnId`, retries a stale-id race once, and sends no new-turn settings with the steer.

### 4. Cache-relevant ordering is under-specified by tests

This branch verifies that the first request excludes pending input and a later request includes it. Those tests do not prove that the first request input is an exact prefix of the second request input.

The required normal-path invariant is:

```text
follow_up.input[0..first.input.len()] == first.input
```

The appended suffix must retain all durable response items produced before the steer and place the steer after them. Compaction and explicit context rewrites require separate tests because exact-prefix preservation is not expected there.

### 5. Rejected steers are fragmented into multiple future turns

This branch drains one rejected steer at a time. If several Enter submissions are rejected by a review, compact, or standalone user-shell task, each becomes a separate future turn.

Upstream merges all rejected steers, in order, into one next-turn message before ordinary queued input. This better matches the user's attempt to add several instructions to the same active work item.

### 6. Avoidable steer attempts occur during plan streaming and standalone shell work

The upstream TUI queues ordinary input when:

- a proposed-plan item is still streaming in the TUI; or
- the only running commands are user-shell commands.

Before this backport, this branch had a plan stream controller but did not use it as a submission gate. It also relied on core rejection for standalone user-shell tasks rather than queueing before submission. Both cases created transient pending-steer state that could not be committed to the active model turn.

### 7. Abort cleanup can discard pending input too early

`core/src/tasks/mod.rs::take_all_running_tasks(...)` removes the active turn and calls `clear_pending()` before task cancellation and cleanup. It also clears pending input when an `ActiveTurn` exists with no running task.

Upstream lets the task observe cancellation before clearing pending waiters/input and preserves pending input for an empty active-turn shell. The exact upstream task architecture should not be copied, but those ordering properties should be retained.

### 8. Pending-steer correlation remains content-based

Both branches correlate committed user messages with pending steers using flattened text plus image count. Repeated identical messages can be ambiguous, and skill/mention identity is omitted.

Upstream's protocol supports `clientUserMessageId`, but the upstream TUI currently sends `None`, so this is a shared residual risk rather than an upstream fix already available end to end.

## Required invariants

### Turn identity

- A steer must target the active turn observed by the TUI.
- Core must atomically validate the expected turn id and task kind before accepting input.
- A stale turn id must never silently steer a replacement review, compact, continuation, or regular turn.

### Commit semantics

- A TUI pending steer is uncommitted until core emits its user-message lifecycle.
- Every accepted user steer is either committed exactly once or returned to recoverable TUI state.
- `TurnComplete` must not precede the commit lifecycle for accepted leftover user input.

### Model-request ordering

- Steering is non-preemptive: already-produced reasoning, assistant messages, tool calls, and tool outputs remain before the steer.
- In a stable, non-compacting turn, the earlier request input remains an exact prefix of the follow-up request input.
- Compaction, truncation, rollback, and deliberate context updates are explicit exceptions.

### Queue fidelity

- The queue key must never execute or validate a slash/shell action immediately.
- Dequeue must preserve whether the item was plain input, a slash command, or a shell command.
- Queue ids, ordering, editing, images, text elements, mention paths, and model/reasoning overrides must survive the backport.
- Non-turn commands may be processed while draining, but at most one new user/model turn may be started per drain pass.

### Pause and continue

- `/pause` does not commit pending steers and does not flatten ordinary queued messages into the composer.
- `/continue` does not add a user message.
- Queue auto-send remains blocked while a continuation is submitted but has not started.
- Explicit interrupt behavior may differ from pause, but queued items must not be lost.

## Non-goals

- Replacing the TUI's direct operation channel with the upstream app-server session.
- Porting upstream multi-thread input snapshots or mailbox scheduling as part of this work.
- Guaranteeing provider-side cache hits after compaction or after a cache-key request field changes.
- Removing this branch's queue editor, model override, reasoning-effort override, `/pause`, or `/continue` features.
