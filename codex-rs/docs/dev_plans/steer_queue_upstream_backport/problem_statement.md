# Problem Statement: Steer And Queue Input Backport

## Context

This branch supports two mid-turn input modes in the TUI:

- Enter steer input: send additional instructions immediately while the active turn is still running.
- Tab queued input: defer the message until the active turn finishes.

This branch also has `/pause` and `/continue`, which are not part of the upstream branch behavior being compared here. Any backport must preserve those controls and their distinction from interruption.

## Current behavior to fix

### 1. Enter steer renders before core commits it

In this branch, `tui/src/chatwidget.rs::submit_user_message_with_overrides(...)` always adds a `history_cell::new_user_prompt(...)` after sending `Op::UserTurn`, including while `agent_turn_running` is true.

Core handles the same input later:

1. `core/src/session/handlers.rs::user_input_or_turn(...)` creates a new `TurnContext`.
2. `Session::inject_input(...)` appends the input to the active turn's pending input if any task is active.
3. `core/src/session/turn.rs` later drains pending input and records a user-message item before the next sampling request.

As a result, the TUI visible transcript is mutated before core has committed the user message. This is the concrete source of the observed "conversation history changed when Enter was used as steer" behavior.

### 2. Live committed user-message events are ignored

`EventMsg::UserMessage(ev)` is handled only when `from_replay` is true. That works only because this branch renders local user prompts eagerly.

After the eager render is removed for active-turn steering, the live user-message event must become the authoritative commit signal. Without that change, accepted pending steers would never appear in the visible transcript.

### 3. Core accepts steer input into non-steerable active tasks

`Session::inject_input(...)` checks only whether `active_turn` exists. It does not validate the active task kind.

The upstream branch rejects same-turn steering for review and compact tasks and reports `CodexErrorInfo::ActiveTurnNotSteerable`. This branch should do the same so a steer typed during review or compaction is not injected into a task that cannot semantically process same-turn instructions.

This branch also has `PostTurnCompletionReview` and standalone `UserShell` task kinds. The backport must handle those explicitly: post-turn completion review should be treated as a review task, and standalone user-shell turns should be rejected as a non-steerable user-shell task. Auxiliary user-shell commands already run under the active regular turn and do not need separate handling.

### 4. Pending steers can be lost when work is stopped

This branch's task stop paths clear active-turn pending state. For example, `core/src/tasks/mod.rs::take_all_running_tasks(...)` clears pending state before draining tasks, and pause paths intentionally keep continuation state rather than pending user input.

Because the TUI does not retain pending steer payloads separately, uncommitted Enter steer input can be lost when the user pauses, interrupts, or hits an error before the active turn drains pending input.

### 5. Queue draining has a start-event race

`maybe_send_next_queued_input()` checks `bottom_pane.is_task_running()` but does not track the interval after a user turn is submitted and before the TUI receives the turn-start/running event.

The upstream branch adds `user_turn_pending_start` to block queue auto-drain during that interval. This avoids accidentally starting or draining another queued input before the previous submission has become visible to the lifecycle state.

### 6. Queue preview does not distinguish queued, pending, and rejected input

This branch shows queued messages only. Enter steers that have been submitted but not yet committed are invisible, and steers rejected by review/compact have no dedicated recovery state.

The upstream branch exposes three distinct states in the bottom pane:

- Pending steers: submitted to core, waiting for commit.
- Rejected steers: could not steer the current task, will be retried at end of turn.
- Queued messages: next-turn inputs queued by the user.

## Prefix-cache clarification

A steer is same-turn user input. Once accepted, the next sampling request includes it and the model-visible suffix changes. Therefore, prefix-cache reuse between the pre-steer request and the post-steer request is not a correctness requirement.

The correctness requirement is narrower:

- local visible history must not change before core commits the steer;
- pending input ordering must be deterministic;
- queued input must not be merged into the first sample of a fresh explicit prompt;
- uncommitted steer input must be recoverable if the active task is stopped.

## Backport goals

|Goal|Expected outcome|
|---|---|
|Commit steer display from core events|Enter steer while a task is running appears as pending preview first, then as one visible user prompt after `EventMsg::UserMessage` or equivalent committed user item.|
|Separate steer from queue|Enter remains same-turn steer; Tab remains next-turn queue.|
|Reject non-steerable active tasks|Review and compact tasks do not accept pending steer input; the TUI moves those payloads to rejected-steer queue.|
|Preserve pending steer payloads|Pause, interrupt, thread switch, and recoverable error paths do not lose uncommitted user input.|
|Close queue-drain race|Queued follow-ups do not drain until no user turn is pending or running.|
|Preserve `/pause` and `/continue`|Upstream interrupt-specific behavior is adapted, not copied over the branch's pause lifecycle.|

## Non-goals

- Do not remove branch-specific `/pause` and `/continue`.
- Do not rewrite the TUI around upstream app-server session routing.
- Do not require upstream `turn/steer` API support for the direct TUI path.
- Do not treat post-steer prefix-cache invalidation as a bug after the steer has actually been committed into model-visible context.
