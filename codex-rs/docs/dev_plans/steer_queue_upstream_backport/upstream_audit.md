# Upstream Audit

## Audit scope

The comparison follows user input from the composer through TUI queue state, protocol dispatch, core active-turn state, model request construction, task completion, interruption, pause, and continuation.

Primary this-branch paths:

- `tui/src/bottom_pane/chat_composer.rs`
- `tui/src/chatwidget/input_submission.rs`
- `tui/src/chatwidget/input_flow.rs`
- `tui/src/chatwidget/input_queue.rs`
- `tui/src/chatwidget/input_restore.rs`
- `tui/src/chatwidget/turn_runtime.rs`
- `tui/src/chatwidget/user_messages.rs`
- `core/src/session/handlers.rs`
- `core/src/session/mod.rs`
- `core/src/session/turn.rs`
- `core/src/tasks/mod.rs`
- `core/src/state/turn.rs`
- `core/tests/suite/pending_input.rs`

Primary upstream paths:

- `tui/src/bottom_pane/chat_composer.rs`
- `tui/src/bottom_pane/chat_composer/slash_input.rs`
- `tui/src/chatwidget/input_submission.rs`
- `tui/src/chatwidget/input_flow.rs`
- `tui/src/chatwidget/input_restore.rs`
- `tui/src/chatwidget/input_queue.rs`
- `tui/src/chatwidget/interaction.rs`
- `tui/src/app/thread_routing.rs`
- `tui/src/app_server_session.rs`
- `app-server-protocol/src/protocol/v2/turn.rs`
- `app-server/src/request_processors/turn_processor.rs`
- `core/src/session/input_queue.rs`
- `core/src/session/mod.rs`
- `core/src/session/turn.rs`
- `core/src/tasks/mod.rs`
- `core/tests/suite/pending_input.rs`
- `core/src/session/tests.rs`

## End-to-end workflow comparison

### Idle user input

Both branches render an idle submission immediately, mark a user turn as pending start, and block queue auto-send until the turn starts or terminates.

This branch sends `Op::UserTurn` directly to core. Upstream constructs an `AppCommand::UserTurn`; the app layer routes it to app-server `turn/start` when no active turn id is cached.

### Enter during an active regular turn

Both branches now use commit-later TUI semantics:

1. Build the rich `UserMessage` and protocol items.
2. Store a `PendingSteer` keyed by flattened text and image count.
3. Submit the input without rendering a committed user history cell.
4. Render the original rich payload when the committed user-message event arrives.

The transport differs:

- This branch sends another `Op::UserTurn`, applies turn settings, then core decides whether to steer.
- Upstream routes through app-server `turn/steer(expectedTurnId)`, which atomically validates active-turn identity and sends no turn-start settings.

### Tab or queue-mode submission

This branch parses slash commands before deciding to queue. The queued record contains text and rich input data, but no deferred action type.

Upstream takes the queue branch first and records:

```rust
QueuedInputAction::Plain
QueuedInputAction::ParseSlash
QueuedInputAction::RunShell
```

Slash validation and shell execution happen only when the item reaches the front of the idle queue. The drain loop can process informational commands and continue to the next item without starting multiple turns.

### Pending input inside core

This branch stores `ResponseInputItem` values in `TurnState::pending_input`. Normal drain converts them to `ResponseItem`, records user items, and emits committed lifecycle events. Original text-element/client identity is no longer available after conversion.

Upstream stores typed `TurnInput` values. `TurnInput::UserInput` preserves the original `Vec<UserInput>` and optional client id. The same `record_pending_input(...)` helper is used by normal drain and task-finish cleanup.

### Model request ordering

Both turn loops defer pending input before the initial model sample. Both also defer a steer when model/tool continuation must resume after compaction.

Upstream adds stronger regression coverage. `user_input_does_not_preempt_after_reasoning_item` snapshots the first and second request inputs. The second request begins with the entire first request input, then appends reasoning, tool call, assistant output, tool output, and finally the steer.

This branch's tests verify inclusion/exclusion and compaction ordering, but do not assert exact prefix equality or the full response-item suffix.

### Natural task completion with undrained input

This branch persists leftover pending response items to conversation history but emits no committed user-message lifecycle.

Upstream preserves typed pending input and emits, before `TurnComplete`:

1. `RawResponseItem`;
2. `ItemStarted(UserMessage)`;
3. `ItemCompleted(UserMessage)`;
4. legacy `UserMessage` with text elements/client id;
5. `TurnComplete`.

This difference closes a duplicate-resubmission race in the TUI.

### Review, compact, and user-shell work

Both branches reject steering for review and compact tasks and move the pending steer to a rejected queue. This branch additionally has `PostTurnCompletionReview` and standalone `UserShell` task kinds and rejects both.

Upstream avoids one rejection path in the TUI: ordinary input is queued immediately when only user-shell commands are running. It also queues Enter while a proposed-plan item is still streaming.

### Interrupt and pause

This branch distinguishes:

- `/pause` or bare Esc: checkpoint and recover pending steers for `/continue`;
- Ctrl+C interrupt: end the current turn and restore pending steers;
- ordinary queued messages remain queued.

Upstream has no equivalent `/pause`/`/continue` workflow. Its interrupt behavior has two modes:

- an explicit interrupt with pending steers can merge and immediately resubmit only those steers as a fresh turn;
- a normal interrupted-turn restore can merge pending, rejected, queued, and composer content into one draft.

These are design differences, not unconditional fixes for this branch.

## Behavior-difference classification

|Area|This branch|Upstream branch|Classification|Disposition|
|---|---|---|---|---|
|Pending steer render|Commit-later render on committed `UserMessage`.|Same, with richer remote-image/mention support.|Other: already backported.|Keep current implementation.|
|Non-steerable tasks|Rejects review, post-turn review, compact, and standalone user shell.|Rejects review and compact; shell-only input is normally queued before core.|Other: already backported plus branch extension.|Keep branch task-kind coverage.|
|Pending-start queue gate|Uses `user_turn_pending_start || task_running`.|Same.|Other: already backported.|Keep.|
|Steer feature toggle|Enter can be configured to queue instead of steer.|Steering is graduated to always-on; queue is an explicit binding.|Diverged design.|Preserve branch toggle.|
|Queued slash commands|Parsed/rejected/dispatched before `should_queue` is honored.|Validation and dispatch are deferred with `ParseSlash`.|Bug fixed upstream.|Done.|
|Queued shell commands|Text is reinterpreted on eventual submission, but intent is not explicit.|Stored as `RunShell`; execution occurs at dequeue.|Upstream hardening/useful feature.|Done with the slash fix.|
|Queue drain after non-turn commands|Submits one queued record per call.|Loops past commands that do not start a turn or open a blocking UI.|Useful upstream feature.|Done with action fidelity.|
|Queue editor and per-message model/effort|Queue popup; arbitrary-entry editing; reordering, deletion, move-to-front, and send-next controls; per-message model/effort overrides.|Basic pop-back editing of the latest queued or rejected message; no equivalent queue manager, reordering/deletion controls, or per-message overrides.|Diverged design / this branch is richer.|Preserve this branch's queue UX and data model.|
|Plan stream submission|Active plan stream does not force queueing.|Enter is queued while a plan item is streaming.|Bug fixed upstream.|Done.|
|Only user-shell commands running|Input can become a pending steer and then be rejected.|Ordinary input is queued before submission.|Bug fixed upstream.|Done.|
|Active turn identity|No expected id; core steers whichever task is active.|`turn/steer` requires `expectedTurnId`; TUI repairs one stale-id race.|Bug fixed upstream.|Done through direct-core `Op::SteerInput`.|
|Start/steer settings separation|`Op::UserTurn` applies session settings before steer validation.|Active TUI steer sends only steer input; settings are sent only by `turn/start`.|Bug fixed upstream.|Done.|
|Mode change while active|`submit_user_message_with_mode(...)` changes mode and submits.|Rejects a collaboration-mode change while a turn is running.|Bug fixed upstream.|Done.|
|App-server `turn/steer` API|Absent.|Supports expected id, client id, additional context, and response metadata.|New upstream feature.|Backport only the semantics needed by direct core; external API is optional.|
|Pending input representation|`Vec<ResponseInputItem>` loses original user-input metadata.|Typed `TurnInput::UserInput` preserves content/client id.|Bug fixed upstream.|Done, with client id reserved for optional follow-up.|
|Leftover pending input at task finish|Persisted silently, then `TurnComplete`.|Committed lifecycle emitted before `TurnComplete`.|Bug fixed upstream.|Done.|
|Pending-input hooks/mailbox wakeups|No equivalent centralized flow.|Input queue integrates user-prompt hooks, mailbox delivery, and activity wakeups.|New upstream feature.|Out of current scope except typed storage.|
|Normal steer request ordering|Appears aligned: pending input drains only after the initial sample and after required continuation.|Same, with stronger snapshots.|Other: aligned behavior, weaker tests here.|P0 test backport.|
|Rejected steer drain|One rejected message per future turn.|All rejected steers are merged into one next-turn message.|Upstream bug fix/design improvement.|Done.|
|Explicit interrupt with pending steers|Restores to composer or rejected queue; normal queue remains queued.|Can interrupt and immediately submit merged pending steers.|New upstream feature / diverged design.|Optional for Ctrl+C only.|
|Normal interrupt restore|Ordinary queue remains intact.|May merge pending, rejected, queued, and current draft into composer.|Diverged design.|Do not copy for `/pause`; retain explicit queue semantics.|
|Cancel-edit initial prompt|No equivalent special restoration path.|Can restore the initial prompt if interrupted before visible turn activity.|New upstream feature.|Optional and independent.|
|Thread-switch input state|Single active TUI state.|Captures/restores composer, pending, rejected, queued, and lifecycle state per thread.|New upstream feature.|Out of scope without thread switching.|
|Queue auto-send suppression|No dedicated suppression bit.|Can suppress drain during transitions.|Useful upstream hardening.|Done for branch-local transition windows.|
|Abort cleanup order|Clears pending before task cancellation and clears an empty active turn.|Cancels first, then clears when a task was actually aborted; preserves empty-turn pending input.|Bug fixed upstream.|Done.|
|Pending-steer correlation|Flattened text plus image count.|Same; protocol has unused client id support.|Shared residual risk.|P2 client-id improvement.|
|Expected-id error transport|No expected-id error.|App-server returns mismatch text and TUI parses the message string.|Upstream-only fragility, not a confirmed behavior regression.|Do not copy; use structured local errors.|
|`/pause` and `/continue`|Implemented as branch-specific lifecycle.|Absent from the inspected upstream flow.|Diverged design.|Preserve and integrate.|

## Confirmed upstream-only regressions

No behavior regression was confirmed from the inspected steer/queue paths.

Two upstream-only risks should not be copied:

- The TUI recovers `ExpectedTurnMismatch` by parsing the exact app-server error message text because the mismatch response has no structured data payload.
- The app-server protocol accepts `clientUserMessageId`, but the upstream TUI currently sends `None`, leaving pending-steer matching content-based.

These are implementation fragilities or incomplete adoption, not reproduced user-visible regressions.

## Prefix-cache analysis

### Intended normal-path sequence

Let `I0` be the parsed `input` array sent for the active request. After the model produces durable items `R` and core accepts steer message `S`, the next request should satisfy:

```text
I1 = I0 ++ R ++ [S]
```

Thus:

```text
I1[0..len(I0)] == I0
```

The steer is not inserted before `R` and does not erase `R`. "Process right away" means the next safe model sampling boundary, not cancellation of the current response stream.

### Evidence in both branches

- This branch initializes `can_drain_pending_input` from `input.is_empty()` in `core/src/session/turn.rs`; fresh turn input is sampled before pending input.
- This branch defers pending input through its pre-compact/model-follow-up states.
- Upstream documents the same two deferral cases and snapshots the no-preemption sequence in `core/tests/suite/pending_input.rs`.

### Cases where exact prefix is not required

- automatic or manual compaction;
- context-window truncation;
- rollback or history reconstruction;
- an explicit environment, permissions, developer-instruction, or collaboration-mode history update;
- a model/provider/tool-schema change that changes the provider's cache key even if the `input` array itself remains prefixed.

The current steer path should not itself cause these changes. Separating steer from `SessionSettingsUpdate` reduces the chance that a nominal steer mutates future context accidentally.

## Recommended backport boundary

Backport behavior, not architecture:

- Add a direct-core steer operation carrying expected turn id and optional client id.
- Add typed pending user input and commit lifecycle parity.
- Add queued action fidelity and drain semantics to the existing editable queue.
- Add the upstream ordering tests and selected lifecycle guards.
- Keep the branch's pause/continue and queue-management UX.
