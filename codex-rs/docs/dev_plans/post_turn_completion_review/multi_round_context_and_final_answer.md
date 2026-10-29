# Bounded Review Context and Complete Final Answers

## Status and decision

Proposed follow-up design, based on static inspection of the customized `codex-rs_v0.98` archive on 2026-10-29. This document does not describe an implemented change. The existing workflow, configuration, and delegate isolation remain the starting point.

**Recommendation:** freeze the genuine user interaction context for a review episode, retain its initial assistant final as a historical reference, and replace a separate latest-candidate slot after each main continuation. Supply at most those two episode answer texts to each fresh reviewer. Require every review-triggered continuation to write a complete replacement answer to the active user request, and make the reviewer assess that exact latest answer independently of the historical reference.

Do not send previous reviewer evaluations to subsequent reviewers. Do not inherit the parent transcript through `InitialHistory`. Do not introduce a separate model-generated review summary or final-answer writer in this change.

The two-answer context and the replacement-answer contract are a joint design. The reference prevents the immediately observed loss of the earlier answer; the replacement contract carries still-relevant results through later iterations. Neither mechanism alone is sufficient.

This decision supersedes the user-boundary-only reconstruction and indefinitely growing session-history prescription in [design_proposal.md](design_proposal.md). It preserves the independent inspection and host-instruction work in [review_effectiveness_hardening.md](review_effectiveness_hardening.md).

## 1. Terms and the failure being addressed

|Term|Meaning|
|---|---|
|Real user input|An actual user request, clarification, correction, or steering message. Review advice is not user input.|
|Completed main task|A successful regular main-session task with a nonempty terminal assistant answer. A review-triggered `ContinueTask` is also regular.|
|Review episode|The initial completed main task and its subsequent review/follow-up iterations while the consumed user-request basis remains unchanged.|
|Review attempt|One delegate invocation against one identified candidate. A transport retry or pause/resume does not create a new user request.|
|Initial answer reference|The episode's first terminal main answer, retained unchanged for context. It is not necessarily complete, correct, or accepted.|
|Candidate final answer|The latest terminal main answer. This is the exact answer delivered to the user and evaluated for present accuracy and self-containment.|
|Request frame|The genuine user interaction context needed to interpret the active request, frozen for an episode. Earlier completed user rounds may supply referents; they are not all newly outstanding tasks.|

For the reported sequence, use `A0` for the first main answer, `R1` for the first reviewer result, and `A1` for the first main follow-up:

```text
U1 -> A0 -> R1 -> A1 -> R2 -> A2
```

`R1` and `R2` enter the parent as developer messages. There is no `U2`. If `A0` is comprehensive and `A1` only says what changed after `R1`, reviewing `U1 + A1` loses useful context. Delivering `A1` as the latest final also leaves the user without a complete current answer.

The required result is not a transcript of the review debate. It is an independent assessment of current work against the user's request and a latest main answer that communicates that work by itself.

## 2. Root causes in the inspected source

### 2.1 User-turn grouping collapses review continuations

[CompletedTurnReviewRound](../../../core/src/state/session.rs) contains `user_messages` and one `final_agent_message`. [completed_turn_interaction_history_from_history](../../../core/src/session/turn.rs) starts a new round at a recognized real user boundary and overwrites `current_final_agent_message` whenever it sees another nonempty assistant message within that round. Ordinary developer messages do not finalize the round.

Consequently, `U1, A0, developer(R1), A1` becomes one reconstructed pair, `U1, A1`. The original `A0` can still exist in the parent transcript; it has been discarded from the review projection.

The existing multi-round support works for `U1, A0, U2, A1`. It does not preserve separate completions inside `U1, A0, R1, A1`. Earlier documentation used the word "round" for both and overstated what had been fixed.

### 2.2 The continuation deliberately adds no user boundary

[record_advisory_and_request_continuation](../../../core/src/tasks/post_turn_completion_review.rs) records a `DeveloperInstructions` item and requests `TurnContinuationSource::PostTurnCompletionReview`. [continue_after_post_turn_completion_review](../../../core/src/tasks/mod.rs) submits `Op::Continue`; [continue_last](../../../core/src/session/handlers.rs) starts [ContinueTask](../../../core/src/tasks/continue_task.rs) with empty user input. Its task kind is `Regular`.

That behavior correctly preserves the user's request. Inserting a synthetic user message to make the reconstruction algorithm retain another round would change turn semantics and is not the repair.

### 2.3 Capture replaces the earlier snapshot with the lossy projection

[capture_completed_regular_turn_for_review](../../../core/src/session/mod.rs) reconstructs `interaction_history` from `state.history.raw_items()`. For an empty-input continuation, it takes user text from the latest reconstructed round and replaces `last_completed_regular_turn_for_review`. It does not preserve the episode's initial final independently of that reconstruction.

The fallback `assistant_message_text` checks the assistant role, not an authoritative task-completion boundary. It can select the last surviving assistant text after history changes. New live capture should use the actual completed task's final result, not infer finality from the role alone.

### 2.4 The delegate runner preserves the intended isolation

[render_completed_turn_context](../../../core/src/tasks/post_turn_completion_review.rs) renders every available `interaction_history` entry. The task passes the resulting text to [run_codex_thread_one_shot](../../../core/src/codex_delegate.rs) with `Some(InitialHistory::New)`. The runner creates a fresh session, submits that input, and stops after completion, abort, or pause.

The context was already incomplete before the runner received it. Keep `InitialHistory::New`, the read-only sandbox, disabled web search/collaboration, review model/provider selection, and review-specific instruction profile.

### 2.5 Neither handoff nor review explicitly requires a replacement final

[render_advisory_developer_message](../../../core/src/tasks/post_turn_completion_review.rs) tells the main model to verify and act on applicable findings. It does not require a complete replacement answer. The [review prompt](../../../core/post_turn_completion_review_prompt.md) checks requested scope and unsupported claims, but does not explicitly distinguish "work is complete" from "the latest answer is complete on its own."

The main model can therefore answer the latest advisory with a short delta. The next reviewer receives that delta through the lossy projection.

### 2.6 TUI exposes the consequence rather than creating it

[on_task_finished](../../../core/src/tasks/mod.rs) publishes the main task's `TurnComplete` before scheduling automatic review; the answer also follows the normal streaming path. [on_agent_message](../../../tui/src/chatwidget/streaming.rs) renders assistant text, and [on_task_complete](../../../tui/src/chatwidget/turn_runtime.rs) stores the latest nonempty final in `last_assistant_output_markdown` and `last_copyable_output`.

Thus a delta-only follow-up becomes the latest main answer in the waterfall and the latest copyable answer. The TUI has no semantic merge operation for earlier answers. A clean review returns no replacement main answer: `PostTurnCompletionReviewTask::run` returns `None`.

### 2.7 Existing tests cover the adjacent case

[review_completed_turn_passes_multi_round_interaction_history](../../../core/tests/suite/review.rs) and [completed_turn_reconstruction_keeps_multi_round_interaction_history](../../../core/src/session/tests.rs) both introduce a second real user message. The single-positive-review integration test stops after one continuation and uses `follow-up done` as its fixture answer. None checks the second reviewer after a developer-advice-only continuation.

These are gaps in coverage, not evidence that tests were failing. No Rust tests were executed for this documentation change.

## 3. Options considered

|Option|Reviewer context|Advantages|Limitations|Decision|
|---|---|---|---|---|
|Full parent history|All user, assistant, review, and tool items|Keeps every interaction|Breaks the fresh-context purpose; large and increasingly dependent on prior reasoning and verdicts|Reject|
|All compact review exchanges|Every final and every review result, without tools|Preserves review causality|Still grows with review count and encourages reviewers to follow earlier verdicts|Reject|
|Sliding window of the last `K` exchanges|Recent finals and optionally advice|Simple count bound|The original comprehensive answer can age out; a fixed window does not preserve cumulative corrections|Reject as the default|
|Initial answer plus latest delta only|`U + A0 + An`, with no new output contract|Immediately restores the missing initial answer|Intermediate corrections can disappear; the user's latest answer can remain incomplete|Insufficient alone|
|Latest full answer only|User scope plus `An`, with a replacement contract|Smallest independent payload|When the model produces the observed delta again, the reviewer immediately loses the earlier answer|Possible later simplification|
|Generated rolling summary or separate final writer|User scope plus synthesized current state|Can consolidate a long sequence|Extra generation, latency, summary drift, and a second text that may differ from the user's answer; still needs review|Defer|
|Frozen request frame, initial reference, latest replacement candidate|`F + A0 + An`, deduplicating identical answer text|Directly addresses the observed loss, retains fresh review, adds no summarizer, bounds episode answer slots to two|Historical reference can be stale; carrying intermediate corrections still depends on the replacement contract|Recommend|

The recommended reference is deliberately called `initial_answer_reference`, not "accepted baseline" or "last complete answer." The harness cannot infer completeness from a long response or a success flag.

The parent may retain review advice in its normal conversation history. The restriction applies to the next review delegate's context, not to erasing the parent's record of work.

## 4. Recommended input and state model

### 4.1 Freeze the request frame; replace the candidate

At the first completed main task for an episode:

1. Capture the genuine user messages actually consumed by that task, including steering consumed during its run.
2. Preserve prior genuine user rounds needed by the existing multi-round contract. Their terminal main answers provide context for references such as "use option two." Keep one terminal answer per earlier user round, not its entire review sequence.
3. Capture the terminal main answer once as both initial reference and latest candidate.
4. Assign a stable episode identity and request revision. These are host metadata, not user text.

For each review-triggered continuation of the same episode, replace only the latest candidate. Do not append a new user round or replace the initial reference. Any older candidate other than the initial reference drops out of the active review snapshot.

Exclude every input owned by the current main task from `prior_user_rounds`; original input and consumed steering belong together in `current_user_messages`. Populate prior rounds only from known completed-task ownership. For `U1 -> assistant progress -> U2 steering -> A0`, do not invent a completed `U1/progress` round or duplicate `U1`. If legacy history cannot establish ownership, retain qualified reconstruction provenance.

Start with the existing policy of preserving genuine prior user context, rather than an untested heuristic that selects "relevant" old constraints. Interpret later user amendments and explicit cancellations chronologically. An earlier assistant promise is context to check, not a new source of user authority.

New real user input invalidates the old frozen frame. Do not review an earlier answer as if it had already fulfilled an acknowledged but unconsumed amendment. The input must join subsequent main work before a new review episode is eligible.

### 4.2 Illustrative internal types

These names are proposed internal types, not a patch or a new public API:

```rust
struct FinalAnswerSnapshot {
    completion_id: String,
    text: String,
}

struct ReviewRequestFrame {
    prior_user_rounds: Vec<CompletedTurnReviewRound>,
    current_user_messages: Vec<String>,
    consumed_input_revision: u64,
    provenance: RequestFrameProvenance,
}

struct CompletionReviewEpisode {
    episode_id: String,
    request_revision: u64,
    history_generation: u64,
    cwd: PathBuf,
    request_frame: ReviewRequestFrame,
    initial_answer_reference: Option<FinalAnswerSnapshot>,
    candidate_final_answer: FinalAnswerSnapshot,
    automatic_reviews_started: u32,
}

struct ReviewTicket {
    episode_id: String,
    request_revision: u64,
    history_generation: u64,
    candidate_completion_id: String,
    attempt_id: String,
}
```

`RequestFrameProvenance` distinguishes an exact host-captured frame from an incomplete legacy reconstruction. `initial_answer_reference = None` is meaningful when an old session cannot establish the original completion. `history_generation` invalidates work after rollback or other transcript replacement. Use the existing task/continuation association to identify a follow-up; do not detect episodes by answer equality or by parsing `<post_turn_completion_review>` in conversation text.

The consumed-input revision needs host ownership at the point inputs enter a main model request. Spawn-time `RunningTask.completed_turn_user_messages` alone is insufficient: the current turn loop can consume steering later, and `on_task_finished` can record additional pending input after the final sampling request.

### 4.3 Delegate payload

Use an explicit envelope instead of describing the latest assistant message as the "highest-indexed round." A schematic payload is:

```xml
<completed_turn_review_context version="2">
  <episode_id>...</episode_id>
  <request_revision>...</request_revision>
  <candidate_completion_id>...</candidate_completion_id>
  <cwd><![CDATA[...]]></cwd>
  <request_context provenance="exact">
    <prior_user_rounds>...genuine user rounds and their terminal answers...</prior_user_rounds>
    <current_user_messages>...actual consumed user input...</current_user_messages>
  </request_context>
  <initial_answer_reference status="historical_unverified">
    <![CDATA[...first final in this episode...]]>
  </initial_answer_reference>
  <candidate_final_answer>
    <![CDATA[...exact latest main final...]]>
  </candidate_final_answer>
  <context_coverage request="complete" candidate="complete" reference="included" />
</completed_turn_review_context>
```

For the first review, or when both answer texts are identical, serialize the answer once under `candidate_final_answer` and use an explicit reference marker for the initial answer. Deduplication changes bytes sent, not completion identity. Escape all text according to the chosen serializer, including the existing CDATA terminator case. User or assistant text that resembles an XML tag must remain data.

Do not add standalone copies of earlier reviewer evaluations, finding lists, accepted/rejected-finding ledgers, hidden reasoning, tool calls, tool results, or intermediate assistant messages. Preserve exact user and candidate text even if it quotes review material; do not scrub it to manufacture a different candidate. Such quotations do not independently establish a finding or expand user scope unless the user actually adopts that requirement. `prior_user_rounds` may contain earlier final answers needed to interpret genuine user history; the two-answer bound applies to the current review episode, not the whole lifetime of the session.

The reviewer may inspect current repository files and deliverable artifacts read-only. Earlier answers are claims and context, not proof of the current filesystem state.

### 4.4 Exact behavior for the reported sequence

|Point|Input to the next reviewer|Latest main answer requirement|
|---|---|---|
|After `A0`|`F + A0`, answer text once|Answer the active request|
|After `R1` and `A1`|`F + reference(A0) + candidate(A1)`|`A1` fully replaces `A0` for the user|
|After `R2` and `A2`|`F + reference(A0) + candidate(A2)`|`A2` incorporates still-relevant corrections from `A1`|
|After any later continuation|Same shape; replace only candidate|No accumulation of answer revisions or reviewer verdicts|
|After new real user input|Invalidate the old ticket; form a new frame after main work consumes that input|Answer the amended active request|

If `A1` remains delta-only, the second reviewer now has the missing initial reference, but must still report that `A1` is an incomplete user deliverable. It must not silently combine `A0 + A1` and declare the latest answer complete.

For example, if `A1` corrects X and `A2` corrects Y while omitting X, `A0 + A2` cannot reconstruct the correction to X. The design does not claim otherwise. `A2` must carry the current result for X itself. When that fails, the reviewer should distinguish an incomplete explanation from missing implementation, based on current evidence.

## 5. Bounds and overflow

There are three independent quantities: retained review revisions, payload size, and number of automatic review attempts. A limit on one does not limit the others.

For review attempt `r`, let `F` be the fixed request frame, `A0` the initial reference, `Ar` the latest candidate, and `H` the framing overhead. The assembled episode input is:

```text
tokens(input_r) = tokens(F) + tokens(Ar) + optional_tokens(A0) + tokens(H)
```

There is no sum of earlier candidates or reviewer evaluations. Answer-slot count is constant in `r`; individual answer length and genuine user history can still grow. Repeatedly sending a fixed-size frame also has cumulative cost across attempts; this proposal does not claim constant total token cost.

### Payload policy

Preflight the entire request using the resolved review model's effective context limit and the project's token-estimation facilities. Reserve capacity for base/host/project instructions, tools, reviewer output, and useful read-only inspection before choosing the packet budget. A provider-specific budget must not be copied blindly from the main model. Missing model limits require a conservative declared budget or an unavailable-review result, not an unbounded request.

Apply this order:

1. Deduplicate identical initial and latest answer text.
2. If necessary, omit the optional initial reference as a whole and mark `reference="omitted_for_budget"`.
3. Preserve applicable user context and the latest candidate exactly. Do not clip the middle of user constraints, shorten a candidate that differs from the user-visible text, or silently discard older user requirements.
4. If mandatory material still does not fit, do not spawn a purported full completion review. Retain the latest main answer and report that review could not cover the request within the context budget.

The first implementation should not add an LLM summarization call to hide overflow. A future request-summary design would need provenance, supersession rules, and its own completeness assessment. Tool-readable files do not make context cost disappear; reading them also spends the review budget.

Images, other nontext input, or referenced artifacts that the text frame cannot represent must be identified explicitly. Preserve supported references and the reviewer's ability to inspect them; if necessary context is unavailable, report the limitation. Do not label a text-only projection as complete multimodal coverage.

### Automatic-attempt policy

The inspected implementation has no review-count cap in `maybe_auto_post_turn_completion_review`. Keep that fact distinct from this proposal's bounded payload.

As a separate, small hardening step, recommend an explicit finite automatic-attempt allowance. A default of three reviewer attempts per episode is a proposed product choice, not a measured optimum. It permits the initial review and up to two reviewed follow-ups. A positive verdict on the last allowed attempt stops automatic continuation and leaves an explicit "review stopped with unresolved advice" status and the latest findings visible. It does not mean the work passed review. The user can explicitly request another manual review.

Pause/resume or transport retries against the same ticket should not silently create new budget allowances. A manually requested review may perform its single advisory handoff even when the automatic allowance is exhausted, but it does not reset that allowance. The resulting main follow-up receives the complete-answer contract; another automatic review remains suppressed when no automatic allowance remains. A new genuine user-request episode establishes a new allowance. A manual review with automatic review disabled likewise retains its requested advisory handoff without enabling automatic re-review. If the cap is deferred, document that only context growth is bounded, not loop duration.

## 6. Final-answer contract

### 6.1 Main continuation instruction

Retain the advisory language and add a task-owned output contract to every review-triggered continuation, including manual review handoffs:

```text
The independent review is advisory. Verify its claims against the current user request and repository, and act only on applicable findings.

After completing applicable work, write one self-contained final answer to the user's active request. This final answer replaces your earlier final answer for the user. Preserve still-valid outcomes and requested explanations, incorporate all material corrections, remove superseded claims, identify current deliverables and verification actually performed, and state unresolved work or limitations.

The user must be able to understand the current result without reading earlier assistant final messages or review feedback. Do not make the final answer only a response to the reviewer. This requirement also applies when you reject every finding or make no file changes. Respect the user's requested format and brevity.
```

Use this contract when preparing the next model request, not only in the built-in reviewer prompt. A custom `post_turn_completion_review_prompt_file` must not accidentally remove the parent's continuation instructions. Re-establish the contract for a resumed review-derived main task after pause or compaction without fabricating a new user message or repeatedly appending historical advice.

The main model should reuse its current conversation, inspected files, and known results to write the replacement. Do not concatenate old answers mechanically, ask the reviewer to rewrite the answer, or create a hidden summary that is more complete than the actual final delivered to the user.

### 6.2 Reviewer instruction

Add the following semantics to the effective review-task framing, and document them for prompt-file overrides:

```text
Evaluate both the current work and candidate_final_answer against the applicable user request.

The initial_answer_reference and prior user-round answers are historical context. They may contain errors or superseded claims. They do not establish current correctness and must not compensate for omissions in candidate_final_answer.

Assess whether candidate_final_answer stands alone as the user's current answer. A response that only addresses prior review findings, drops still-required deliverables or explanations, or requires earlier final messages to be understood is actionable incomplete delivery. Set fix_actions_advised to true when concrete follow-up can correct it, even when repository changes are otherwise correct.

Do not assume an implementation is missing solely because the latest answer omits it. Inspect available artifacts and distinguish work defects from answer defects. Do not revive an old claim merely because it appeared in the historical reference.
```

Retain `PostTurnCompletionReviewOutputEvent { evaluation, fix_actions_advised }`. The evaluation can name work completeness and answer completeness separately without another public verdict field. Prior reviewer findings must not be promoted into user requirements by the next review.

### 6.3 What the UI should deliver

Keep the current streamed-main-answer behavior. Each follow-up final is a new complete replacement answer; earlier answers may remain in the waterfall. `/copy` should select the latest completed main answer. Review evaluation remains visibly separate and must not become the copyable main final.

A clean review does not require another synthesis call or a duplicate assistant answer. The candidate just reviewed is already the answer the user received. The review status may be the last physical cell in the waterfall; that is separate from which text is the latest main answer.

Buffering all finals until approval, rewriting past waterfall cells, or moving to a new final-publication protocol would affect streaming, notifications, replay, and clients. Those are different designs and are not needed for this repair.

## 7. Episode lifecycle and stale-result handling

Bind every scheduled review and pending follow-up to its `ReviewTicket`. Validate it when spawning the delegate, accepting its output, recording an advisory, and consuming a continuation. A matching task ID alone is insufficient after request revision or rollback.

|Event|Required behavior|
|---|---|
|Initial main completion|Capture the consumed request frame and exact final; establish initial reference and candidate; persist before scheduling review.|
|Valid review advising follow-up|Check the ticket and, for an automatic attempt, its allowance; record the newest advisory and replacement-answer contract; continue the same main episode without new user input. A requested manual review retains its one handoff even after the automatic cap.|
|Main follow-up completion|Replace candidate; retain initial reference and frame; schedule the next eligible review.|
|Valid review with no follow-up advised|End the automatic episode normally; keep the latest main answer. Describe the result as no actionable follow-up advised, not a proof of correctness.|
|New real user input|Invalidate outstanding tickets immediately. Track whether input is pending or consumed; start a new eligible episode only after main work includes it.|
|Stale delegate result or queued auto-review operation|Discard its side effects; do not attach advice to a newer request/candidate or silently retarget the queued operation.|
|Pause during reviewer|Preserve ticket and snapshot; keep the review continuation target; `/continue` restarts review of the same candidate unless it has been invalidated.|
|Pause during review-derived main work|Preserve episode lineage and replacement contract; resume main work, not the reviewer.|
|Abort or cancellation|Do not publish a new candidate from partial output or apply a late verdict. Keep prior completed answer and an accurate stopped status.|
|Rollback, fork, or history replacement|Restore only state belonging to surviving history, or invalidate it. Never retain a future candidate from a discarded branch.|
|Context budget exceeded|No truncated full-review request; report unavailable coverage and retain the latest answer.|
|Automatic allowance exhausted|Stop with unresolved advice visible. Do not convert the stop into a clean verdict or generate an unreviewed answer and label it approved.|

Internal automatic operations currently use the public `Op::ReviewCompletedTurn`. The implementation must carry the expected ticket through private scheduling metadata or an equivalent internal operation; the manual command can still resolve the latest eligible completion at dispatch. Otherwise a queued auto-review can accidentally review a newer task.

An `Op::Continue` queued by a positive review must likewise be bound to the ticket, rather than consuming whichever unrelated pending continuation happens to exist later. Persist a prepared/pending handoff with its advisory and ticket before enqueueing. Recovery may re-enqueue that pending ticket; main-task entry must claim it idempotently and record the started disposition so duplicates cannot start concurrent continuations. A crash after the claim is handled as interrupted main work, not a fresh advisory handoff. Do not mark a handoff consumed before it can be recovered or started. This controls duplicate scheduling; it does not guarantee exactly-once model or tool side effects across process crashes.

Current files are inspected when the delegate runs. Episode identity binds conversation state; it is not an immutable filesystem snapshot. Independent external file edits can change the reviewed state. Do not claim transactional repository consistency or add a mandatory whole-repository snapshot system for this documentation fix.

## 8. Persistence, compaction, and legacy sessions

### 8.1 Existing persistence is insufficient for task-boundary reconstruction

`CompletedTurnForReview` currently exists only in [SessionState](../../../core/src/state/session.rs), and [completed_turn_for_review](../../../core/src/session/mod.rs) falls back to raw prompt history after that cache is absent. [rollout/policy.rs](../../../core/src/rollout/policy.rs) does not persist `TurnStarted`, `TurnComplete`, `TurnPaused`, or `TurnContinued`; [rollout/recorder.rs](../../../core/src/rollout/recorder.rs) also filters older custom pause/continue events when loading/cleaning.

Therefore "replay the existing TurnComplete events" is not an implementation plan. There is no reliable existing persisted completion boundary for that approach.

### 8.2 Proposed persistence

Add a narrow versioned rollout metadata record for the review episode, for example `RolloutItem::PostTurnCompletionReviewState`. It records the exact request frame, initial reference, latest candidate, provenance, generation/revision, allowance, and handoff disposition needed to restore the active snapshot. This is a proposed variant requiring deliberate updates to rollout serialization, filtering, reconstruction, rollback/fork handling, and any generated schema/exhaustive consumers; it is not already available.

Keep this metadata outside model-facing `ResponseItem` history and outside TUI assistant/user text. Do not persist all runtime lifecycle events merely to solve this one feature. Reuse existing rollout writer ordering and flushing; make completion capture durable before scheduling its review. Persist a prepared handoff before enqueue, then durably claim it at main-task entry; recover pending and started records differently. The prepared record must contain enough advisory/ticket data to recover a missing enqueue without adding duplicate developer instructions. Coordinate these operations with cancellation and transcript replacement.

For a first implementation, a bounded snapshot record is simpler than a new database or a mutable sidecar. It may duplicate answer text already present in the append-only rollout. The active state and per-review input are bounded; the audit log still grows with performed work, as it already does. An implementation can later reference immutable rollout text if it preserves exact text and surviving-history ownership.

Do not interpret model-produced XML as trusted host metadata. Legacy model text cannot manufacture an episode ID, completion boundary, or successful review status.

### 8.3 Compaction

[replace_compacted_history](../../../core/src/session/mod.rs) replaces the prompt history. It must not rewrite or discard the active request frame and answer snapshots. On the next review-derived main request, the host can supply the bounded frame/candidate and output contract when they are no longer present in the compacted conversation, under the same input budget rules.

Preserve the existing guard in [on_task_finished](../../../core/src/tasks/mod.rs) that suppresses completion-review hooks when work-notes compaction is incomplete. Assistant work notes are not a final answer. A failed compaction, partial streaming answer, or interrupted synthesis cannot become a candidate solely because it is assistant-role text.

### 8.4 Rollback and fork

The inspected `replace_with_reconstructed_rollout` and `clear_reconstructed_turn_metadata` paths do not clear `last_completed_regular_turn_for_review`. This is an adjacent stale-cache risk in the current code. The new snapshot must have explicit invalidation/reconstruction on these paths rather than extending that behavior.

Replay only records whose completion and request basis survive rollback. Fork at the selected history boundary, carry only surviving request/answer state, assign branch-local generation, and clear pending delegate/handoff operations. Compaction is a representation replacement of surviving work; rollback is a change to which work survives. They must not share a blanket "keep latest cached episode" rule.

### 8.5 Legacy fallback

Old rollouts lack the proposed metadata. Use only boundaries and final text that can be established from the available history; do not call the first surviving assistant message the episode's initial answer merely because it appears before a review-looking string.

If the initial reference cannot be recovered, set it to unavailable and use the latest candidate plus an explicitly qualified request frame. If required user scope or terminal-answer identity cannot be established, report incomplete review context rather than certifying the result. A subsequent genuine main completion under the new capture path establishes an exact snapshot. No unbounded reconstruction of every old review exchange is required.

## 9. Review failures and realistic guarantees

The current parser maps malformed or empty review output to `fix_actions_advised = false`; see [parse_post_turn_completion_review_output_event](../../../core/src/tasks/post_turn_completion_review.rs). That means the existing false boolean is not equivalent to a successful clean review.

Keep the public two-field output object, but distinguish a successfully parsed nonempty evaluation from unavailable/invalid review internally. A proposed parser can return a `Result` or internal outcome enum. Parse failure, missing output, execution failure, incomplete mandatory context, and cancellation must not become a valid no-follow-up verdict. Display a diagnostic or an exit without structured review output using the existing event facilities. Do not autonomously apply raw malformed prose as advice.

|Property|What the design can establish|
|---|---|
|Context identity|The reviewer evaluates the exact latest completed main answer for the matching request revision.|
|Bounded revision retention|At most initial reference plus latest candidate for the current episode; no cumulative review verdicts.|
|Freshness of conversation context|Every delegate starts with `InitialHistory::New` and the explicit envelope.|
|Output consistency|The candidate text is the main final already delivered to the user, not a separate optimistic summary.|
|Lifecycle correctness|Stale tickets and discarded-history candidates cannot continue new work; pause/replay preserve or explicitly degrade context.|
|Semantic completeness|A requirement placed on the main model and assessed by the independent reviewer. A prompt, JSON boolean, or length threshold cannot prove it.|

If the latest candidate remains incomplete after a stop or failure, disclose that the workflow did not establish complete delivery. Do not call it accepted merely because automatic work has stopped. The proposal improves and checks communication completeness; it does not promise an impossible deterministic proof about arbitrary natural language.

## 10. Implementation scope and sequence

|Surface|Proposed change|
|---|---|
|`core/src/state/session.rs`|Represent an episode, exact candidate/reference, request ownership, and pending review ticket.|
|`core/src/tasks/mod.rs`|Capture terminal main results from `on_task_finished`'s completed-task result; preserve work-notes exclusion; bind auto-review and continuation scheduling to tickets.|
|`core/src/tasks/continue_task.rs`, `core/src/session/handlers.rs`, `core/src/session/turn.rs`|Carry review lineage and consumed-input ownership through continue/pause/resume; stop treating raw role segmentation as live completion authority.|
|`core/src/session/mod.rs`|Install/replace snapshots; restore/invalidate them on compaction, resume, rollback, and fork.|
|`core/src/tasks/post_turn_completion_review.rs`|Render the bounded envelope, apply budgets, add replacement-answer handoff, reject stale results, distinguish invalid review output.|
|`core/post_turn_completion_review_prompt.md`|Assess exact latest-answer completeness; explain historical-reference semantics and independent inspection.|
|`core/src/rollout/*`, rollout type/schema consumers|Persist narrow versioned review state and replay it with surviving-history ownership.|
|`tui/src/chatwidget/{replay,streaming,turn_runtime}.rs`|Retain current answer streaming and copy behavior; show unavailable/capped review accurately if new status rendering is needed.|
|`core/src/codex_delegate.rs`|Keep generic fresh one-shot runner semantics; do not embed episode or summarization logic here.|

Recommended order:

1. Add a regression fixture using one real user request and two review passes. Separately test the renderer's bounded packet and the parent's replacement-answer instruction.
2. Implement live episode capture, ownership, and the two-slot envelope together. Keep the first reference stable and use authoritative terminal output for the candidate.
3. Add narrow durable state, compaction/resume/rollback/fork rules, and stale-ticket checks before claiming cross-restart support.
4. Add explicit latest-answer review criteria, invalid-output handling, and effective-model payload budgets. Cover configured prompt overrides.
5. Add the finite automatic-attempt policy if included in this iteration; otherwise explicitly leave execution count unbounded and do not claim the context bound limits runtime.

A prompt-only patch is a useful early mitigation, but must not be described as the complete fix. A larger public verdict schema, a new agent, a hidden finalizer, and a TUI buffering protocol are not prerequisites.

## 11. Validation plan

These are tests to implement with the runtime change, not tests reported as executed for this document update.

|Case|Required assertion|
|---|---|
|Exact reported shape: `U1, A0, R1(true), A1, R2`|Second delegate receives `U1`, historical `A0`, and candidate `A1`; no fabricated `U2`; no `R1`, reasoning, tools, or intermediate messages.|
|Many same-request iterations|Packet contains no more than two episode answer texts, even after ten synthetic completions; request frame is unchanged.|
|First attempt or identical answers|Answer text serialized once; candidate identity remains exact.|
|Three corrections|Fixtures supply a replacement `A2` carrying earlier still-valid corrections; no missing-middle-round reconstruction is claimed.|
|Delta-only latest answer with correct repository|Independent behavioral evaluation identifies answer incompleteness; the reference does not make it pass.|
|All reviewer findings rejected|Continuation still receives the replacement contract; returned final addresses the active request.|
|Stale or incorrect initial reference|Reviewer trusts current evidence and latest candidate, not a superseded claim.|
|Multiple genuine user rounds|Keep the existing `U1, A0, U2, A1` behavior, including references to earlier options and later cancellation of old scope.|
|Consumed steering versus late pending input|Only consumed inputs belong to the reviewed completion; late input invalidates scheduling and remains pending for main work.|
|Queued operation or late verdict after new input/rollback|No advisory or continuation reaches the newer request; same-ticket handoff occurs at most once.|
|Pause in reviewer versus main follow-up|Resume the correct task kind with the same valid snapshot and main output contract.|
|Resume after completed task or review handoff|Restore exact metadata and handoff disposition without depending on nonpersisted `TurnComplete` events.|
|Crash before enqueue, after enqueue, or after task claim|Recover prepared handoff without duplicate advice, claim each ticket idempotently, and resume a started task through interruption recovery; do not assert exactly-once tool effects.|
|Compaction and incomplete work notes|Compaction preserves review snapshots; incomplete notes never become final answers.|
|Rollback and fork|Remove future candidates and pending review work; replay only surviving state.|
|Legacy rollout|Missing reference/scope is explicit; arbitrary assistant text is not promoted to a known initial completion.|
|Budget pressure|Optional reference omitted before mandatory scope/candidate; mandatory overflow prevents a false full review.|
|Invalid/empty output or delegate error|No clean-review status, no raw-text automatic advisory, latest main answer retained.|
|TUI and copy|Latest main final is the candidate tested; reviewer JSON/status does not replace it as the copyable response.|
|Automatic cap, if implemented|Bounded attempts; unresolved status at exhaustion; retry/pause does not reset allowance.|
|Prompt-file override|Input/reference semantics and parent continuation contract remain in the task framing.|

Separate deterministic harness tests from model-quality evaluation. Mock SSE fixtures can establish packet contents, identities, event ordering, and continuation instructions; they cannot prove a model will always write a complete answer. Use a small behavioral evaluation set for answer self-containment, rejected findings, and contradictory references. Run repository tests through the prescribed `cargo-local`/`just-local` wrappers and network-sandbox policy when implementing the change.

## 12. Acceptance criteria

The implementation is ready when the exact same-user multi-review chain is covered, the reviewer receives the bounded specified packet, each continuation receives the complete-answer contract, lifecycle/persistence cases preserve ownership, and invalid or incomplete reviews cannot appear as clean verdicts.

The user-facing success condition is that the latest main answer describes the current result of the active request at the requested level of detail. Earlier answers and review reports can remain visible, but the user should not need to combine them to understand what was delivered, what was checked, and what remains unresolved.
