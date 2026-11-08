# State And Decisions

## Ownership

|Surface|Responsibility|
|---|---|
|[Completion state](../../../core/src/completion_review.rs)|Host-owned request frame, accepted/consumed input revisions, initial/latest answer slots, attempt allowance and tickets.|
|[Session transitions](../../../core/src/session/completion_review.rs)|Durable capture, prepared/started claims, private scheduling, advisory installation and main-request framing.|
|[Task lifecycle](../../../core/src/tasks/mod.rs)|Keep completion handles alive through finalization; serialize finalization with registration and stopping; retain acknowledged late input without treating it as consumed.|
|[Review task](../../../core/src/tasks/post_turn_completion_review.rs)|Fresh delegate configuration, packet rendering/budgets, strict result handling and advisory output contract.|
|[Replay](../../../core/src/session/rollout_reconstruction.rs)|Restore explicit state independently of prompt history and infer the appropriate interrupted task target.|
|[Recorder](../../../core/src/rollout/recorder.rs)|Suppress unchanged snapshots without skipping transcript flushes; preserve metadata during loading and continuation cleanup, including after dangling calls and interrupt markers.|

## Completion Boundaries

- Capture only the successful main task's returned nonempty `last_agent_message`, never the last assistant-role history item.
- Capture the actual completed task's CWD on every candidate, including replacements within an existing episode. A configuration-only directory change does not retarget the candidate.
- A main task has an explicit owner ID. A late callback cannot install its answer under a newer task's request frame.
- Accepted input immediately invalidates an outstanding ticket. Only input consumed by subsequent main work joins the request frame.
- Original input and consumed steering belong together; assistant progress between them does not create a completed prior round.
- A genuinely new user task preserves earlier known user context and terminal answers. Review continuations replace only the candidate slot.
- Incomplete work-notes capture remains continuation work and cannot produce a candidate.

## Durable Metadata

Completion-review tracking is disabled for `SessionSource::SubAgent(SubAgentSource::Review)`, shared by ordinary `/review` and post-turn review delegates. Their input follows the ordinary transcript path without review revisions, main-task ownership or candidate capture. Completion, pause and shutdown do not append `PostTurnCompletionReviewState` records to their rollouts. Other session sources retain existing tracking.

The parent owns recovery through its snapshots, ticket, allowance and prepared/started handoffs. Resuming a paused reviewer uses the parent's saved candidate to create a fresh delegate, not the child's rollout. A reviewer rollout remains a readable transcript, not an independent completion-review recovery checkpoint. This runtime policy adds no storage or configuration fields; existing inline metadata remains readable and is not removed.

`RolloutItem::PostTurnCompletionReviewState` carries host-only JSON recovery metadata whose typed implementation is internal to core. Its format checks do not appear in model-facing packets, prompts or review results. This avoids expanding the review result or app-server RPC payloads. The record contains:

- Generation and accepted/consumed request revisions.
- Frozen known user context, original pending input payloads, client IDs, exact model-facing message bodies and their installation phases.
- Initial historical reference, exact candidate, completion ID, actual completed-task CWD and automatic-attempt count.
- Current review ticket and optional prepared/started handoff with advisory text.
- Main-task ownership, independent recovery destination, advisory-installation state and replacement-answer requirement.

The optional `recovery_model` belongs to review-derived main work, not its advisory ticket. Claiming a handoff establishes it; explicit recovery-time selection retargets it even after steering clears the handoff. Steering preserves it, while successful completion, a new root request and fork initialization clear it. Metadata without that field uses the handoff/turn-settings fallback; a selection never persisted cannot be inferred.

It is not a `ResponseItem`, model summary, assistant output or UI cell. Unsupported or incomplete metadata is unavailable, not reconstructed from model-produced XML.

`RolloutItem::TokenUsageContext` separately records the complete usage baseline, its `last_usage_item_count` covered-history boundary, the estimated request-only frame tokens already included in it, and whether historical reasoning was included. Every positive completed usage report records this tuple, including when frame credit is zero and reasoning is excluded. A single record restores all fields even when its following UI token event was not written. Replay applies baselines in history order; matching repeated UI events cannot cover later unreported output. A legacy record without a boundary uses the last model item at that record's position, not at the end of the rollout. A different unannotated baseline has no known frame credit or server reasoning-inclusion guarantee. The reasoning flag describes the stored baseline, not a new task or an in-progress response: capture transport headers privately and commit their semantics only with positive completed usage. Missing/zero usage, failed streams and cancellation retain the preceding baseline and boundary. Compaction, local recomputation and full-window replacement reset frame credit; local/full-window baselines cover the entire installed history, including retained reasoning and notes. Continuation cleanup preserves the covered unchanged prefix. These host-only records do not alter completion-review checkpoints, public token events or model history. Missing reasoning flags deserialize conservatively as false. Legacy records remain readable without inferred frame credit; a near-limit legacy continuation may need compaction or a larger model once before a successful request establishes complete accounting.

Only a genuinely new root session or trusted metadata establishes exact request provenance. Resume, fork and history replacement without that evidence retain unknown scope, even when prompt history is empty or contains only encrypted compaction. Missing visible user boundaries do not prove missing requirements never existed. Subsequent genuine input remains tracked but cannot restore unknown prior scope by itself.

Snapshot writes and handoff transitions use a serialized persistence boundary. Finalization persists a completed candidate and any automatic-review reservation in one snapshot; later scheduling only enqueues that current ticket. Manual reservation remains a separate acknowledged transition. Persist a prepared handoff before enqueue and the started claim before main work. Failed writes disable review eligibility and surface a diagnostic. A full/closed operation queue preserves recoverable work instead of waiting while task finalization holds its lifecycle guard.

Recorder flush/shutdown acknowledgements carry the underlying I/O result, not just notification that the writer processed the command. A buffered write failure must reach the reservation or claim caller before task startup. Failure leaves the existing durable prefix intact and prevents the in-memory transition from authorizing new review work.

Each recorder writer suppresses a review snapshot equal to its latest written snapshot, including pending buffered writes. Pending state takes precedence over the flushed baseline, preserving transitions such as `A -> B -> A` before one flush. Only a successful flush advances the acknowledged baseline. Recorder clones share that writer; reopening or replacing a recorder starts without a cached baseline, even at the same path. Ordinary transcript records and flush/shutdown commands are never suppressed. Continuation cleanup preserves metadata, so its rewrite does not invalidate the latest retained snapshot.

Known initial main-task ownership shares the accepted-input snapshot. A resumed or otherwise newly assigned owner still requires an acknowledged checkpoint before sampling. These changes preserve the existing JSONL record shape and exact recovery payloads; they do not rewrite historical rollouts or improve how older readers recognize unknown record types. Flush acknowledgement means buffered writes completed, not an `fsync` power-loss guarantee.

Every resumed review-tracked main task must also persist its new owner before sampling, even when the handoff was already Started. Owner-write failure stops before history cleanup, snapshot injection or a model request; finalization retains interrupted continuation ownership. Missing mandatory context is an explicit error, distinct from an ordinary task that requires no review context. Continue preflight preserves the checkpoint on that error, including after the writer is repaired, so an invalidated in-memory snapshot cannot overwrite the last good recovery prefix.

Advisory preparation and installation finish their short durable transaction even if their caller is cancelled; task-stop joins that boundary before recording its stopped state. Pausing a reviewer clears an unconsumed prepared handoff and preserves its ticket for re-review. Pausing main work preserves started lineage and its output contract.

Continuation cleanup and acknowledgement of unconsumed input use the same boundary. A cleanup rewrite cannot discard metadata or acknowledged input written concurrently, and stopping a task cannot leave those writes racing the published pause state.

Input installation persists an `Installing` marker for a specific accepted revision before recording its user message. For immediate consumption, it flushes the body before consuming the revision into the request frame and persists the consumed state without an intermediate `Recorded` snapshot. Retained, unconsumed input still persists `Recorded` with its body. Replay recognizes a body after its installing marker using the exact host-owned message; it never deduplicates by text found anywhere in history. Accepted inputs without a body are reinstalled before ordinary sampling, after pending notes recovery. Acknowledgement before main-task startup also establishes recoverable continuation work. Pending notes retain the resume-selected destination model while compaction resolves its separate source.

Recognizing that installation preserves the prior, now-ineligible candidate and its CWD. The accepted revision already invalidates review eligibility, while an unfinished review-derived main task still needs that directory owner to resume. An unrecognized user boundary continues to discard the candidate and disable exact provenance. Do not erase known recovery ownership merely because the matching body arrived before installation or consumption was checkpointed.

Once input installation starts, its short owned transaction survives caller cancellation and task-stop joins it. A consumed amendment therefore cannot be persisted ahead of its model-facing message. Metadata without durable input-installation ownership cannot establish these guarantees and stays unavailable rather than being upgraded by inference.

Task-stop also joins the task lifecycle guard, including when finalization already removed the active-task entry. Only afterward may shutdown detach the recorder. Explicit pause/interrupt invalidate any private automatic-review ticket queued by the finalizer, so joining completion cannot accidentally start new review work after the stop. A started handoff instead belongs to interrupted main work; stop/shutdown retains its ticket, snapshots, selected destination and replacement contract rather than clearing that lineage as unstarted reviewer scheduling.

`ResponseItem::Message.id` is not serialized. Restart deduplication compares the exact host-owned developer item associated with the durable handoff, not an assumed persistent message ID. The main request only reinserts the advisory when compaction removed that item; it always re-establishes the replacement contract and exact request/candidate frame.

The active snapshot and review packet have bounded answer revisions. A clean automatic-review episode without steering or handoffs normally records five states: accepted input with owner, installing input, consumed input, completed candidate with automatic ticket, and finished review. Additional meaningful transitions remain recorded. The append-only rollout still grows with actual work; unchanged-snapshot suppression does not bound session storage or cumulative cost.

## Tickets And Recovery

A ticket identifies episode, request revision, history generation, exact candidate completion and attempt. Private operations associate the ticket with an opaque submission ID; public manual commands resolve the latest eligible completion. Dispatch, delegate entry, result acceptance, handoff preparation and main-task claim all validate ownership.

Manual reservation joins the task lifecycle before selecting work, including the interval after the active-task entry is removed but finalization is still installing a newer answer. Under the installation guard, one state lock reserves the ticket and clones its candidate snapshot; persistence completes before either is returned. The delegate checks both the supplied snapshot's ticket identity and the ticket's live ownership. A snapshot read before finalization must never be paired with a ticket reserved afterward.

Reservation failures retain their cause through the command handler: running task, missing candidate, pending input, unavailable provenance or failed persistence. Diagnose unavailable provenance before suggesting that pending input can restore eligibility. A new prompt can resolve missing work but cannot establish the requirements of an unknown prior scope. Storage failures remain storage errors rather than being collapsed into a generic missing-turn message. These internal errors add no wire or rollout fields.

|Transition|Behavior|
|---|---|
|Prepared, before or after enqueue|Resume requeues the ticket. A duplicate claim cannot start another main task. `/continue` can claim a prepared handoff if enqueue was unavailable.|
|Started, before completion|Resume offers interrupted main continuation with the same request snapshot and output contract; it does not repeat the advisory handoff.|
|Paused reviewer|Continue the same reviewer/candidate/ticket without another automatic-attempt reservation.|
|Paused review-derived main|Continue main work, preserving its replacement-answer requirement.|
|New user input|Invalidate old advice immediately, track pending versus consumed input, and establish a new episode only after main completion.|
|Compaction|Keep explicit host snapshots outside rewritten prompt history. Standalone compaction completion preserves the existing continuation checkpoint, including paused/interrupted main ownership. Reinsert mandatory context and the output contract for main follow-ups.|
|Explicit model change during recovery|Retarget pending regular work and its independent durable recovery destination, synchronizing a surviving Started handoff. Steering may invalidate that handoff without discarding model ownership. Preserve intent from the submitted `model` or collaboration-mode field, not a full mode synthesized for other overrides. Keep request/candidate snapshots, paused reviewer ownership and the notes capture's source model unchanged; CWD, approval and effort-only updates must not replace the saved model with a resumed session's default.|
|Rollback/history replacement|Invalidate uncertain provenance and pending operations; never retain a future candidate.|
|Fork|Recover only eligible metadata before the selected surviving user boundary; renew generation and clear delegate/main scheduling state. Explicit loss of exact provenance is a backward-scan barrier even within the same generation and without a new user message. Do not skip that invalidation to recover an older candidate. Metadata for an excluded next user must still not retarget the fork.|
|CWD change|Review the latest candidate's actual task directory. Enforce the same CWD when preparing, claiming or resuming review-derived main work, including pause, budget recovery and restart. Reject a mismatched resume before consuming its checkpoint or starting a task; returning to the original directory permits retry. Ordinary non-review continuation is not restricted by this review-specific rule.|

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

Main follow-ups use the same token-usage ledger as automatic compaction: the last positive usage baseline plus all items after its covered-history boundary, including unreported model output. Successful empty responses cover their input; missing-usage/failed output does not advance the boundary. Historical reasoning before the boundary is estimated only when the baseline excludes it; reasoning after it is already in the tail estimate. Charge only `next_request_frame_tokens - baseline_request_frame_tokens` for the replacement contract and exact review frame, including a negative delta when the replacement shrinks. The preceding frame is already included in reported usage even though it is not stored in prompt history. Record its cost only when usage is updated for the request that actually sent it; failed or cancelled requests cannot replace that accounting. Retain the 8,192-token reserve and omit the optional reference before rejecting mandatory overflow. Without a positive baseline, use the full prompt's byte estimate; missing/zero usage preserves any preceding usable baseline instead of erasing it. Reconstructed compaction baselines remain usable without frame credit. A whole-transcript byte estimate must not override usable usage accounting: accumulated reasoning and text estimates can drift far above model-reported tokens and stop a healthy long-running continuation before automatic compaction is due. Overflow diagnostics report estimated tokens and the available budget.

Unexpected context pressure must not turn into a clean verdict: compaction during the delegate ends full-review coverage; malformed output, missing output, execution failure and unrepresentable nontext context stay unavailable. Main follow-ups also refuse to send truncated request/candidate snapshots.

A budget-stopped main follow-up remains resumable without reclaiming its already-started handoff. `/compact` reduces ordinary history without consuming that continuation; an explicit main-model selection updates the pending destination rather than letting `/continue` restore the insufficient model. Honor that selection even when session settings already name it but a restored checkpoint still names the old model. Persist the destination independently of advisory validity so steering, pause and restart cannot undo it. Replay prefers that destination for pending input and interrupted main work without changing the notes capture's source model. If mandatory snapshots alone do not fit, compaction still cannot authorize truncation.

`request_user_input` clarification answers are outside the currently captured request frame. Until structured question/answer ownership is implemented, receiving an answer marks coverage unavailable before delivering it to the waiting main task. That persisted invalidation also blocks recovery of a pre-clarification candidate when forking the completed follow-up's full history. Do not reconstruct human scope from arbitrary function-output JSON.

The three-attempt automatic default is a finite execution policy, separate from the two-answer context bound. It is not an empirically optimized count. A manual handoff after exhaustion does not enable another automatic attempt.
