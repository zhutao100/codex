# Pause and continue

Read this when changing task stopping, continuation, restart recovery, or the TUI's pause controls. Related contracts: [steering and queue](steering-and-queue.md), [history replay](history-replay.md), [work notes](session-work-notes.md), and [completion review](post_turn_completion_review/README.md).

## User and core behavior

|Action|Behavior|
|---|---|
|`/pause` or bare Esc during supported active work|Cancel active execution and retain a continuation checkpoint without adding a user instruction. Popup/modal Esc handling takes precedence.|
|Ctrl+C during active work|Interrupt and record the contextual `<turn_aborted>` guidance; regular work can subsequently be continued.|
|`/continue`|Start from a pending checkpoint with empty explicit input; do not synthesize a user message.|
|New ordinary prompt|Start normal input handling and supersede the old regular checkpoint.|
|Pause during review, standalone compaction, or standalone user-shell work|Warn that pause is unavailable; leave that task running. Post-turn completion review has its own supported continuation target.|
|Continue while running or without a checkpoint|Return a control error without starting a model request.|

The TUI dispatches `Op::Pause` and `Op::Continue` directly. The app-server recognizes paused status in event handling, but its request enum does not expose dedicated `turn/pause` or `turn/continue` methods.

## Continuation contract

- `ContinueTask` enters the same sampling loop as regular work, with a checkpoint and no new `UserInput`. It preserves completed conversation items and avoids repeating normal input/skill injection solely to resume.
- A regular checkpoint retains its selected model. Explicit model selection can retarget pending regular work; paused completion review and compaction's source model have separate ownership. Preserve the review-specific CWD and recovery-model rules in the completion-review docs.
- Interrupted continuation removes the trailing interruption marker. It trims the incomplete tail from the first unmatched tool call after the latest real user boundary or encrypted compaction item, retaining ghost snapshots.
- A pending work-notes transaction owns its capture segment. Generic continuation cleanup must leave that segment for notes recovery, which finishes before ordinary sampling or new input installation.
- Cleanup also attempts to rewrite the rollout. Its current failure path logs a warning; do not describe it as an atomic transaction with in-memory cleanup. Completion-review persistence has stronger failure handling documented separately.
- Pause cancels execution; it does not suspend a model stream, preserve partial text deltas, or freeze child processes for later resumption. Completed external effects remain completed.

## Stop, input, and restart ordering

Task stopping joins finalization through `task_lifecycle`; it also joins compaction and completion-review installation before publishing the stopped state. Keep running-task handles alive until durable finalization finishes. A stop must not race a completed task's removal from `active_turn` and detach its recorder early.

Accepted pending input is retained through the shared installation path before stop/completion. For tracked requests, retained input remains unconsumed until the next sampling pass installs it for work; an acknowledgment alone does not establish that the model used it. The TUI restores only steers still uncommitted when the stop event arrives, and leaves ordinary queued messages queued. `/continue` sets the pending-start gate so queued input cannot overtake it.

`TurnStarted`, `TurnComplete`, `TurnPaused`, and `TurnContinued` are transient events, excluded by the rollout policy. Restart recovery derives ordinary continuation from incomplete durable history, compaction provenance, and host-owned completion-review state. A regular resumed checkpoint may therefore have no original turn ID or pause reason; do not make recovery depend on replaying a pause event. A standalone compaction summary alone must not invent unfinished regular work.

## Implementation and checks

|Concern|Entry point|
|---|---|
|Control dispatch and checkpoint preparation|[`handlers.rs`](../../core/src/session/handlers.rs): `continue_last`; [`continue_task.rs`](../../core/src/tasks/continue_task.rs)|
|Task eligibility, stop ordering, retained input|[`tasks/mod.rs`](../../core/src/tasks/mod.rs): `stop_all_tasks`, `handle_task_abort`, `retain_pending_input_on_stop`, `on_task_finished`|
|Shared loop and incomplete-tail cleanup|[`turn.rs`](../../core/src/session/turn.rs): `continue_turn`, `history_needs_continuation`, `trim_incomplete_continuation_tail`|
|Persistence and restart|[`rollout/policy.rs`](../../core/src/rollout/policy.rs), [`rollout_reconstruction.rs`](../../core/src/session/rollout_reconstruction.rs)|
|TUI lifecycle|[`slash_dispatch.rs`](../../tui/src/chatwidget/slash_dispatch.rs), [`protocol_requests.rs`](../../tui/src/chatwidget/protocol_requests.rs), [`bottom_pane/mod.rs`](../../tui/src/bottom_pane/mod.rs)|

Relevant existing tests are the `rollout_reconstruction_*` and continuation-tail cases in [`session/tests.rs`](../../core/src/session/tests.rs), pause/continue cases in [`chatwidget/tests.rs`](../../tui/src/chatwidget/tests.rs), and `responses_websocket_pause_continue_reconnects_after_in_flight_response` in [`client_websockets.rs`](../../core/tests/suite/client_websockets.rs). Changes to stop durability also need the real-recorder scenarios in [`completion_review_tests.rs`](../../core/src/session/completion_review_tests.rs) and the [compaction validation guide](remote_compaction_v2/validation.md).
