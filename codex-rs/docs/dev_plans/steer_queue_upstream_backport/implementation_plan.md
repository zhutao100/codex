# Implementation Plan: Steer And Queue Input Backport

## Phase 0 - Reproduce and lock current behavior

Add failing tests before changing behavior.

### Tests

|Test|Expected current failure|
|---|---|
|Enter steer while active does not render immediately|This branch currently renders immediately.|
|Committed user-message event renders pending steer once|This branch ignores live `EventMsg::UserMessage`.|
|Pause after pending steer does not lose text|This branch has no TUI pending-steer state.|
|Review active turn rejects steer and queues it for next turn|This branch has no core non-steerable steer validation.|
|Queue does not drain during submitted-but-not-started interval|This branch has no `user_turn_pending_start` gate.|
|Tab queued input remains next-turn input|Use as a safety test while editing Enter behavior.|
|`/pause` and `/continue` do not create user prompts|Protect branch-specific behavior.|

Recommended test areas:

- `tui/src/chatwidget.rs` tests or a new `tui/src/chatwidget/tests/steer_queue.rs`.
- `core/src/session/tests.rs` for `Session::steer_input(...)` validation.
- Existing pause/continue tests under core and TUI.

## Phase 1 - Add TUI pending steer state

### Steps

1. Add `PendingSteer`, `PendingSteerCompareKey`, `UserMessageHistoryRecord`, and `InputQueueState`.
2. Move existing `queued_user_messages`, `next_queued_user_message_id`, and queued edit plumbing into or behind this state without changing queued edit behavior.
3. Add preview conversion methods:
   - queued message preview text;
   - pending steer preview text;
   - rejected steer preview text.
4. Keep existing queue editing shortcuts working for normal queued messages.

### Files

- `tui/src/chatwidget.rs`
- optional new `tui/src/chatwidget/input_queue.rs`
- optional new `tui/src/chatwidget/user_messages.rs`
- `tui/src/bottom_pane/queued_user_messages.rs` or new `tui/src/bottom_pane/pending_input_preview.rs`
- `tui/src/bottom_pane/mod.rs`

## Phase 2 - Stop eager rendering for active-turn Enter steers

### Steps

1. In `submit_user_message_with_overrides(...)`, compute `render_in_history = !self.agent_turn_running`.
2. For `render_in_history == false`, push `PendingSteer` after a successful send and refresh preview.
3. Do not call `add_to_history(history_cell::new_user_prompt(...))` for pending steers.
4. Keep immediate rendering for idle user turns.
5. Add duplicate-protection state for last rendered user message.

### Tests

- Enter steer while active sends an op and creates one pending steer.
- Enter steer while active does not add a visible user prompt.
- Idle submit still renders immediately.
- Failed submit does not add pending preview.

## Phase 3 - Render live committed user-message events

### Steps

1. Change `EventMsg::UserMessage(ev)` handling to run for live events too.
2. Add an `on_committed_user_message(...)` helper.
3. Match live committed messages against `pending_steers.front()` by compare key.
4. Pop and render matched pending steers using the original `UserMessage` payload so local image paths and text elements are preserved.
5. If unmatched, render only when not in review mode and not a duplicate.

### Tests

- Committed user-message event pops exactly the first matching pending steer.
- A non-front match does not pop out of order.
- Local image plus text elements are preserved from the pending steer render path.
- Replay still renders user messages.

## Phase 4 - Add core `steer_input(...)` validation

### Steps

1. Add `NonSteerableTurnKind` and `CodexErrorInfo::ActiveTurnNotSteerable` to `protocol/src/protocol.rs`.
2. Add `SteerInputError` and `to_error_event()` to `core/src/session/mod.rs`.
3. Implement `Session::steer_input(...)` using this branch's existing `active_turn` and `TaskKind` types.
4. Replace the user-turn `inject_input(...)` call in `core/src/session/handlers.rs` with `steer_input(...)` plus `NoActiveTurn` fallback.
5. Keep `inject_response_items(...)` for non-user-turn internals unless they need the same validation.
6. Keep the `TaskKind` match exhaustive. This branch has `PostTurnCompletionReview` and `UserShell` in addition to upstream's regular/review/compact task kinds:
   - map `PostTurnCompletionReview` to `NonSteerableTurnKind::Review`;
   - map standalone `UserShell` to `NonSteerableTurnKind::UserShell`.

### Tests

- No active turn returns `NoActiveTurn` and starts a new regular task from the handler.
- Empty input emits a bad-request error.
- Regular active turn accepts steer input into pending input.
- Review active turn emits `ActiveTurnNotSteerable { Review }`.
- Compact active turn emits `ActiveTurnNotSteerable { Compact }`.
- Post-turn completion review emits `ActiveTurnNotSteerable { Review }`.
- Standalone user-shell active turn emits `ActiveTurnNotSteerable { UserShell }`.

## Phase 5 - Recover rejected steers in the TUI

### Steps

1. Detect `CodexErrorInfo::ActiveTurnNotSteerable` in the TUI error path.
2. Pop `pending_steers.front()` and push its `UserMessage` into `rejected_steers_queue`.
3. Refresh pending-input preview.
4. Ensure rejected steers drain before normal queued messages after the active task finishes.
5. Avoid finalizing the visible turn as a generic failure for this specific error.

### Tests

- Review rejection moves pending steer to rejected queue.
- Compact rejection moves pending steer to rejected queue.
- Rejected steer drains before normal queued messages.
- Rejected steer preserves text elements and local images.

## Phase 6 - Add queue start gate

### Steps

1. Add `user_turn_pending_start` to TUI state.
2. Set it after successful idle user-turn submission.
3. Clear it on turn started/running, turn complete, turn abort, turn pause, stream error, and submit failure.
4. Change `maybe_send_next_queued_input()` to return without draining when `user_turn_pending_start` is true.

### Tests

- Two queued messages do not both submit before the first turn-start event.
- Queue drains exactly one item when a turn completes.
- Queue does not drain while a modal or queue edit is active.
- Queue resumes after modal close if idle.

## Phase 7 - Adapt pause and continue

### Steps

1. Audit all `TurnPaused`, `TurnAborted`, `TurnContinued`, and stream-error handlers.
2. On pause, do not drop pending steers from TUI state until they either commit or are restored.
3. If pause completes before pending steers commit, restore pending steers to composer or leave them visible as pending with clear state. The safer MVP is restore-to-composer because core has cleared active pending input.
4. On `/continue`, suppress normal queue auto-drain until the continued turn has restarted or completed.
5. Keep explicit interrupt behavior separate from pause.

### Tests

- Pending steer followed by `/pause` leaves the steer recoverable.
- `/continue` resumes without adding a user prompt.
- `/continue` does not drain normal queued messages before the continued turn lifecycle is settled.
- Explicit interrupt restores pending steers before queued messages.

## Phase 8 - Optional queue action semantics

Backport upstream `QueuedInputAction` if queued slash prompts should preserve command semantics.

### Steps

1. Extend `InputResult::Queued` with an action:
   - `Plain`
   - `ParseSlash`
   - `RunShell`
2. Store the action in `QueuedUserMessage`.
3. During queue drain, parse slash commands and shell commands according to the action instead of treating everything as plain prompt text.

### Tests

- Queued `/review` dispatches as a slash command when drained.
- Queued plain text beginning with `/` can still be sent as text if action is `Plain`.
- Queued `!cmd` executes as a shell command when action is `RunShell`.

## Phase 9 - Optional app-server `turn/steer`

Do this only if external clients need same-turn steer parity.

### Steps

1. Add `TurnSteerParams` and `TurnSteerResponse` to `app-server-protocol/src/protocol/v2/turn.rs`.
2. Route `turn/steer` in `app-server-protocol/src/protocol/common.rs` and app-server message processing.
3. Require `expectedTurnId`.
4. Validate input limits.
5. Call `Session::steer_input(...)` with expected turn id support.
6. Map errors to structured JSON-RPC invalid-request responses.

### Tests

- `turn/steer` requires active turn.
- `turn/steer` rejects empty input.
- `turn/steer` rejects oversized input.
- `turn/steer` enforces expected turn id.
- `turn/steer` rejects review/compact turns with structured error data.

## Acceptance criteria

|Criterion|Required result|
|---|---|
|Enter while active|Creates pending steer preview; does not immediately add a visible user prompt.|
|Core commit|Live user-message event renders the pending steer exactly once.|
|Tab while active|Creates a queued message; does not steer the current turn.|
|Review/compact active|Steer is rejected, moved to rejected queue, and retried before normal queued input.|
|Pause after pending steer|The uncommitted steer remains recoverable and is not silently discarded.|
|Continue|Resumes without adding a user prompt and without prematurely draining queued messages.|
|Prefix-cache semantics|No premature local transcript/history mutation before core commit; post-commit model-visible context changes are accepted as intended.|
|Existing queue editing|Move, edit, delete, and send-next behavior for normal queued messages still works.|
