# Design Proposal

## Design goals

1. Make turn start and turn steer distinct operations.
2. Make accepted steer input commit exactly once, including the task-finish race.
3. Preserve exact normal-path request-prefix ordering.
4. Preserve queued input intent until dequeue.
5. Retain this branch's pause/continue lifecycle and editable queue.
6. Avoid importing the upstream app-server-first TUI architecture.

## 1. Dedicated steer transport

### TUI state

Track the active core turn id in `ChatWidget`:

```rust
active_turn_id: Option<String>,
```

Use the live `Event.id` attached to `EventMsg::TurnStarted` as the turn id. Clear it on `TurnComplete`, `TurnAborted`, `TurnPaused`, terminal error, and failed start. Replay events must not establish a live active turn id.

Change the event dispatch path so turn lifecycle handlers receive the live event id:

```rust
EventMsg::TurnStarted(_) => self.on_task_started(id),
```

### Protocol operation

Add a branch-local operation rather than overloading `Op::UserTurn`:

```rust
Op::SteerInput {
    expected_turn_id: String,
    items: Vec<UserInput>,
    client_user_message_id: Option<String>,
}
```

`Op::UserTurn` remains the only path that carries new-turn settings such as cwd, approval policy, sandbox policy, model, reasoning effort, collaboration mode, personality, and service tier.

### Core validation

Extend `SteerInputError`:

```rust
enum SteerInputError {
    NoActiveTurn(Vec<UserInput>),
    ExpectedTurnMismatch { expected: String, actual: String },
    ActiveTurnNotSteerable { turn_kind: NonSteerableTurnKind },
    EmptyInput,
}
```

Core must hold the active-turn lock while it:

1. verifies a running task exists;
2. reads the actual active turn id;
3. compares it with `expected_turn_id`;
4. validates task kind;
5. appends the typed input to that turn's pending queue.

Do not apply `SessionSettingsUpdate` on this path.

### Structured error mapping

Add structured `CodexErrorInfo` variants for the two races rather than parsing message text:

```rust
NoActiveTurnToSteer,
ExpectedTurnMismatch { expected: String, actual: String },
```

Retain `ActiveTurnNotSteerable { turn_kind }`.

TUI recovery:

|Error|TUI action|
|---|---|
|`NoActiveTurnToSteer`|Clear stale active id. Convert the pending steer to a normal queued/startable input without rendering it twice.|
|`ExpectedTurnMismatch`|Update the cached id and retry once only when the replacement task is still eligible for steering; otherwise queue/reject.|
|`ActiveTurnNotSteerable`|Move the matching pending steer to rejected-steer state.|
|Transport failure|Restore the pending steer to composer/rejected state; do not mark it committed.|

A simpler first patch may queue on any expected-id mismatch instead of retrying. Correct non-misdirection is more important than matching upstream's retry behavior exactly.

### Collaboration mode

Adopt the upstream guard in `submit_user_message_with_mode(...)`:

- if a turn is active and the requested collaboration mask differs from the active mask, show an error and do not submit;
- if it matches, submit through `Op::SteerInput` without changing session mode;
- mode changes remain legal for idle queued/new turns.

## 2. Typed pending input and exact commit lifecycle

### Core representation

Replace user-originated pending `ResponseInputItem` values with a typed enum:

```rust
enum TurnInput {
    UserInput {
        content: Vec<UserInput>,
        client_id: Option<String>,
    },
    ResponseItem(ResponseInputItem),
}
```

Only the user-input variant is required for this backport. Upstream mailbox variants are not required.

Store `Vec<TurnInput>` in `TurnState` and expose turn-state-specific take/extend helpers. Keeping helpers scoped to the captured `Arc<Mutex<TurnState>>` avoids accidentally draining a replacement active turn.

### Normal drain

For `TurnInput::UserInput`, call the existing committed user-prompt helper with the original `UserInput` values and client id. This preserves text elements and ensures the same events are emitted as for an ordinary user prompt.

For `TurnInput::ResponseItem`, preserve the existing conversation-item recording path.

### Task-finish drain

When the last regular task finishes naturally:

1. detach/remove the task while retaining its turn-state handle;
2. take leftover `TurnInput` from that exact turn state;
3. record and emit each accepted user input through the same helper as normal drain;
4. emit `TurnComplete` only after those commit events;
5. clear the active turn.

Do not silently persist a user input without emitting its commit lifecycle.

For pause or explicit abort, the server-side pending steer is discarded and the TUI remains responsible for restoring uncommitted pending steers. Do not emit a committed user-message event for discarded input.

### Client ids

Generate a stable id in the TUI when creating `PendingSteer`:

```rust
struct PendingSteer {
    client_id: String,
    target_turn_id: String,
    user_message: UserMessage,
    history_record: UserMessageHistoryRecord,
    compare_key: PendingSteerCompareKey,
}
```

Add the id to committed user-message events and structured steer rejection error events. Match by client id first and retain the current compare key only as a compatibility fallback for replay or old core events.

Client-id adoption can be split into P2 if minimizing the first patch. Typed input and task-finish lifecycle must remain P0.

## 3. Queue action fidelity

### Queue envelope

Extend the existing editable `QueuedUserMessage` instead of replacing it with upstream's simpler queue state and basic latest-item edit flow:

```rust
enum QueuedInputAction {
    Plain,
    ParseSlash,
    RunShell,
}

struct QueuedUserMessage {
    id: u64,
    text: String,
    local_images: Vec<LocalImageAttachment>,
    text_elements: Vec<TextElement>,
    mention_paths: HashMap<String, String>,
    model_override: Option<String>,
    effort_override: Option<Option<ReasoningEffortConfig>>,
    action: QueuedInputAction,
    pending_pastes: Vec<(String, String)>,
    history_record: UserMessageHistoryRecord,
}
```

Embedding the history record removes the need for another parallel deque. Existing rejected-steer parallel state can be migrated separately.

### Composer ordering

In `handle_submission_with_time(...)`, evaluate `should_queue` before immediate slash dispatch:

1. flush paste burst state as needed;
2. inspect the raw draft to decide whether slash validation must be deferred;
3. prepare the queued payload without dispatching or reporting slash errors;
4. classify it as `ParseSlash`, `RunShell`, or `Plain`;
5. return `InputResult::Queued`.

Only the non-queue branch calls `try_dispatch_bare_slash_command(...)` or `try_dispatch_slash_command_with_args(...)`.

Required classification:

- slash-led, non-leading-space input with slash commands enabled: `ParseSlash`;
- bang-shell input: `RunShell`;
- leading-space slash and all other text: `Plain`.

### Dequeue loop

`maybe_send_next_queued_input()` should continue until one of these occurs:

- a user/model turn is submitted;
- a shell task is submitted;
- a command opens a modal/popup or otherwise blocks interaction;
- the queue is empty.

Informational commands and unknown-command diagnostics may return `QueueDrain::Continue` so the next queued item can be considered in the same idle pass.

At most one new turn/task may start per drain call.

### Queue overrides

- `Plain`: apply the stored model/reasoning overrides when starting the turn.
- `RunShell`: overrides are not applicable; the queue UI should not offer them for this action.
- `ParseSlash`: command semantics own model/mode changes. The queue UI should not offer per-message overrides for this action; if parsing later falls back to a diagnostic or literal handling path, it should not retain hidden overrides.

Queue editing must retain action and paste metadata when moving between drafts.

## 4. Request-ordering invariant

The turn loop remains non-preemptive. Do not cancel an active model stream merely because steer input arrived.

For a stable turn without compaction or context rewrite:

```rust
assert_eq!(
    &follow_up_input[..initial_input.len()],
    initial_input.as_slice(),
);
```

The expected suffix is:

1. all durable response items emitted by the previous sample;
2. required tool output items;
3. the committed steer user item.

When auto-compaction occurs:

- if the old model/tool task still needs continuation, continue it before draining the steer;
- if only the steer requires another sample, compact first and then drain the steer without an empty continuation request.

The existing branch logic is close to this behavior; the primary backport is test strength and preservation during the typed-input refactor.

## 5. Rejected-steer and queue policy

### Merge rejected steers

When idle, drain all rejected steers into one message before ordinary queued input. Rebase text-element ranges, image placeholders, and mention mappings using the existing `merge_user_messages(...)` utilities.

This produces one next-turn correction rather than one turn per rejected Enter press.

### Plan stream guard

Expose a predicate equivalent to:

```rust
fn is_plan_streaming_in_tui(&self) -> bool {
    self.plan_stream_controller.is_some()
}
```

Enter during an active plan stream goes to the ordinary queue instead of pending-steer state. This keeps a streamed proposed-plan cell and the next user turn from interleaving.

### User-shell guard

When only standalone/auxiliary user-shell commands are active, ordinary model input goes to the queue. A new bang-shell action may still be handled according to existing shell concurrency rules.

## 6. Interrupt, pause, and continue integration

### `/pause`

Preserve current behavior:

- core discards active pending steer input as part of pausing;
- TUI restores uncommitted pending steers to the composer or rejected state;
- ordinary queued messages remain queued;
- no pending steer is automatically submitted;
- `/continue` remains available.

Do not port upstream's generic "merge every queued draft into composer" interrupt behavior into pause.

### `/continue`

- Set `user_turn_pending_start` before submitting `Op::Continue`.
- Do not render a user prompt.
- Do not drain the queue until `TurnStarted`, `TurnPaused`, `TurnComplete`, `TurnAborted`, or a terminal failure resolves the pending-start state.

### Explicit interrupt

An optional later feature may adopt upstream's interrupt-and-submit behavior:

- if Ctrl+C is invoked specifically while pending steers exist, set a one-shot flag;
- after `TurnAborted`, merge and submit only pending steers as one fresh turn;
- leave ordinary queued messages and the current composer draft untouched;
- never use this path for `/pause`.

## 7. Abort cleanup

Refactor current task stopping so pending state is not cleared before cancellation is observed:

1. take the active task/turn handle;
2. cancel and run task-specific abort cleanup;
3. emit abort lifecycle;
4. clear pending approvals/input for a turn that actually had an aborted task;
5. leave captured pending input intact if the active-turn shell contained no task.

TUI restoration remains the source of truth for user steers discarded by explicit abort/pause.

## 8. Queue auto-send suppression

Add a narrow `suppress_queue_autosend` flag to input state. Use it only around transitions that can temporarily look idle while state is being restored, such as:

- pause/interrupt finalization;
- queue edit restore;
- session replay/resume initialization;
- any future thread switch.

Clear it deterministically before the final explicit drain check.

## State transition summary

|State|Enter with steering enabled|Queue key|Core result|Next state|
|---|---|---|---|---|
|Idle|`Op::UserTurn`; render immediately; pending start|Queue or immediate submit according to existing idle queue-key behavior|Turn starts|Running|
|Running regular|`Op::SteerInput(expected_id)`; pending preview only|Store deferred queue envelope|Accepted|Pending steer until commit|
|Running review/compact/user-shell|Steer attempt may be avoided by TUI guard; otherwise pending preview|Store queue envelope|Structured non-steerable rejection|Rejected steer|
|Plan item streaming|Ordinary queue|Ordinary queue|No steer attempt|Queued|
|Paused|Restore pending steer; keep queue|Store queue envelope|No active turn|Paused/idle|
|Continuation pending start|Queue|Queue|`TurnStarted` or terminal event|Running or idle|
|Natural completion with leftover accepted steer|No TUI action|No TUI action|Commit lifecycle before `TurnComplete`|Pending steer removed, no duplicate|

## Non-goals

- Porting upstream mailbox scheduling, sleep wakeups, multi-agent thread channels, or remote image handling.
- Matching upstream's string-parsed app-server error behavior.
- Flattening the editable queue into upstream's simpler queue state and latest-item-only editing model.
