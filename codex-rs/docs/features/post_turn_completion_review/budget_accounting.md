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
- For ordinary capacity, reserve older encrypted reasoning before the boundary when the baseline excludes it. Tail reasoning is already charged with tail items. When estimating whether an advisory or candidate fits the two compaction helpers, use the [measured helper prefix](../session-work-notes.md#fit-and-prefix-reuse) rather than treating the capacity reserve as new content.
- Commit transport reasoning-inclusion semantics only with completed positive usage. A new task, incomplete stream or absent usage cannot change what the preceding baseline included.

For an attributable positive baseline, apply the signed differences in base instructions, advertised tool schemas and review frames. Charge uncovered results at their destination wire size, including paired shell/patch formatting and image placeholders. Keep the review's reasoning reserve and the ordinary pressure trigger separate. Both new and pinned frames use the ordered tools selected for the request; the outgoing helpers use their own source. If the model, base or measured tool cost cannot be attributed, price the destination locally.

## Persistence and history edits

[Session persistence](../../../core/src/session/mod.rs) writes `TokenUsageContext` with the receipt, measured base and tool costs, covered item count, local modality cost, signed result-format adjustment, frame credit, reasoning inclusion and request model. These costs are evidence for admission and [helper fitting](../session-work-notes.md#fit-and-prefix-reuse), not billed usage. Equal tool costs do not imply equal schemas or a cache hit. An older record can recover a formatting difference from intact paired calls and results, not prove the cost of deleted calls or unrecorded tools. None of this metadata is model input.

[Replay](../../../core/src/session/rollout_reconstruction.rs) restores this tuple in history order. A complete host record is sufficient even without its following UI event. An identical repeated UI event cannot cover later unreported output. A different unannotated baseline has no known frame credit or reasoning guarantee. Legacy records infer coverage at that record's position, not at the final rollout tail; absent reasoning flags remain conservative.

Continuation cleanup preserves coverage of surviving measured items, not just the prefix before the first deleted call. Unreported results still cost their projected size once. `Compacted` ends the old baseline: preserve lifetime totals, estimate the installed history and clear frame credit and measurement ownership. Restart does not need an adjacent turn context or subsequent receipt to stop charging the old prompt. Rollback keeps coverage within the remaining history.

Do not duplicate this ledger when integrating another usage representation. Preserve baseline ownership, coverage and reasoning semantics in the target implementation; request-only review frame credit remains a separate adjustment.

## Recovering from overflow

The ordinary auto-compaction threshold is not the review payload limit. Before recording incoming advice, charge it against the outgoing work and both helpers. Then try the full and mandatory frame projections, including earlier frames. Reserve the capture marker and notes output room, and the V2 trigger and effective window. Leave an addition that would displace fitting work in the durable handoff until installation; a larger resume window does not cancel the deferral. Once sampled, keep the chosen frame even if usage falls. If a full frame no longer fits, compact the [unchanged work](request_prefix.md) rather than dropping its optional reference. The replacement gets a fresh selection; mandatory scope and candidate are never truncated.

Within one run, the review guard does not compact again without an intervening successful ordinary sample. If the rebuilt request still exceeds the limit, stop before sending it; the user can compact further or select a larger main model, then `/continue`. If the mandatory frame alone is too large, skip the review-triggered compaction and ask for a larger window. Neither path truncates the scope or candidate.

A budget stop preserves `/continue` ownership across standalone `/compact` and restart. On a downshift the destination sets the ordinary review budget, but the larger model that measured the outgoing work sets the notes and V2 budgets until the destination samples. This applies even when review triggers compaction below the ordinary auto-compaction threshold. Explicit model selection changes the recovery destination, not the snapshots or an outstanding capture's source.

## Regression obligations

[Completion-review HTTP tests](../../../core/tests/suite/completion_review.rs), [history tests](../../../core/src/context_manager/history_tests.rs) and [session tests](../../../core/src/session/completion_review_tests.rs) cover long reported-usage histories, near-limit frame reuse, multi-round encrypted reasoning, missing/zero usage, failed streams, successful empty responses and live/restarted continuation. Keep tests for actual `/compact` or model selection followed by `/continue`; restoring a snapshot alone does not exercise the advertised recovery path.

Use captured requests to distinguish notes and compaction from ordinary sampling. Verify that recoverable overflow resumes with the exact frame, and that an irreducible or still-over-budget prompt sends no ordinary follow-up. Live model acceptance, summary quality and cache behavior require separate evidence.
