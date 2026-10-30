# State And Decisions

## Ownership

|Surface|Responsibility|
|---|---|
|[Completion state](../../../core/src/completion_review.rs)|Versioned request frame, accepted/consumed input revisions, initial/latest answer slots, attempt allowance and tickets.|
|[Session transitions](../../../core/src/session/completion_review.rs)|Durable capture, prepared/started claims, private scheduling, advisory installation and main-request framing.|
|[Task lifecycle](../../../core/src/tasks/mod.rs)|Keep completion handles alive through finalization; serialize finalization with new task registration; retain acknowledged late input without treating it as consumed.|
|[Review task](../../../core/src/tasks/post_turn_completion_review.rs)|Fresh delegate configuration, packet rendering/budgets, strict result handling and advisory output contract.|
|[Replay](../../../core/src/session/rollout_reconstruction.rs)|Restore explicit state independently of prompt history and infer the appropriate interrupted task target.|
|[Recorder](../../../core/src/rollout/recorder.rs)|Preserve metadata during loading and continuation cleanup, including after dangling calls and interrupt markers.|

## Completion Boundaries

- Capture only the successful main task's returned nonempty `last_agent_message`, never the last assistant-role history item.
- A main task has an explicit owner ID. A late callback cannot install its answer under a newer task's request frame.
- Accepted input immediately invalidates an outstanding ticket. Only input consumed by subsequent main work joins the request frame.
- Original input and consumed steering belong together; assistant progress between them does not create a completed prior round.
- A genuinely new user task preserves earlier known user context and terminal answers. Review continuations replace only the candidate slot.
- Incomplete work-notes capture remains continuation work and cannot produce a candidate.

## Durable Metadata

`RolloutItem::PostTurnCompletionReviewState` carries a versioned JSON payload whose typed implementation is internal to core. This avoids expanding the review result or app-server RPC payloads. The record contains:

- Generation and accepted/consumed request revisions.
- Frozen known user context, supported input references and explicitly pending input.
- Initial historical reference, exact candidate, completion ID and automatic-attempt count.
- Current review ticket and optional prepared/started handoff with advisory text.
- Main-task ownership, advisory-installation state and replacement-answer requirement.

It is not a `ResponseItem`, model summary, assistant output or UI cell. Unknown/incomplete metadata versions are unavailable, not reconstructed from model-produced XML.

Snapshot writes and handoff transitions use a serialized persistence boundary. Persist completion before reserving its review; persist a prepared handoff before enqueue; persist the started claim before main work. Failed writes disable review eligibility and surface a diagnostic. A full/closed operation queue preserves recoverable work instead of waiting while task finalization holds its lifecycle guard.

Advisory preparation and installation finish their short durable transaction even if their caller is cancelled; task-stop joins that boundary before recording its stopped state. Pausing a reviewer clears an unconsumed prepared handoff and preserves its ticket for re-review. Pausing main work preserves started lineage and its output contract.

Continuation cleanup and acknowledgement of unconsumed input use the same boundary. A cleanup rewrite cannot discard metadata or acknowledged input written concurrently, and stopping a task cannot leave those writes racing the published pause state.

`ResponseItem::Message.id` is not serialized. Restart deduplication compares the exact host-owned developer item associated with the durable handoff, not an assumed persistent message ID. The main request only reinserts the advisory when compaction removed that item; it always re-establishes the replacement contract and exact request/candidate frame.

The active snapshot and review packet have bounded answer revisions. The append-only rollout still grows with actual work and includes repeated bounded snapshots; there is no claim of bounded session storage or constant cumulative cost.

## Tickets And Recovery

A ticket identifies episode, request revision, history generation, exact candidate completion and attempt. Private operations associate the ticket with an opaque submission ID; public manual commands resolve the latest eligible completion. Dispatch, delegate entry, result acceptance, handoff preparation and main-task claim all validate ownership.

|Transition|Behavior|
|---|---|
|Prepared, before or after enqueue|Resume requeues the ticket. A duplicate claim cannot start another main task. `/continue` can claim a prepared handoff if enqueue was unavailable.|
|Started, before completion|Resume offers interrupted main continuation with the same request snapshot and output contract; it does not repeat the advisory handoff.|
|Paused reviewer|Continue the same reviewer/candidate/ticket without another automatic-attempt reservation.|
|Paused review-derived main|Continue main work, preserving its replacement-answer requirement.|
|New user input|Invalidate old advice immediately, track pending versus consumed input, and establish a new episode only after main completion.|
|Compaction|Keep explicit host snapshots outside rewritten prompt history. Reinsert mandatory context and the output contract for main follow-ups.|
|Rollback/history replacement|Invalidate uncertain provenance and pending operations; never retain a future candidate.|
|Fork|Recover only eligible metadata before the selected surviving user boundary; renew generation and clear delegate/main scheduling state. Metadata for an excluded next user must not retarget the fork.|
|CWD change|Review the completed task's directory. Do not prepare or claim a main handoff into a different current project.|

These are scheduling/ownership guarantees, not exactly-once tool or model side effects across process failures.

## Context And Answer Decisions

|Alternative|Decision|
|---|---|
|Full parent transcript|Rejected: includes reasoning/tools/verdicts and defeats independent review.|
|Every compact review exchange|Rejected: answer revisions and prior findings accumulate.|
|Last `K` revisions|Rejected: the original comprehensive answer eventually disappears.|
|Latest answer only|Smaller but loses the reference when the model again writes a delta.|
|Initial reference plus latest delta, without a replacement contract|Insufficient: omitted intermediate corrections cannot be recovered.|
|Generated rolling summary or separate final writer|Deferred: adds latency, drift and a second text that may differ from the user's answer.|
|Frozen frame plus initial reference and exact latest replacement|Implemented: bounded revisions, independent reviewer and no extra synthesis call.|

The reference helps detect dropped content but is not authoritative. Cumulative corrections must survive in the latest replacement answer itself. No length threshold, prompt or boolean can prove arbitrary semantic completeness.

## Budget And Failure Policy

Use the resolved review model, not the parent's capacity. Deduplicate initial/latest text, omit the optional reference if necessary, and refuse a full review when mandatory material does not fit. Approximate token counts and explicit reserves are conservative planning tools, not exact tokenizer guarantees.

Unexpected context pressure must not turn into a clean verdict: compaction during the delegate ends full-review coverage; malformed output, missing output, execution failure and unrepresentable nontext context stay unavailable. Main follow-ups also refuse to send truncated request/candidate snapshots.

The three-attempt automatic default is a finite execution policy, separate from the two-answer context bound. It is not an empirically optimized count. A manual handoff after exhaustion does not enable another automatic attempt.
