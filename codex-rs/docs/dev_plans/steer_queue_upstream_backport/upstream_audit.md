# Upstream Audit: Steer And Queue Input

## Executive summary

This branch and the upstream branch agree on the product distinction between Enter and Tab while a task is running:

- Enter is steer input: the user is adding same-turn instructions that should be processed as soon as the active regular turn can sample again.
- Tab is queued input: the user is scheduling a next-turn message that should wait until the current turn is no longer pending or running.

The important behavior difference is not whether steer input eventually changes model-visible history. It must, because accepted steer input becomes additional user input for the active turn. The difference is when and how the UI and core commit that input.

This branch renders Enter steer input into the visible conversation immediately in `tui/src/chatwidget.rs`, while core only records the same input later when `core/src/session/turn.rs` drains active-turn pending input. The upstream branch keeps Enter steer input in a pending-steer preview and renders it only after core commits a user-message item. That upstream behavior is the correct target for this branch.

## Relevant current-branch paths

|Workflow|Current-branch paths|Behavior|
|---|---|---|
|Composer Enter while configured|`tui/src/chatwidget.rs` handles `InputResult::Submitted` and calls `submit_user_message(...)`.|Enter sends immediately when steer is enabled.|
|TUI submit|`tui/src/chatwidget.rs::submit_user_message_with_overrides(...)` builds `Op::UserTurn`.|The TUI sends `Op::UserTurn`, persists `Op::AddToHistory`, and immediately adds `history_cell::new_user_prompt(...)` to visible history.|
|Composer Tab queue|`tui/src/chatwidget.rs` handles `InputResult::Queued`; `queue_user_message(...)` stores `QueuedUserMessage` when unconfigured, running, or in review mode.|Queued messages are previewed by `tui/src/bottom_pane/queued_user_messages.rs` and drain after completion or selected error paths.|
|Core user turn dispatch|`core/src/session/handlers.rs::user_input_or_turn(...)` builds a `TurnContext`, then calls `Session::inject_input(...)`.|If an active turn exists, input is appended to active-turn pending input. If not, a new `RegularTask` starts.|
|Core pending input storage|`core/src/session/mod.rs::inject_input(...)`; `core/src/state/turn.rs::TurnState::pending_input`.|The active turn accepts any injected input without validating task kind or expected active turn.|
|Core pending input drain|`core/src/session/turn.rs` drains `sess.get_pending_input().await` when `can_drain_pending_input` and pre-compact notes are idle.|This branch already has the initial guard `let mut can_drain_pending_input = input.is_empty()`, so fresh explicit input is not intentionally merged with already-pending input before the first sample.|
|Live user-message event|`tui/src/chatwidget.rs` handles `EventMsg::UserMessage(ev)` only for replay.|Live committed user-message events are ignored because the TUI already rendered on submit.|
|Pause/continue|`protocol/src/protocol.rs`, `core/src/session/handlers.rs`, `core/src/tasks/mod.rs`, `tui/src/chatwidget.rs`, and `docs/dev_plans/pause_continue/`.|`/pause` and `/continue` are branch-specific controls. Pause and task-stop paths clear active-turn pending state, so TUI-side pending-steer retention is required to avoid losing uncommitted steer input.|

## Relevant upstream paths

|Workflow|Upstream paths|Behavior|
|---|---|---|
|Composer Enter while configured|`tui/src/chatwidget/input_flow.rs` calls `submit_user_message(...)` when configured and not blocked by plan streaming or shell-command-only state.|Enter submits immediately, but active-turn submissions are treated as pending steers.|
|TUI submit|`tui/src/chatwidget/input_submission.rs` computes `render_in_history = !self.turn_lifecycle.agent_turn_running`.|Idle submissions render immediately and set `user_turn_pending_start`. Running-turn submissions create `PendingSteer` and do not render immediately.|
|Pending steer identity|`tui/src/chatwidget/user_messages.rs::pending_steer_compare_key_from_items(...)`.|Pending steers are matched against committed user-message items by flattened text plus image count.|
|Commit-later rendering|`tui/src/chatwidget.rs::on_committed_user_message(...)`.|Live committed user-message items pop the matching pending steer, refresh preview, and render the original UI-rich message exactly once.|
|Pending input preview|`tui/src/chatwidget/input_queue.rs`; `tui/src/bottom_pane/pending_input_preview.rs`.|The bottom pane displays queued messages, pending steers, and rejected steers as distinct states.|
|Rejected steer recovery|`tui/src/chatwidget/input_restore.rs`; `tui/src/chatwidget/turn_runtime.rs`.|Steers rejected by non-steerable active tasks move from pending steer to rejected-steer queue and drain before normal queued messages.|
|Interrupt recovery|`tui/src/chatwidget/input_restore.rs`; `tui/src/chatwidget/interaction.rs`.|Pending steers can be restored to the composer after interrupt, or immediately resubmitted as one fresh turn on the explicit interrupt-and-send path.|
|Queue drain gate|`tui/src/chatwidget/input_flow.rs::is_user_turn_pending_or_running(...)`.|Queue auto-drain is blocked both while a turn is running and while a user turn has been submitted but the start event has not arrived.|
|Core steer validation|`core/src/session/mod.rs::steer_input(...)`.|Core validates active turn existence, optional expected turn id, task kind, and empty input before accepting steer input.|
|Core pending input storage|`core/src/session/input_queue.rs`.|Input queue operations are centralized, can prepend blocked input, and include mailbox coordination.|
|Core dispatch|`core/src/session/handlers.rs::user_input_or_turn_inner(...)`.|`Op::UserInput` first tries `steer_input(...)`; `NoActiveTurn` falls back to spawning a new regular task; other steer errors emit an error event.|
|Core pending input drain|`core/src/session/turn.rs`.|Pending input drains after initial sampling and after model/tool continuation checkpoints; hook inspection can block and requeue remaining pending input.|
|App-server steer API|`app-server-protocol/src/protocol/v2/turn.rs`; `app-server/src/request_processors/turn_processor.rs`.|`turn/steer` requires `expectedTurnId`, validates input size, returns active turn id, and maps structured steer errors.|

## Behavior-difference classification

|Area|This branch|Upstream branch|Category|Backport priority|
|---|---|---|---|---|
|Enter steer transcript commit|Renders the prompt locally in `submit_user_message_with_overrides(...)` even while a turn is running.|Stores a `PendingSteer`; renders only when a committed user-message item arrives.|Upstream bug fix.|High.|
|Live committed user-message handling|Ignores live `EventMsg::UserMessage`; handles replay only.|Handles live and replay user-message items with duplicate protection.|Upstream bug fix.|High.|
|Prefix-cache behavior|The visible transcript changes before core commits the steer. The next model request that includes accepted steer input also has a changed model-visible suffix.|The visible transcript does not change before core commit. The next request still changes once the steer is accepted, by design.|Clarification / intended semantics.|Document and test.|
|Steer validation|`Session::inject_input(...)` accepts pending input for any active turn.|`Session::steer_input(...)` rejects missing active turn, mismatched expected turn id, empty input, review turns, and compact turns.|Upstream bug fix.|High.|
|Review and compact steering|A submitted steer can be injected into whichever task is active unless higher-level TUI queue logic prevents it.|Non-steerable tasks return `ActiveTurnNotSteerable`; TUI requeues the steer for the next regular turn.|Upstream bug fix.|High.|
|Pending steer loss on task stop|The TUI has no pending-steer state. Pause/abort paths clear active-turn pending input, so uncommitted steers can be lost or become unrecoverable.|The TUI keeps pending steers and restores or resubmits them after interruption.|Upstream bug fix adapted to branch pause semantics.|High.|
|Queue auto-drain race|Drain is gated by `bottom_pane.is_task_running()` only.|Drain is gated by `user_turn_pending_start || bottom_pane.is_task_running()`.|Upstream bug fix.|Medium-high.|
|Pending input preview|Only queued messages are shown.|Queued messages, pending steers, and rejected steers are shown separately.|New upstream feature.|Medium.|
|Queue action semantics|Queued text is submitted through the normal user-message path. Shell prompts still work via `!` handling on submission; queued slash prompts can degrade to plain text depending on how they were queued.|Queued inputs carry `QueuedInputAction::{Plain, ParseSlash, RunShell}`.|New upstream feature.|Medium.|
|App-server `turn/steer`|No equivalent branch-local backport is required for TUI direct `Op::UserTurn` operation.|Structured app-server API requires `expectedTurnId` and maps steer errors.|New upstream feature.|Optional unless app-server clients need same behavior.|
|`/pause` and `/continue`|Branch-specific user controls; bare `Esc` may pause active work.|Not part of the inspected upstream flow.|Diverged design.|Preserve this branch.|
|Initial pending-input ordering|This branch already uses `can_drain_pending_input = input.is_empty()` in `core/src/session/turn.rs`.|Upstream also uses this rule and has additional hook-aware requeue behavior.|Partially already backported / related to `docs/dev_plans/turn_history_reference_context_backport/`.|Validate with tests; do not duplicate blindly.|

## Answer to the observed behavior

The immediate visible-history mutation from Enter steer is a bug relative to the upstream branch. It should be backported as an upstream bug fix.

The later model-visible history mutation is intended. A same-turn steer is additional user input. Once core accepts and drains it, the next sampling request necessarily includes that steer, so the pre-steer request prefix and the post-steer follow-up request are not expected to be byte-identical. The backport target is therefore:

1. Do not render or otherwise commit the steer in local visible conversation history before core emits the committed user-message item.
2. Preserve deterministic ordering: fresh explicit user prompt first, then pending steer follow-up after a successful sample, then queued next-turn input only after the current turn is no longer pending or running.
3. Preserve uncommitted steer text across pause, interrupt, review rejection, compact rejection, and recoverable error paths. In-place thread-switch restoration is only required if that upstream app-server workflow is backported later.
