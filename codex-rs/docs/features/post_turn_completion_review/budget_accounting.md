# Completion-review budget accounting

Read this before changing review payload limits, usage accounting, compaction baselines or their replay.

## Two budgets

|Request|Budget policy|
|---|---|
|Independent reviewer|Use the resolved review model's effective window. Reserve half for tools/output/inspection, then subtract base/project/developer instructions, static tool schemas and 8,192 tokens from the packet allowance.|
|Review-derived main follow-up|Use the main model's effective context window minus 8,192 tokens. Reuse the session usage ledger and charge the replacement delta for the request-only review frame.|

Deduplicate identical initial/latest answers. Omit the optional initial reference first; if exact scope and candidate still do not fit, stop without truncation. Estimates are approximate, not tokenizer guarantees. Reviewer compaction invalidates coverage rather than authorizing a verdict against rewritten scope.

## Usable usage boundary

[History accounting](../../../core/src/context_manager/history.rs) keeps a baseline and the history prefix it covers independently of the latest model output. [Sampling](../../../core/src/session/turn.rs) supplies the raw input-history count before the response.

- Positive completed usage covers submitted input and subsequent model output, including a successful response with no output items.
- Missing/zero usage, failed streams and cancellation retain the previous positive baseline and boundary. Initial zero usage can produce a UI event but covers no history.
- Estimate every item after the boundary, including output retained from failed or usage-less responses. Do not advance coverage merely because a model item exists.
- Estimate older encrypted reasoning before the boundary only when the baseline excludes it. Tail reasoning is already charged with tail items.
- Commit transport reasoning-inclusion semantics only with completed positive usage. A new task, incomplete stream or absent usage cannot change what the preceding baseline included.

For a positive baseline, the main guard uses the ledger estimate plus `new_frame_tokens - baseline_frame_tokens`, bounded below by the new frame's own cost. The subtraction can be negative when a replacement shrinks. Reported usage already includes the old frame, so adding another complete frame double-counts it. A full-history byte estimate is a fallback only when no positive baseline exists; accumulated reasoning estimates must not override usable reported accounting.

## Persistence and history edits

[Session persistence](../../../core/src/session/mod.rs) writes `TokenUsageContext` with the complete baseline, covered item count, frame credit, reasoning inclusion and the request model that produced positive usage. `/continue` can change that model without a new user boundary. A local estimate has no measurement owner; older records without `source_model` retain the previous-turn hint, not proof of ownership. None of these fields are model input.

[Replay](../../../core/src/session/rollout_reconstruction.rs) restores this tuple in history order. A complete host record is sufficient even without its following UI event. An identical repeated UI event cannot cover later unreported output. A different unannotated baseline has no known frame credit or reasoning guarantee. Legacy records infer coverage at that record's position, not at the final rollout tail; absent reasoning flags remain conservative.

Continuation cleanup preserves the covered unchanged prefix without extending coverage into unreported output. `Compacted` commits the replacement and ends the old usage baseline: preserve lifetime totals, estimate installed history (including notes and reasoning) and clear frame credit and measurement ownership. Restart does not need the adjacent turn context or subsequent usage record to stop charging the old prompt. If no usage was recorded, legacy history remains unmeasured. Rollback keeps the covered boundary within the remaining history.

Do not duplicate this ledger when integrating another usage representation. Preserve baseline ownership, coverage and reasoning semantics in the target implementation; request-only review frame credit remains a separate adjustment.

## Recovering from overflow

The ordinary auto-compaction threshold is not the review payload limit. On a new handoff, try full then mandatory, including earlier frames. Once admitted, keep the choice even if usage falls. If a sampled full frame no longer fits, compact instead of dropping the optional reference mid-history. An unsampled frame fitting *alone* does not imply it fits a helper: reserve the capture marker and notes output room, and the V2 trigger and effective window. If either estimate would overflow, freeze the [outgoing work](request_prefix.md) without the new frame and persist the deferral until installation. Resuming into a larger window does not change an in-flight checkpoint. The installed history gets a fresh selection; mandatory scope and candidate are never truncated.

Within one run, the review guard does not compact again without an intervening successful ordinary sample. If the rebuilt request still exceeds the limit, stop before sending it; the user can compact further or select a larger main model, then `/continue`. If the mandatory frame alone is too large, skip the review-triggered compaction and ask for a larger window. Neither path truncates the scope or candidate.

A budget stop preserves `/continue` ownership across standalone `/compact` and restart. Explicit main-model selection updates the durable recovery destination even after steering invalidates the advisory handoff; it does not alter the snapshots or the notes capture's source model.

## Regression obligations

[Completion-review HTTP tests](../../../core/tests/suite/completion_review.rs), [history tests](../../../core/src/context_manager/history_tests.rs) and [session tests](../../../core/src/session/completion_review_tests.rs) cover long reported-usage histories, near-limit frame reuse, multi-round encrypted reasoning, missing/zero usage, failed streams, successful empty responses and live/restarted continuation. Keep tests for actual `/compact` or model selection followed by `/continue`; restoring a snapshot alone does not exercise the advertised recovery path.

Use captured requests to distinguish notes and compaction from ordinary sampling. Verify that recoverable overflow resumes with the exact frame, and that an irreducible or still-over-budget prompt sends no ordinary follow-up. Live model acceptance, summary quality and cache behavior require separate evidence.
