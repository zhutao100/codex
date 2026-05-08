# Task

Prepare a drop-in design for recoverable mid-turn pause and continuation in the TUI.

## Target Base

This plan is written against code shape `15a9f932c49d78c8b5db4625bd542165b30e3c9b`.

The relevant surfaces on this base are:

- `tui/src/slash_command.rs`
- `tui/src/bottom_pane/mod.rs`
- `tui/src/bottom_pane/footer.rs`
- `tui/src/chatwidget.rs`
- `protocol/src/protocol.rs`
- `core/src/codex.rs`
- `core/src/tasks/mod.rs`
- `core/src/tasks/regular.rs`
- `core/src/rollout/policy.rs`
- `app-server-protocol/src/protocol/common.rs`
- `app-server-protocol/src/protocol/v2.rs`
- `app-server/src/codex_message_processor.rs`
- `app-server/src/bespoke_event_handling.rs`

## Context

There are two related but distinct user needs:

- **Pause:** stop an in-progress turn intentionally, keep enough durable state to resume the same work later, and avoid telling the model that the user changed the request.
- **Continue:** restart work after an accidental interruption, transient stream/server error, or machine shutdown without requiring a new visible user message such as "continue".

The current TUI supports interruption well, but interruption has different semantics:

- `Esc` during a running task sends `Op::Interrupt`.
- `Op::Interrupt` aborts the active task with `TurnAbortReason::Interrupted`.
- core records a durable user-role `<turn_aborted>` marker that says the user interrupted on purpose and that partial tool side effects should be verified.
- the TUI shows an error-like "Conversation interrupted" message and waits for the next user turn.
- if the user types "continue", that text becomes a normal user message and starts a new turn.

That behavior is correct for "stop, I want to redirect the work." It is not correct for "pause this and resume later" or "that interruption was accidental; keep going."

## Current Behavior On This Base

### Slash commands

`tui/src/slash_command.rs` defines built-in commands in popup order. Commands are routed through:

1. `SlashCommand`
2. `bottom_pane/slash_commands.rs`
3. `bottom_pane/command_popup.rs`
4. `bottom_pane/chat_composer.rs`
5. `ChatWidget::dispatch_command(...)`

Commands that can run while a task is active must return `true` from `SlashCommand::available_during_task()`.

### Mid-turn `Esc`

`BottomPane::handle_key_event(...)` currently treats `Esc` as interrupt when all of the following are true:

- no modal view is active.
- no composer popup is active.
- `is_task_running` is true.
- a status indicator exists.

The status indicator sends `AppEvent::CodexOp(Op::Interrupt)`.

This happens before the composer handles the key, so a bare `Esc` during active work never becomes normal text editing, command completion, or idle backtracking.

### Interrupt semantics

`Op::Interrupt` is handled in `core/src/codex.rs` by aborting the current task. `Session::handle_task_abort(...)` in `core/src/tasks/mod.rs` then records:

```text
<turn_aborted>
The user interrupted the previous turn on purpose. If any tools/commands were aborted, they may have partially executed; verify current state before retrying.
</turn_aborted>
```

That marker is intentionally user-role prompt content. It affects the next model turn and is persisted into the rollout.

### Queued user input

When a user submits text while a turn is running, `Session::inject_input(...)` stores the input in the active turn. `run_turn(...)` drains pending input inside the same logical turn and replays it into prompt history.

That is useful for explicit follow-up input. It is not a good fit for `/continue`, because the desired behavior is specifically to avoid adding a new user message to the conversation history.

### Resume after TUI restart

`codex resume ...` reconstructs in-memory history from rollout items. It does not automatically restart unfinished work. After an interrupted turn is resumed, the TUI is idle at the interrupted state, and the only current way to keep going is to submit another user input.

## Problem

Users need a control path that means:

> Continue the same unfinished work, with no additional user-visible instruction and without triggering "new user turn" side effects.

The current control path cannot express that:

- `Op::Interrupt` records "the user interrupted on purpose" into prompt history.
- normal `UserTurn` records another user message.
- closing and later resuming the TUI only reconstructs the interrupted transcript.
- partial streamed assistant text is UI state, not durable model history until an output item is complete.
- in-flight external commands cannot be frozen exactly; they can only be allowed to finish or cancelled.

## Requirements

### Pause

- Add a `/pause` slash command.
- Allow `/pause` while a task is running.
- Do not add a normal user message.
- Stop active work and persist a checkpoint that survives TUI restart.
- Do not record the existing interrupted-turn user guidance.
- Make `/continue` able to consume the checkpoint and resume work without user text.
- Keep already completed tool calls, tool outputs, reasoning items, and assistant messages in history.
- Clearly define what happens if pause occurs while a tool process is running.

### Continue

- Add a `/continue` slash command.
- Do not submit `UserTurn` with text `"continue"`.
- Resume a paused checkpoint when one exists.
- Also support best-effort continuation after an existing interrupted state or retry-exhausted stream error.
- Work after `codex resume [conversation_id]`, not only while the original TUI stays open.
- Avoid pre-turn "new round" behaviors that are tied to a user input, especially unnecessary queue mutation and extra visible user history.

### TUI shortcut behavior

The current `Esc` behavior is a common source of accidental interruption. The proposal should choose one of:

- change bare `Esc` during active work from interrupt to pause.
- keep `Esc` as interrupt and add a separate pause shortcut.

The design should preserve current `Esc` behavior for popups, modal views, and idle backtracking.

## Non-goals

- Do not attempt to freeze an OS process and later thaw it.
- Do not promise byte-exact model stream continuation after a network disconnect unless the backend exposes a resumable stream contract.
- Do not add new app-server v1 API surface.
- Do not convert every interruption into pause; explicit interrupt remains valuable when the user wants to redirect or stop unsafe work.

## Success Criteria

The design is successful if:

- `/pause` is a recoverable control action distinct from `/interrupt`.
- `/continue` does not create a visible user message.
- accidental `Esc` recovery no longer requires typing "continue".
- `codex resume ...` can continue a checkpointed turn.
- the proposal stays grounded in current TUI and core control flow.
- implementation can be split into small, rebase-friendly commits.
