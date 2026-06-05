# Design Proposal

## Target Base

This proposal targets the `15a9f932c49d78c8b5db4625bd542165b30e3c9b` code shape.

The goal is to add two user-facing controls:

- `/pause`: stop active work in a recoverable state.
- `/continue`: resume paused or accidentally interrupted work without adding a normal user message.

## Design Summary

- Add `SlashCommand::Pause` and `SlashCommand::Continue`.
- Add core control operations `Op::Pause` and `Op::Continue`.
- Treat pause as a separate lifecycle from interrupt.
- Persist a pause checkpoint as a non-prompt rollout event.
- Add a continuation task path that re-enters the sampling loop without recording new user input.
- On `codex resume ...`, reconstruct the latest pending pause/interruption checkpoint from rollout metadata.
- Change bare `Esc` during active work to request pause, not interrupt.
- Keep `Ctrl+C` as the explicit interrupt path during active work.
- Keep popup/modal `Esc` handling and idle `Esc` backtracking unchanged.

The key distinction is:

- interrupt means "stop this turn; the user may redirect next."
- pause means "stop work now; continue the same turn later."
- continue means "start the unfinished work again without a new user message."

## Current Control Flow To Preserve

### TUI command dispatch

Built-in slash commands are declared in `tui/src/slash_command.rs`. They become selectable through the command popup and are dispatched by `ChatWidget::dispatch_command(...)`.

The new commands should follow the existing path:

```rust
SlashCommand::Pause => {
    self.app_event_tx.send(AppEvent::CodexOp(Op::Pause));
}

SlashCommand::Continue => {
    self.app_event_tx.send(AppEvent::CodexOp(Op::Continue));
}
```

`/pause` should be available during a task. `/continue` should be available while idle, and should report a friendly "nothing to continue" message when no checkpoint exists.

### Core task execution

Current `Op::UserTurn` handling:

1. builds a `TurnContext`.
2. tries to inject input into an active task.
3. if no task is active, records settings updates and spawns `RegularTask`.
4. `RegularTask` calls `run_turn(...)`.
5. `run_turn(...)` records the user prompt, then enters the sampling loop.

`/continue` must not use this path directly, because the first durable action in `run_turn(...)` is recording the user prompt.

The implementation should split `run_turn(...)` into two layers:

```rust
pub(crate) async fn run_turn(
    sess: Arc<Session>,
    turn_context: Arc<TurnContext>,
    input: Vec<UserInput>,
    cancellation_token: CancellationToken,
) -> Option<String> {
    if input.is_empty() {
        return None;
    }

    record_initial_user_input(...).await;
    run_turn_sampling_loop(sess, turn_context, cancellation_token).await
}
```

Then add:

```rust
pub(crate) async fn continue_turn(
    sess: Arc<Session>,
    turn_context: Arc<TurnContext>,
    mode: ContinuationMode,
    cancellation_token: CancellationToken,
) -> Option<String> {
    prepare_continuation_history(...).await;
    run_turn_sampling_loop(sess, turn_context, cancellation_token).await
}
```

This keeps the sampling, tool, compaction, pending-input, and hook logic shared while avoiding a fake user message.

## Pause Semantics

### Add a pause operation

Extend `Op` in `protocol/src/protocol.rs`:

```rust
Pause,
Continue,
```

`Pause` aborts active work differently from `Interrupt`.

Do not implement pause as a thin alias for `Op::Interrupt`, because the current interrupted path deliberately records user-role guidance into prompt history.

### Add a pause lifecycle event

Add a new event instead of overloading `TurnAborted(Interrupted)`:

```rust
TurnPaused(TurnPausedEvent),
TurnContinued(TurnContinuedEvent),
```

Recommended payloads:

```rust
pub struct TurnPausedEvent {
    pub turn_id: String,
    pub reason: TurnPauseReason,
}

#[serde(rename_all = "snake_case")]
pub enum TurnPauseReason {
    UserRequested,
    EscShortcut,
}

pub struct TurnContinuedEvent {
    pub continued_from_turn_id: Option<String>,
}
```

Do not persist these custom fork-only events into rollout files. They are live protocol signals for connected clients only. Keeping them out of JSONL preserves compatibility with vanilla branches that do not know these `EventMsg` variants.

If retry-exhausted stream errors should be resumable after TUI restart in the first patch, add a small persisted checkpoint event too:

```rust
TurnContinuationCheckpointed(TurnContinuationCheckpointedEvent),
```

Do not rely on `EventMsg::Error` for this; errors are not persisted into rollouts on this base.

### Pause task abort

Refactor abort handling in `core/src/tasks/mod.rs` so the abort caller can choose whether to write interrupted guidance.

Current shape:

```rust
pub async fn abort_all_tasks(self: &Arc<Self>, reason: TurnAbortReason)
```

Recommended shape:

```rust
pub(crate) enum TaskStopKind {
    Interrupt,
    Replace,
    ReviewEnded,
    Pause { pause_reason: TurnPauseReason },
}
```

`TaskStopKind::Interrupt` preserves existing behavior:

- cancel the task.
- run task cleanup.
- record the `<turn_aborted>` user marker.
- emit `TurnAborted { reason: Interrupted }`.

`TaskStopKind::Pause` should:

- cancel the task.
- run task cleanup.
- close unified exec processes.
- set an in-memory pending continuation.
- emit `TurnPaused`.
- not record `<turn_aborted>`.
- not emit `TurnAborted`.

This gives pause a clean prompt surface for `/continue`.

### What pause can and cannot preserve

Pause should preserve all completed model-visible history:

- initial context.
- real user prompts.
- completed assistant messages.
- completed reasoning items.
- completed tool calls.
- completed tool outputs.
- compaction output.

Pause should not promise to preserve:

- partial text deltas that never completed as a `ResponseItem`.
- partial reasoning deltas that never completed as a `ResponseItem`.
- an OS process exactly where it was suspended.

When pausing during a running exec/tool call, the command may already have performed side effects. Pause should stop it the same way interruption stops it today, but `/continue` should resume from the last durable history item without injecting "the user interrupted on purpose" guidance.

## Continue Semantics

### Continuation checkpoint

Maintain a lightweight pending continuation checkpoint in session state:

```rust
pub(crate) struct ContinuationCheckpoint {
    pub continued_from_turn_id: Option<String>,
    pub source: ContinuationSource,
    pub turn_context: Option<TurnContextItem>,
}

pub(crate) enum ContinuationSource {
    Paused,
    Interrupted,
    StreamError,
}
```

The checkpoint is not model prompt content. It is control metadata used to decide whether `/continue` is valid and how to build the continuation turn.

Sources:

- `Paused`: created in memory by `Op::Pause`.
- `Interrupted`: derived from the last persisted `TurnAborted(Interrupted)` event or `<turn_aborted>` marker.
- `StreamError`: derived from an incomplete rollout tail or a future vanilla-compatible checkpoint format if needed.

For `codex resume ...`, derive this checkpoint by scanning rollout items during `InitialHistory::Resumed` handling:

- find the last vanilla-compatible `TurnAborted(Interrupted)` event or infer an unfinished turn from the reconstructed response history.
- clear it if a later real `UserMessage`, rollback, or new task marker supersedes it.
- recover the latest `TurnContextItem` before the checkpoint so continuation can reuse model, effort, sandbox, approval, cwd, and truncation policy where possible.
- ignore and clean old fork-only `TurnPaused`/`TurnContinued` rollout lines when encountered.

`EventMsg::TurnComplete` is not persisted today, so rollout recovery should not depend on seeing it. A later real user prompt is enough to supersede a pending checkpoint in the intended flows. If a rollout ends mid-turn because of power loss or a partial write, continue should recover to the last valid prompt point and trim dangling tool calls or broken tail records before sampling again.

### Add a continuation task

Add a `ContinuationTask` beside `RegularTask`.

It should call `continue_turn(...)` instead of `run_turn(...)`. The continuation task should:

- emit `TurnStarted`.
- reuse the shared sampling loop.
- not record a user prompt.
- emit `TurnContinued` before the first sampling request.
- consume the pending checkpoint only after the task starts successfully.

If `/continue` is invoked while a task is already running, return a warning or no-op. It should not inject input into the running task.

### Prompt preparation by source

For `ContinuationSource::Paused`:

- use the existing prompt history as-is.
- do not add a synthetic model instruction.
- do not record anything into prompt history.

For `ContinuationSource::Interrupted`:

- build the model prompt from history with the trailing `<turn_aborted>` guidance removed or ignored for this continuation request.
- best-effort delete the persisted `<turn_aborted>` marker and trailing `TurnAborted(Interrupted)` event from the rollout before appending continuation records.
- emit `TurnContinued` as a control event.
- optionally include a transient developer message in the request only:

```text
Continue the unfinished previous turn from the current transcript.
Do not treat this as a new user request.
Do not ask for confirmation solely because the turn was interrupted.
```

The transient message should not be persisted as `ResponseItem`; otherwise repeated resumes would accumulate hidden control text.

For `ContinuationSource::StreamError`:

- use the existing history as-is.
- do not add user text.
- optionally include a transient developer message that the previous stream ended before completion and the model should continue from the last completed transcript item.

### Why not submit an empty `UserTurn`

An empty `UserTurn` is attractive but wrong on this base:

- `run_turn(...)` returns early on empty input.
- normal user turn setup is tied to user prompt recording.
- making empty input special would blur "no input" with "continue previous work."

An explicit `Op::Continue` makes intent clear and avoids accidental prompt/history changes.

## TUI Behavior

### Slash command list

Add enum variants near high-use session controls:

```rust
New,
Resume,
Continue,
Pause,
Session,
```

Descriptions:

- `/pause`: "pause current turn so it can be continued later"
- `/continue`: "continue paused or interrupted work"

Availability:

```rust
SlashCommand::Pause => true,
SlashCommand::Continue => true,
```

`ChatWidget::dispatch_command(...)` should enforce runtime state:

- `/pause` while running sends `Op::Pause`.
- `/pause` while idle shows "Nothing is running."
- `/continue` while idle sends `Op::Continue`.
- `/continue` while running shows "A turn is already running."

### Recommended `Esc` behavior

Change bare `Esc` during active work from interrupt to pause.

Rationale:

- accidental `Esc` is one of the target recovery scenarios.
- `Esc` is currently easy to hit and destructive from a conversation-state perspective.
- explicit interrupt remains available through `Ctrl+C`.
- popups and modal views already consume `Esc` before the global running-task path.

Keep existing behavior for:

- command popup: `Esc` dismisses popup.
- file popup: `Esc` dismisses popup.
- approval/request-user-input views: existing view-specific handling wins.
- idle main view: `Esc` remains backtracking/edit-previous.
- transcript overlay: existing backtracking keys remain unchanged.

Update bottom-pane status hints:

- while running: show `esc to pause` and `ctrl+c interrupt`.
- while paused: show `type /continue to resume` or a compact equivalent.

The footer currently has snapshots expecting "esc to interrupt"; those should change only where the running status indicator is visible.

### Alternative shortcut if `Esc` is kept as interrupt

If preserving `Esc` interrupt is preferred, add a new shortcut in the running-task path instead:

- `Ctrl+P` is mnemonic but conflicts with popup/history navigation in some focused contexts.
- `Shift+Esc` is already used by backtracking.
- `Ctrl+S` can be terminal flow control.

Because all of those are worse ergonomically, the recommended plan is to change bare running-task `Esc` to pause and keep `Ctrl+C` for interrupt.

## App-server API

Do not add v1 API surface.

For v2, add methods only if non-TUI clients need the same behavior:

- `turn/pause`
- `turn/continue`

Shapes:

```rust
pub struct TurnPauseParams {
    pub thread_id: String,
    pub turn_id: String,
}

pub struct TurnPauseResponse {}

pub struct TurnContinueParams {
    pub thread_id: String,
}

pub struct TurnContinueResponse {
    pub turn_id: String,
}
```

If app-server support is deferred, keep the core `Op` implementation client-agnostic so v2 can wire to it later without changing semantics.

When app-server support is added:

- `turn/pause` should resolve only after `TurnPaused`.
- `turn/continue` should produce a normal active turn stream without adding a user message.
- v2 `TurnStatus` should add `paused`.
- docs in `app-server/README.md` should explain that `turn/interrupt` and `turn/pause` have different prompt/history semantics.

## Rollout And Resume

### Persisted data

Persist:

- existing vanilla-compatible response items and events.
- existing `TurnContextItem`.

Do not persist:

- a user-role "pause" message.
- a user-role "continue" message.
- `EventMsg::TurnPaused`.
- `EventMsg::TurnContinued`.
- transient developer continuation instructions.

### Resume reconstruction

Add helper logic near `record_initial_history(...)`:

```rust
fn pending_continuation_from_rollout(items: &[RolloutItem]) -> Option<ContinuationCheckpoint>
```

It should scan from the end and return the newest still-valid checkpoint.

Invalidating items:

- real user `ResponseItem::Message { role: "user", ... }` after the checkpoint.
- `EventMsg::ThreadRolledBack`.
- new `TurnContextItem` followed by a real user prompt.

Paused checkpoints are in-memory only. Resumed rollouts use vanilla-compatible interrupted markers and incomplete-tail heuristics.

## Interaction With Existing Features

### Queued user messages

`/continue` should not drain queued user messages first. It is a control action, not a queued prompt.

If queued messages exist when `/continue` starts:

- keep them queued.
- run the continuation first.
- after the continuation reaches `TurnComplete`, existing queue behavior can send the next queued user message.

### Auto-compaction notes

This branch already has pre-compact work-notes capture. Continuation should reuse that sampling loop so:

- if continuation hits auto-compact, work notes are captured as usual.
- pending user input remains deferred during work-notes capture.

### Plan mode

Continuation should preserve the active collaboration mode from the checkpointed `TurnContextItem` when possible.

For interrupted plan output, trailing partial plan deltas that were not completed are not durable. Continue from the last completed item.

### Review mode

Pause/continue targets regular turns and the post-turn completion review workflow.

- Regular turns resume through `ContinueTask`.
- Post-turn completion reviews resume by restarting the review delegate for the captured completed-turn context; they do not continue the parent conversation directly.
- Generic `/review`, compact, and user-shell tasks reject `/pause` with "Pause is not available for this task."

When a post-turn completion review is paused, core emits `TurnPaused` without `ExitedReviewMode`, so clients should not render a "review finished" banner. Clients may leave review-mode UI state on `TurnPaused` and re-enter it when `/continue` restarts the review delegate.

## Failure Handling

- `/pause` when idle: show an informational message; do not send core op.
- `/continue` with no checkpoint: show "Nothing to continue."
- `/continue` with a checkpoint but missing usable context: continue with current session defaults and warn once.
- continuation model request fails: keep the checkpoint until a normal user message supersedes it, so the user can retry `/continue`.
- pause during tool execution: cancel the process; continuation trims any dangling tool call from memory and rollout before resampling from the last valid prompt point.
- app restart after pause or power loss: no custom pause marker is expected; recovery is best-effort from vanilla-compatible rollout history.

## Implementation Sequence

1. Add protocol types:
   - `Op::Pause`
   - `Op::Continue`
   - `EventMsg::TurnPaused`
   - `EventMsg::TurnContinued`
2. Keep the new events out of `core/src/rollout/policy.rs` persistence.
3. Add pause handling in core:
   - split task stop behavior.
   - keep existing interrupt behavior intact.
   - add pause-specific event emission.
4. Extract `run_turn_sampling_loop(...)` from `run_turn(...)`.
5. Add `ContinuationTask` and `Op::Continue` handling.
6. Add rollout checkpoint recovery on resume.
7. Add TUI slash commands and dispatch.
8. Change running-task `Esc` routing from interrupt to pause.
9. Update footer/status copy and snapshots.
10. Add rollout cleanup for custom event lines, interrupted markers, and dangling incomplete tool calls.
11. Optionally wire app-server v2 methods and schema fixtures.

## Tests

### Core

- `Op::Pause` during a regular turn emits `TurnPaused`.
- `TurnPaused` and `TurnContinued` are not persisted to rollout JSONL.
- pause does not persist the interrupted `<turn_aborted>` marker.
- `Op::Interrupt` still persists the interrupted marker.
- `Op::Continue` after pause starts sampling without recording a user message.
- `Op::Continue` after interrupted marker builds prompt without that marker for the continuation request.
- `Op::Continue` best-effort removes persisted abort signals from the rollout.
- resume from an incomplete rollout tail enables `/continue` and trims dangling tool calls before sampling.
- checkpoint recovery works from resumed rollout.
- queued user input is not consumed before continuation.

### TUI

- `/pause` appears in slash popup.
- `/continue` appears in slash popup.
- `/pause` while running sends `Op::Pause`.
- `/pause` while idle shows an info message.
- `/continue` while idle sends `Op::Continue`.
- `/continue` while running shows an info message.
- `Esc` while running and no popup sends `Op::Pause`.
- `Esc` while command popup is open still dismisses the popup and sends no pause/interrupt.
- running status snapshots change from `esc to interrupt` to `esc to pause`.

### App-server, if wired

- `turn/pause` returns after paused status.
- `turn/continue` starts a turn without an added user message.
- schema fixtures include the new v2 methods and `paused` status.

## Rebase-Sensitive Surfaces

Check these first during future rebases:

- `SlashCommand` enum ordering and `available_during_task()`.
- `BottomPane::handle_key_event(...)` because it owns the global running-task `Esc` route.
- `ChatWidget::dispatch_command(...)` and `submit_op(...)`.
- `Op` matching in `submission_loop(...)`.
- `Session::spawn_task(...)`, `abort_all_tasks(...)`, and `handle_task_abort(...)`.
- the beginning of `run_turn(...)`, especially user prompt recording and pre-turn compaction.
- the main sampling loop in `run_turn(...)`, especially pending-input draining and auto-compaction.
- rollout reconstruction in `record_initial_history(...)`.
- app-server v2 method declarations in `common.rs`.

## Bottom Line

The feature should not make `/continue` a magic user prompt.

It should add a real control path:

- pause records durable non-prompt control state.
- continue consumes that state.
- no user-visible "continue" message is added.
- the sampling loop resumes from durable transcript history.
- explicit interrupt remains available for real cancellation.
