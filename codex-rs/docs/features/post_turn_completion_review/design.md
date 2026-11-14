# Completion-review state and decisions

## Code entry points

|Responsibility|Source|
|---|---|
|Frozen scope, answer slots, revisions and tickets|[Completion state](../../../core/src/completion_review.rs)|
|Durable transitions, claims and request framing|[Session review](../../../core/src/session/completion_review.rs)|
|Finalization and task-stop ordering|[Task lifecycle](../../../core/src/tasks/mod.rs)|
|Delegate setup, context packets and verdicts|[Review task](../../../core/src/tasks/post_turn_completion_review.rs)|
|Recovery from durable records|[Rollout reconstruction](../../../core/src/session/rollout_reconstruction.rs)|
|Flush errors, snapshot suppression and cleanup|[Recorder](../../../core/src/rollout/recorder.rs)|

## Capture and ownership

Capture only the successful main task's returned nonempty `last_agent_message`, never an arbitrary last assistant-role history item. Record that task's actual CWD on every candidate, including replacements. Explicit main-task ownership prevents a late callback from attaching its answer to a newer request. Unfinished work-notes capture cannot produce a candidate.

Accepted input immediately invalidates old review tickets. Only consumed input joins the frozen request. Original input and consumed steering belong to one request; intervening assistant progress does not create a completed prior round. A new user task preserves known earlier user rounds and terminal answers; a review continuation replaces the current candidate while retaining the initial reference.

A ticket binds episode, request revision, history generation, candidate completion and attempt. Validate it at dispatch, delegate entry, result acceptance, handoff preparation and main-task claim. Manual reservation joins finalization, then reserves the ticket and clones its candidate under the same state lock; persist the pair before returning it. Never combine an older snapshot with a ticket reserved after a newer completion.

Review delegates use `SubAgentSource::Review` and leave completion-review recovery to the parent. They record ordinary transcripts but no accepted-input revisions, candidates or completion-review snapshots. Resuming a paused reviewer creates a fresh delegate from the parent's saved candidate. Other session sources retain tracking; automatic review scheduling is suppressed for subagent sessions.

## Durable metadata

`PostTurnCompletionReviewState` is host-owned rollout metadata, separate from `ResponseItem` history, model packets and UI output. It stores exact scope and pending input, candidate/reference slots, tickets, handoffs, task ownership, the replacement-answer requirement and the recovery model. Storage-format checks remain internal; model-facing packet scaffolding and result fields are unversioned. Unsupported or incomplete records cannot establish exact provenance.

`TokenUsageContext` separately persists usage coverage, request-only frame credit and reasoning semantics. Keep it out of model history and preserve it during cleanup; see [budget accounting](budget_accounting.md).

### Write ordering

1. Persist accepted original input with its known initial task owner. Keep its original payload until installation is durable.
2. Persist a revision-specific `Installing` marker before the model-facing body. Flush the body before consumption. Immediately consumed input needs no intermediate `Recorded` snapshot; retained unconsumed input does.
3. Persist a completed candidate and any automatic-review reservation together. Scheduling uses that existing ticket; it cannot recreate a ticket invalidated by stop.
4. Persist a `Prepared` handoff before enqueueing and its `Started` claim before main work. A full or closed operation queue leaves recoverable work rather than blocking the finalizer.
5. Persist every resumed main task's new owner before cleanup, context injection or sampling, including an already-Started handoff.

Replay matches a body to its revision-specific installing marker using the exact host-owned message. It repairs both acknowledged-but-unwritten input and body-written/state-not-written gaps. It never deduplicates distinct identical amendments by searching for matching text elsewhere. Missing bodies are reinstalled after pending notes recovery and before ordinary sampling.

Recognizing an amendment body preserves the prior, now-ineligible candidate as the directory owner until amended work completes. Do not discard recovery ownership merely because the body arrived before its installation/consumption checkpoint. An unrecognized user boundary instead invalidates exact provenance.

### Cancellation and I/O

Short input/advisory installation transactions survive caller cancellation; task-stop joins their guard before publishing stopped state. Keep task handles alive through terminal persistence. Registration and externally requested pause/interrupt/shutdown must join finalization even after the active-task entry disappears; only then may shutdown detach the recorder.

Flush and shutdown acknowledgements carry the writer's I/O result. A failed reservation, claim or resumed-owner checkpoint prevents model startup and preserves the last durable prefix. Missing mandatory snapshots are an error, not an ordinary no-review task. Continue preflight must retain the pending checkpoint even after writer repair, so invalid in-memory state cannot overwrite good recovery data. Flush completion is not an `fsync` or power-loss atomicity guarantee.

Suppress unchanged review snapshots only inside the recorder writer. Compare against pending state before the flushed baseline, preserving `A -> B -> A` within one flush. Advance the acknowledged baseline only after a successful flush and reset it when replacing the writer. Never suppress transcript writes, flush/error delivery or newly assigned ownership because review metadata is unchanged.

Continuation cleanup shares the installation boundary and preserves host metadata and acknowledged input. `ResponseItem::Message.id` is not serialized; advisory recovery compares the exact durable developer item, not an assumed persistent message ID. Reinsert advice only when compaction removed it, while always restoring the replacement contract and exact request/candidate frame.

## Recovery transitions

|Transition|Contract|
|---|---|
|Prepared handoff|Resume requeues it; `/continue` can claim it if enqueue failed. A duplicate claim cannot start another main task.|
|Started handoff|Resume interrupted main work with its exact snapshots and replacement contract; do not repeat the advisory action.|
|Paused reviewer|Keep the ticket/candidate/allowance, clear unconsumed prepared advice, and start a fresh reviewer on continue.|
|Explicit pause, interrupt or shutdown|Invalidate queued unstarted automatic work after joining finalization. Retain already-started main lineage and recovery selection.|
|New steering|Invalidate old advice immediately; keep pending versus consumed ownership and any unfinished main recovery destination.|
|Compaction|Preserve explicit review snapshots outside rewritten history. Standalone compaction does not complete or consume paused/interrupted main ownership.|
|Explicit model selection|Persist the pending regular-work destination independently of the advisory handoff; synchronize a surviving Started handoff. Honor submitted selection even if session settings already name that model.|
|CWD, approval or effort-only update|Do not retarget the saved model from a synthesized full mode or a resumed session's default.|
|CWD mismatch|Reject review-derived preparation, claim or resume before taking its checkpoint or starting a task. Ordinary continuation is not subject to this review-specific restriction.|
|Successful completion or new root request|Clear the old recovery model. Capture each replacement's actual completed-task directory.|
|Rollback or uncertain history replacement|Invalidate uncertain provenance and pending operations; never retain a future candidate.|
|Fork|Use eligible metadata inside the surviving boundary, renew the generation and clear scheduling/main ownership. Exact-provenance invalidation is a backward-scan barrier; do not recover an older candidate across it.|

Explicit recovery selection does not change exact request/candidate snapshots, a paused reviewer's ownership or a pending compaction capture's source model. Metadata without an independent recovery model retains its existing handoff/turn-settings fallback; a never-persisted selection cannot be inferred.

## Provenance and failure policy

Only a new root session or trusted host metadata establishes exact request scope. Empty, summary-only or encrypted-only history does not prove that no requirements were lost. Later referential input cannot repair unknown scope. A fork must distinguish excluded-next-user metadata from an explicit provenance invalidation inside the surviving generation.

Human clarification currently invalidates complete review coverage before its answer reaches the main task. Preserve that invalidation through restart and full-history forks; arbitrary tool-output JSON is not a trusted question/answer frame. Nontext input likewise cannot become a claimed complete text-only projection.

Keep reservation causes distinct: running task, missing candidate, pending input, unavailable provenance and persistence failure have different recovery paths. Unsupported records can disappear from a fork's recognized-record copy; offline repair must inspect original ancestors, back up the rollout, reconstruct exact scope only from verified request/completion/consumption checkpoints, and validate through production replay before appending a corrected checkpoint. No automatic legacy migration exists. Do not flip an exactness flag, treat an assistant message as a completion, or promote a summary to user requirements.

## Design choices

|Choice|Reason|
|---|---|
|Frozen request plus initial reference and exact latest candidate|Bounds answer revisions while retaining a reference that can expose dropped content.|
|Complete replacement answer on every continuation|Intermediate corrections must survive in the latest answer itself; a reference cannot complete a delta-only candidate.|
|Exclude the parent transcript and previous verdicts|Preserves independent review and avoids growing reasoning/tool/review context.|
|No extra final synthesis call|Keeps the reviewed candidate identical to the user's streamed answer.|
|Optional reference omitted before mandatory overflow|Preserves exact scope/candidate; incomplete coverage cannot become a clean verdict.|
|Finite automatic allowance|Bounds execution independently of the two-answer context bound; manual review does not reset it.|

These contracts do not guarantee semantic completeness, filesystem snapshot isolation or exactly-once model/tool side effects across process failure.
