# Plan: efficient waits and background completion

Status: proposed runtime work. The current shell watcher emits execution events; it does not schedule model inference after an idle parent. Longer empty waits can ship independently of completion delivery and automatic continuation. Preserve the existing [pause/continue](../../features/pause-continue.md), [input ownership](../../features/steering-and-queue.md), [history replay](../../features/history-replay.md), and [completion review](../../features/post_turn_completion_review/README.md) contracts.

## Problem and scope

A long-running shell job can force repeated model invocations just to call `write_stdin` again. Waiting inside the runtime costs no model tokens by itself; the extra requests occur when the wait returns and the model chooses another action. The current empty-input path applies the same short maximum as interactive input. Its collector also repeatedly drains bounded process buffers into an unbounded per-wait `Vec<u8>`, so extending the deadline alone can increase memory use substantially.

The first milestone changes empty shell waits and their collection bounds. Later milestones add explicit registration and delivery of terminal shell outcomes, then permit an eligible waiting owner to continue without polling. Native-agent notifications, a general durable workflow engine, detached processes surviving application exit, and unrestricted wake-up of idle sessions are outside this plan.

## Milestone 1: longer waits with bounded collection

1. Give empty `write_stdin` requests a separate maximum wait, using 300 seconds as the initial upstream-aligned candidate. Keep initial execution and nonempty interactive stdin on their existing short bounds. Do not first apply the interactive clamp and then attempt to raise it.
2. Replace the unbounded per-wait accumulator with bounded collection. Keep observed-byte accounting separate from retained bytes and preview size. Longer deadlines must not enlarge memory in proportion to command output. Recoverable full output belongs to the [output-artifact plan](recoverable_tool_output.md), not this accumulator.
3. Preserve early completion after process exit and bounded trailing-output drainage. Make interruption and shutdown responsive through the existing cancellation path. A long requested wait is a maximum, not a requirement to wait after the process has exited; receiving a progress chunk alone need not start another inference.
4. Update the actual handler, tool schema/description, and any host wrapper that imposes another yield ceiling together. A prompt requesting a longer wait cannot bypass a runtime clamp. A host incapable of keeping a call pending must advertise its effective shorter bound rather than imply the full duration is available.
5. Keep process identity, exit status, stdin behavior and tool-call/result pairing unchanged. When a command still runs at the deadline, return the ordinary running result; do not fabricate completion or an additional user turn.

Do not tune the wait against an assumed universal cache TTL. Choose its maximum using interruption responsiveness, host timeout behavior, completion latency and measured invocation count. Report cached and uncached usage separately in the [evaluation](evaluation_plan.md).

## Milestone 2: explicit completion delivery

Keep the following boundaries distinct:

|Boundary|Responsibility|
|---|---|
|Process completion|Observe exit or termination and finish output capture within its bounded finalization policy.|
|Completion delivery|Persist a terminal outcome and admit one contextual notification for the registered owner.|
|Model continuation|Reserve an eligible owner and construct its next model request after normal input, cancellation and recovery checks.|

An `ExecCommandEnd` event alone satisfies only the first boundary. A UI event, a queued notification and a consumed model input are not interchangeable evidence.

### Registration and ownership

- Completion delivery is opt-in for a specific shell job. The model-facing registration must identify which job result the current task needs and acknowledge acceptance in an ordinary tool result. Finalize the smallest compatible tool-schema change before implementing it; no such registration argument or tool exists today.
- Assign a stable job identity distinct from the reusable process/session handle. Retain the source conversation, original call identity and owning request/continuation generation in host metadata. Never recover ownership by parsing assistant prose, process command text or a guessed recent user message.
- Registration must become durable before promising delivery. Record its completed tool-call/acknowledgment pair before installing an idle waiting checkpoint; no unanswered `wait` call may be carried across that boundary. If persistence fails before launch, fail without launching. If launch already occurred, report the launched job and unavailable registration accurately; do not silently relaunch it to repair registration.
- Unregistered jobs retain current explicit polling behavior. Jobs whose owner was completed, replaced or abandoned remain inspectable according to output retention, but cannot start inference for an unrelated request. Forks may read an authorized source artifact; they do not inherit the source conversation's wake-up ownership.
- Do not change command approvals or sandbox capabilities. Delivery only reports work that was already admitted through normal execution policy.

### Terminal outcome and artifact integration

Persist the job's terminal state before queue admission. It should identify the source job/call, terminal outcome, available exit code, owner generation and output artifact metadata. Use the output-artifact contract's actual completeness states; include observed/retained counts or unavailable status where relevant. Keep the model notification bounded, with a locator and an optional short excerpt, rather than injecting the full log.

Process completion and output completeness are independent: an exit code of zero can have a capped or failed artifact; a nonzero exit can have fully retained output. Finalization cannot wait indefinitely for descendants holding a pipe open. Record any incomplete output state before declaring the terminal outcome ready.

The initial `exec_command` result has already completed its tool-call/result pair when it returns a live process handle. A later notification is contextual host input, not a second function result for that call and not a real user request. Preserve this distinction in `is_user_turn_boundary`, rollback, review provenance, work-note capture and model-switch accounting. Admission appends through the normal context path; it must not rewrite old tool results or instruction prefixes.

### Delivery, consumption and races

- Give each terminal outcome a stable notification identity. Delivery may retry, but logical history admission is idempotent. Persist enough state to distinguish a pending outcome, an admitted notification, and input owned by a sampling pass. Queue insertion alone is not model consumption.
- Preserve the notification identity with the durable admission record for its contextual history item, or reconcile a partially committed receipt against that durable admission during replay. Do not assume a receipt update and a separate transcript append commit atomically. Use the existing recorder and host-metadata owners; a new SQL database or WAL engine is not a prerequisite. Crash tests must exercise both write orderings so a persisted receipt cannot suppress a missing item and an existing item cannot be admitted twice.
- Reuse an already admitted notification during request retry or reconnect. Do not append a duplicate because the server's response was lost. This guarantees one logical input item per outcome, not exactly-once remote inference or exactly-once external command effects.
- If an explicit `write_stdin` call already delivers the terminal result to the owning conversation, reconcile that delivery with the pending notification. One path must claim logical delivery; the other may still emit its normal UI event but must not cause a duplicate continuation.
- Deferral during compaction, work-note preparation, completion review or stop must retain ownership and the pending outcome. Once those operations finish, re-evaluate eligibility before admission or sampling. Do not splice a notification into a request already in flight.
- Coalesce simultaneous completed jobs into a bounded batch for the same owner. Each outcome keeps its own receipt; quota or batching must not silently discard a result. Avoid one model invocation for every progress update.

## Milestone 3: explicit waiting owners and idle continuation

Automatic continuation requires a durable transition stating that the task is awaiting one or more registered job results. A background process handle or an assistant message saying “waiting” does not establish that state. Define this transition in the tool/lifecycle contract before enabling idle wake-up; its registration/acknowledgment tool pair must already be complete, and it must yield model inference without pretending that the original user task is complete. A normal completed answer closes its owner unless that separate waiting transition was explicitly established. This plan does not introduce an armed-watch owner that remains eligible across unrelated user turns.

Use the existing task admission and ownership boundary to reserve continuation, then revalidate cancellation, current owner generation, waiting jobs and pending user input before consuming a notification. At most one active inference owner may be admitted. A reservation that loses a race to pause, new input or shutdown is released without losing the pending outcome.

Entering the waiting state must check the registered wait condition and already recorded/admitted or poll-delivered outcomes under the same owner-transition synchronization used by completion admission. Recheck after registration and before parking. If the wait condition is already satisfied, continue the current eligible owner without parking or duplicating a consumed outcome. A completion arriving between the registration acknowledgment and the waiting checkpoint must remain actionable after replay; a notification emitted only before the waiter was installed is insufficient. Define whether a registration awaits any or all selected jobs, and use that predicate consistently rather than treating every terminal job as a new wake-up.

|Owner/session state when a terminal outcome arrives|Required behavior|
|---|---|
|Same owner has active ordinary work|Queue at the normal safe input boundary; do not interrupt an in-flight request or start a second owner.|
|Explicitly waiting owner, session alive and eligible|Reserve and revalidate continuation; deliver a bounded completion batch and start the next inference without a synthetic user prompt.|
|Paused or interrupted owner|Retain the outcome; never undo the stop automatically. `/continue` can reconcile it with the same owner after ordinary recovery.|
|New prompt accepted or another owner is current|Apply existing input precedence and supersession rules; do not treat an old completion as permission to resume old work. Retain it for explicit inspection.|
|Owner completed, abandoned, or rolled back away|Mark automatic delivery ineligible; never recreate the owner from a process event.|
|Review, compaction or work-note transaction owns execution|Defer and revalidate afterward; a shell result does not become a new review request or invalidate exact transaction boundaries by injection.|
|Session/process host exits|Persist the known state and honor existing process cleanup. No daemon or cross-exit process survival is promised.|
|Session resumes after a crash|Replay receipts and recorded outcomes. Unknown/lost process state remains unknown; never invent success, restart the command, or activate inference merely because the session was opened.|

Pause continues to cancel execution rather than freezing child processes. If cancellation terminates a registered process, its resulting terminal outcome remains stopped/canceled as observed; it does not become a successful completion trigger. Preserve the existing pause checkpoint and explicit-continuation contract rather than applying a new rule that abandons every awaited result on pause. A replayed waiting owner may reconcile durable finished output on explicit continuation, while a job that was running when its process host disappeared is not assumed to still exist. Leave incomplete-tail repair and pending work-note recovery to their current owners; do not add a blanket restart sweep that invents failed results for every dangling call.

Headless and interactive hosts need separate lifecycle integration. An opted-in headless host must remain alive while its explicitly waiting owner has registered work, and exit when that work finishes or is canceled within the task's deadline. Do not derive that behavior from a TUI queue implementation. Interactive status should distinguish waiting from paused and completed; this status is a projection of durable ownership, not the recovery record itself.

## Implementation references

The local ownership and persistence contracts remain authoritative. Inspect [`process_manager.rs`](../../../core/src/unified_exec/process_manager.rs) for clamping and collection, [`async_watcher.rs`](../../../core/src/unified_exec/async_watcher.rs) for current completion events, and [`tasks/mod.rs`](../../../core/src/tasks/mod.rs) for task stopping and process cleanup. Model history and durable metadata must follow [`history-replay.md`](../../features/history-replay.md), including exact compaction replacement and the separation of real user boundaries from contextual messages.

Useful external patterns are OpenAI Codex's split empty/nonempty wait policy and bounded collector (`codex-rs/core/src/unified_exec/process_manager.rs`), its reservation/revalidation separation (`codex-rs/core/src/session/turn_input.rs`), and Claude Code's job-to-notification chain and synchronous-completion deduplication (`src/tasks/LocalShellTask/LocalShellTask.tsx`). Borrow the responsibilities, not a newer API surface or an assumption that queue acceptance provides crash durability. Detailed source comparisons and consultation records belong with research evidence outside these persistent plans.

## Acceptance evidence and rollout

|Case|Required result|
|---|---|
|Silent job lasts longer than the old maximum|One longer empty wait remains pending until exit/deadline; no intermediate model request.|
|High-volume output during a long wait|Per-wait memory remains bounded, counts distinguish observed/retained output, and artifact status is accurate.|
|Nonempty stdin, initial exec, process exit and user stop|Existing interaction bounds and pairing remain; exit/stop does not wait out the full requested duration.|
|Completion races polling, registration, queue drain or owner stop|One logical outcome is admitted, and no stale owner starts inference.|
|Immediate completion before waiting checkpoint|The wait predicate is rechecked; no lost wake-up or permanently parked owner, including concurrent polling delivery.|
|Registration transitions into idle waiting|The acknowledged tool-call/result pair is durable before the waiting checkpoint; later completion remains contextual input.|
|Crash before/after outcome persistence, admission and consumption|Real recorder/replay preserves the correct pending/admitted state without duplicate history or invented process success.|
|Compaction, work notes, review, pause and new input|Existing transaction/input owners keep precedence; notifications are contextual and cannot create real user boundaries.|
|Multiple jobs and artifact failure/expiration|Bounded batched notification, per-job delivery state, truthful retrieval result, and no automatic command rerun.|
|Interactive/headless exit and resume|Waiting-owner lifecycle works on each host; ordinary completed sessions are not reopened by late events.|

Ship the longer-wait milestone after scoped deterministic collection, exit and cancellation checks. Keep completion delivery and idle continuation opt-in until their real persistence/replay and host-lifecycle cases are covered. Compare short waits, long waits and completion-driven continuation with the same tasks, output budgets and worker settings; report model invocations during waiting, correctness, total usage and end-to-end time. Move implemented contracts into feature docs and remove the completed portions of this plan.
