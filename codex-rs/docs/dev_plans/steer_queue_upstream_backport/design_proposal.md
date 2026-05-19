# Design Proposal: Steer And Queue Input Backport

## Design summary

Backport the upstream behavior with a branch-local shape:

1. Add TUI state for pending steers and rejected steers alongside existing queued messages.
2. Make `submit_user_message_with_overrides(...)` render immediately only for idle user turns.
3. Handle live committed user-message events and use them to render accepted pending steers exactly once.
4. Replace `Session::inject_input(...)` for user turn steering with a validated `Session::steer_input(...)` path.
5. Add structured non-steerable-turn errors and recover those errors in the TUI by moving pending steers to rejected queue.
6. Add `user_turn_pending_start` to prevent queued input from draining before the submitted turn has actually started.
7. Preserve this branch's `/pause` and `/continue` semantics instead of copying upstream's `Esc` behavior literally.

## TUI state model

Add a small input-state bag to `tui/src/chatwidget.rs` or a new module such as `tui/src/chatwidget/input_queue.rs`.

Recommended state:

```rust
struct InputQueueState {
    queued_user_messages: VecDeque<QueuedUserMessage>,
    queued_user_message_history_records: VecDeque<UserMessageHistoryRecord>,
    pending_steers: VecDeque<PendingSteer>,
    rejected_steers_queue: VecDeque<UserMessage>,
    rejected_steer_history_records: VecDeque<UserMessageHistoryRecord>,
    user_turn_pending_start: bool,
    suppress_queue_autosend: bool,
}

struct PendingSteer {
    user_message: UserMessage,
    history_record: UserMessageHistoryRecord,
    compare_key: PendingSteerCompareKey,
}

struct PendingSteerCompareKey {
    message: String,
    image_count: usize,
}
```

`UserMessageHistoryRecord` can be minimal for this branch. It should exist so future queued slash-command or transformed-prompt flows can render different history text without breaking queue/steer ordering.

## Submit path

Change `tui/src/chatwidget.rs::submit_user_message_with_overrides(...)`:

|Condition|Behavior after backport|
|---|---|
|No configured session|Queue at front as today.|
|Empty text and no images|No-op as today.|
|Unsupported images|Restore draft as today.|
|Shell command `!cmd`|Run shell command as today.|
|No active agent turn|Send `Op::UserTurn`, set `user_turn_pending_start = true`, append cross-session history, render visible user prompt immediately.|
|Active regular turn|Send `Op::UserTurn`, append cross-session history if desired, push `PendingSteer`, refresh pending-input preview, do not render visible user prompt yet.|

The visible-history render must move behind a condition equivalent to upstream's `render_in_history = !agent_turn_running`.

Do not remove `Op::AddToHistory` unless separate product behavior requires it. Cross-session message recall and visible transcript rendering are different concerns. If `Op::AddToHistory` causes unwanted recall for rejected/lost steers, gate it behind successful submit and keep a follow-up issue to make message-history persistence commit-based too.

## Commit-later rendering

Change live event handling in `tui/src/chatwidget.rs`:

- Handle `EventMsg::UserMessage(ev)` for both live events and replay.
- For replay, render as today.
- For live events, compute a pending-steer compare key from the event payload.
- If the key matches `pending_steers.front()`, pop that pending steer and render its original UI-rich payload.
- If no pending steer matches, render the event only if it is not a duplicate of the last rendered user message.

This mirrors upstream `on_committed_user_message(...)` but can use this branch's `UserMessageEvent` shape.

A minimal compare key is sufficient:

```rust
fn pending_steer_compare_key_from_inputs(items: &[UserInput]) -> PendingSteerCompareKey;
fn pending_steer_compare_key_from_event(event: &UserMessageEvent) -> PendingSteerCompareKey;
```

Use flattened text and total image count. Do not require exact text-element equality because committed core events may not preserve all UI spans in the same representation.

## Pending input preview

Replace or extend `tui/src/bottom_pane/queued_user_messages.rs` with a preview that can render three sections:

1. Pending steers: "Messages to be submitted after next tool call" or branch-appropriate wording.
2. Rejected steers: "Messages to be submitted at end of turn".
3. Queued messages: existing queued-message preview.

This is a UX feature, but it also makes loss/recovery bugs visible during testing.

## Queue drain behavior

Change `maybe_send_next_queued_input()` to gate on `is_user_turn_pending_or_running()`:

```rust
fn is_user_turn_pending_or_running(&self) -> bool {
    self.user_turn_pending_start || self.bottom_pane.is_task_running()
}
```

Set `user_turn_pending_start = true` only when an idle user turn is submitted and expected to start a new turn. Clear it when the TUI observes turn start, turn complete, turn abort, turn pause, stream error, or a failed submit path.

Drain priority should be:

1. Rejected steers, merged into a single message when needed.
2. Normal queued user messages.

This preserves the user's intent: a steer that could not be applied to the current task should run before later queued follow-ups.

## Core steer validation

Add a branch-local `SteerInputError` in `core/src/session/mod.rs`:

```rust
enum SteerInputError {
    NoActiveTurn(Vec<UserInput>),
    ActiveTurnNotSteerable { turn_kind: NonSteerableTurnKind },
    EmptyInput,
}
```

Optionally add `ExpectedTurnMismatch` if app-server or thread-switch steering is backported at the same time.

Add protocol-level error information in `protocol/src/protocol.rs`:

```rust
enum NonSteerableTurnKind {
    Review,
    Compact,
    UserShell,
}

enum CodexErrorInfo {
    ActiveTurnNotSteerable { turn_kind: NonSteerableTurnKind },
    // existing variants
}
```

Implement `Session::steer_input(...)` with these rules:

1. If no active turn exists, return `NoActiveTurn(input)`.
2. If input is empty, return `EmptyInput`.
3. Inspect the first active task kind.
4. Accept only `TaskKind::Regular`.
5. Reject `TaskKind::Review` and `TaskKind::Compact` as non-steerable.
6. Treat this branch's `TaskKind::PostTurnCompletionReview` as `NonSteerableTurnKind::Review`.
7. Treat this branch's standalone `TaskKind::UserShell` as `NonSteerableTurnKind::UserShell`. Auxiliary user-shell commands run inside an existing regular turn and do not change the active task kind.
8. If accepted, push input into active-turn pending input.

Then change `core/src/session/handlers.rs::user_input_or_turn(...)`:

```rust
match sess.steer_input(items.clone()).await {
    Ok(()) => {
        current_context.otel_manager.user_prompt(&items);
    }
    Err(SteerInputError::NoActiveTurn(items)) => {
        sess.clear_pending_continuation().await;
        sess.refresh_mcp_servers_if_requested(&current_context).await;
        sess.spawn_task(Arc::clone(&current_context), items, RegularTask).await;
    }
    Err(err) => {
        sess.send_event_raw(Event { id: sub_id, msg: EventMsg::Error(err.to_error_event()) }).await;
    }
}
```

Keep this branch's `Op::UserTurn` and `SessionSettingsUpdate` flow. The upstream branch's app-server-first `Op::UserInput` refactor is not required for this backport.

## Pending input drain

This branch already has the key initial ordering guard in `core/src/session/turn.rs`:

```rust
let mut can_drain_pending_input = input.is_empty();
```

Keep that behavior and add regression tests. Do not reintroduce eager draining before the first sample of a fresh explicit prompt.

If hook-based pending-input inspection is backported later, follow upstream's additional behavior:

- inspect each pending input item before recording it;
- if blocked, prepend the remaining pending input back to the queue;
- force follow-up sampling when accepted pending input remains.

## Pause, continue, and interrupt adaptation

Do not copy the upstream `Esc` behavior literally. This branch treats pause as a first-class lifecycle, and bare `Esc` may be part of that pause UX.

Recommended branch behavior:

|Operation|Pending steer behavior|
|---|---|
|`/pause` or branch pause key|Do not discard pending steers. Keep them in TUI pending preview if still expected to commit, or restore them to composer if core reports the active turn stopped before commit.|
|`/continue`|Resume the paused turn without adding a user message. Do not drain normal queued messages before the continued turn has actually restarted or completed.|
|Explicit interrupt|Restore pending steers to the composer, preserving order before queued messages.|
|Optional explicit interrupt-and-send|Merge pending steers into one fresh user turn after interrupt, but bind this to an explicit command or shortcut that does not conflict with pause.|
|Review/compact steer rejection|Move the pending steer to rejected queue and drain it before normal queued messages once the non-steerable task ends.|

This branch's `core/src/tasks/mod.rs` clears pending state during task stop. Therefore, TUI-side pending-steer storage is mandatory for pause/interrupt safety.

## App-server API option

If external app-server clients need same-turn steering, backport upstream's `turn/steer` contract separately:

- `app-server-protocol/src/protocol/v2/turn.rs::TurnSteerParams`
- `app-server/src/request_processors/turn_processor.rs::turn_steer_inner(...)`
- required `expectedTurnId`
- input-size validation
- structured `ActiveTurnNotSteerable` error mapping

This is optional for the direct TUI path. It should not block the main TUI/core backport.
