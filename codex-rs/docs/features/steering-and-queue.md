# Steering and queued input

Read this when changing composer submission, pending input, user-message acknowledgment, or task lifecycle. [Pause and continue](pause-continue.md) and [completion review](post_turn_completion_review/README.md) define the associated recovery behavior.

## Input ownership

|Input state|Owner and behavior|
|---|---|
|New prompt while idle|TUI renders it and sets `user_turn_pending_start` before submitting `Op::UserTurn`.|
|Steer during a regular turn|TUI sends `Op::SteerInput` with the observed turn ID and a generated client message ID; preview remains pending until core emits the committed user item.|
|Ordinary queue|TUI retains editable/reorderable entries for later turns, including action kind, attachments, mentions, and supported per-message settings.|
|Rejected steer|TUI retains it for recovery; rejected steers are merged into one next-turn submission before ordinary queue entries.|

`features.steer` controls whether Enter steers running work or queues it, and defaults to enabled. Explicit queue actions retain their intent. Input is queued while a plan item is streaming, only user-shell commands are running, or a submitted turn has not started yet. Queue draining is also blocked by queue editing, modal/popup state, and lifecycle auto-send suppression.

## Core contract

- Steering is non-preemptive: validate the expected active turn before appending input. It must not interrupt the in-flight model request or apply new-turn model/CWD/approval settings.
- Only regular tasks accept steering. Review, completion review, compaction, and user-shell tasks produce structured non-steerable errors. Empty input, missing tasks, and mismatched turn IDs have distinct errors.
- `TurnInput::UserInput` preserves the original `Vec<UserInput>`, optional client ID, and review revision. Do not flatten it into a response item before acknowledgment; that loses UI spans and request ownership.
- Record the user response item, update previous-turn settings, and emit user-item lifecycle through the same installation path for normal drain and retained input at task completion/stop. The TUI matches client IDs first, with content matching reserved for legacy events without IDs.
- A committed user item means it was installed in transcript history. It does not prove the model consumed it. Completion-review tracking keeps retained acknowledgments unconsumed until a sampling pass takes ownership, so late input can require continuation after a model final.
- Stop paths retain accepted input before clearing pending waiters and publishing stop. Uncommitted TUI steers are restored; ordinary queued entries remain queued. Never restore a steer already committed by core as a second prompt.

## Request ordering

A fresh prompt is sampled before accepted pending input is drained. On the ordinary path, the next model input extends the preceding request with completed model/tool items and then the steer. Preserve the exact prior input prefix when history and request properties have not changed.

Work-notes capture defers pending input; compaction also defers it until the required model follow-up finishes. `/continue` uses empty explicit input and may drain retained input after cleanup/recovery. Do not move steering ahead of a response/tool continuation just to lower perceived submission latency. Compaction, rollback, image projection, and intentional context changes can legitimately replace a prior request prefix; [WebSocket eligibility](responses-websocket.md) still checks the exact current input.

## Deferred queue actions

|Action|Dequeue behavior|
|---|---|
|Plain prompt|Submit as a new user turn, applying that entry's model/reasoning override.|
|Slash command|Parse and execute when dequeued, with saved paste/mention information; do not execute it at enqueue time.|
|Shell command|Launch when dequeued; do not turn the command into a model prompt.|

Model/reasoning overrides belong to plain entries; command entries do not inherit unsupported overrides. Queue IDs support editing/reordering independently of client IDs used to correlate pending steers. The dequeue loop can pass through commands that finish synchronously, but stops when a turn starts or a command requires interaction.

## Implementation and checks

|Concern|Entry point|
|---|---|
|Turn-ID validation and typed input|[`session/mod.rs`](../../core/src/session/mod.rs): `steer_input_for_turn`, `record_pending_input`; [`state/turn.rs`](../../core/src/state/turn.rs)|
|Accepted/installed/consumed ownership|[`session/completion_review.rs`](../../core/src/session/completion_review.rs), [`tasks/mod.rs`](../../core/src/tasks/mod.rs)|
|Sampling order|[`session/turn.rs`](../../core/src/session/turn.rs): `can_drain_pending_input`|
|Submit and commit correlation|[`input_submission.rs`](../../tui/src/chatwidget/input_submission.rs), [`chatwidget.rs`](../../tui/src/chatwidget.rs): `on_committed_user_message`|
|Queue intent, drain, edit, restore|[`input_flow.rs`](../../tui/src/chatwidget/input_flow.rs), [`input_queue.rs`](../../tui/src/chatwidget/input_queue.rs), [`input_restore.rs`](../../tui/src/chatwidget/input_restore.rs)|

Existing checks in [`pending_input.rs`](../../core/tests/suite/pending_input.rs) inspect exact request ordering and compaction deferral; [`session/tests.rs`](../../core/src/session/tests.rs) tests turn-ID and task-kind rejection; [`chatwidget/tests.rs`](../../tui/src/chatwidget/tests.rs) covers client-ID matching, queued actions, and pause/error restoration. Changes spanning accepted input and restart also need [`completion_review_tests.rs`](../../core/src/session/completion_review_tests.rs).
