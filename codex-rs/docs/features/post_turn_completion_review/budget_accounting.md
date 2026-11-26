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

For a positive baseline from the same [model and route](../session-work-notes.md#fit-and-prefix-reuse), apply the signed differences in base instructions, advertised tools, surviving input projection and review frames. Charge uncovered results at destination wire size. Keep the review reasoning reserve and ordinary pressure trigger separate. New and pinned frames use the destination's ordered tools; outgoing helpers use their source. Without an attributable receipt, base or tool cost, price the destination locally.

## Persistence and history edits

[Session persistence](../../../core/src/session/mod.rs) records the model, provider key, endpoint fingerprint and actual image/result format with each positive receipt, alongside coverage, measured costs, frame credit and reasoning inclusion. These are host evidence for admission and [helper fitting](../session-work-notes.md#fit-and-prefix-reuse), not billed usage or model input. Equal tool costs do not imply equal schemas or a cache hit. A legacy model name or provider key cannot prove which endpoint produced its receipt.

[Replay](../../../core/src/session/rollout_reconstruction.rs) restores this tuple in history order. A complete host record is sufficient even without its following UI event. An identical repeated UI event cannot cover later unreported output. A different unannotated baseline has no known frame credit or reasoning guarantee. Legacy records infer coverage at that record's position, not at the final rollout tail; absent reasoning flags remain conservative.

Continuation cleanup preserves coverage of surviving measured items, not just the prefix before the first deleted call. Unreported results still cost their projected size once. `Compacted` ends the old baseline: preserve lifetime totals, estimate the installed history and clear frame credit and measurement ownership. Restart does not need an adjacent turn context or subsequent receipt to stop charging the old prompt. Rollback keeps coverage within the remaining history.

Do not duplicate this ledger when integrating another usage representation. Preserve baseline ownership, coverage and reasoning semantics in the target implementation; request-only review frame credit remains a separate adjustment.

## Recovering from overflow

First price the destination request: work, actual tools, anchored frames, incoming advice once and the mandatory frame as it will render after admission. If it fits, do not consult the smaller helper budget. Before starting a new checkpoint, including a downshift, price an empty replacement with the destination base and its [canonical context](../../../core/src/session/mod.rs): policy, developer and collaboration instructions, project instructions and environment. Mid-turn installation inserts these items; a fresh turn seeds them after an installation without a reference. Charge them once in this minimum, not again against ordinary measured work.

For a review handoff with no pending input, check two independent limits before starting new helpers. Preview the refused advisory's placement on the outgoing source first. If carried, price it inside the installed mandatory frame; if deferred, price the separate advisory plus the replacement-answer contract. Charge mandatory scope, candidate and destination tools, not old frames or optional references. [Streaming V2](../remote_compaction_v2/design.md) must also fit fixed history, instructions and tools under its installation target without charging request-only review text. Include an existing notes wrapper when a structured source cannot replace it. Publish the same placement and recovery obligation only after both checks pass.

Failure of these preflight checks sends neither helper. This is not a general capacity-compaction guarantee: non-review work and reviews with pending inputs can reach installation checks after helper requests. On `/continue`, recheck a frame-only deferral before starting capture; an already recorded capture decision or notes marker finishes first.

Advisory history and frame selection are separate writes. Before capacity preflight, replay recognizes the advisory at its recorded position only if the same current, started handoff owned the append. A later interrupt marker does not undo admission; a matching developer message written before the claim is not admission. Price the admitted form, but do not insert a new frame there after intervening work. [Recorder and request tests](../../../core/src/session/live_review_dispatch_tests.rs) exercise both sides of the item write, including a completion without usage between authenticated endpoints.

Passing the fixed-cost checks does not guarantee installation: a returned summary or fresh notes can exceed the destination limit after both helpers ran. For a valid replacement, reserve the mandatory ordinary review and its output headroom before retaining optional user history. A user message's retained copy is optional; its exact review scope is not. If the installed result leaves no room for review even without optional history, keep the checkpoint and stop before ordinary inference.

Unless work already owes a capacity checkpoint, only the [authenticated sender](../../../core/src/client.rs) decides ordinary review overflow, before HTTP POST or WebSocket `response.create`. A measured estimate can be above or below the local price: neither an advisory nor a pinned frame starts helpers based on a predicted login. On an unknown or different endpoint, price projected input, base and ordered tools locally. Try the mandatory alternative only for the never-dispatched frame. After refusal verify both destination floors before publishing a checkpoint obligation. A refused send is neither a transport retry nor a cache receipt.

For recoverable overflow, reserve the capture marker and output room for notes and the trigger for V2. Leave an addition that would displace otherwise fitting work in the durable handoff until installation. Once sampled, compact the [unchanged work](request_prefix.md) before dropping its optional reference; the installed history gets a fresh selection.

Within one run, the review guard does not compact again without an intervening successful ordinary sample. If the rebuilt request still exceeds the limit, stop before sending it; the user can compact further or select a larger main model, then `/continue`. If the mandatory frame alone is too large, skip the review-triggered compaction and ask for a larger window. Neither path truncates the scope or candidate.

A budget stop preserves `/continue` ownership across standalone `/compact` and restart. On a downshift the destination sets the ordinary review budget, but the larger model that measured the outgoing work sets the notes and V2 budgets until the destination samples. This applies even when review triggers compaction below the ordinary auto-compaction threshold. Explicit model selection changes the recovery destination, not the snapshots or an outstanding capture's source.

## Regression obligations

[Completion-review HTTP tests](../../../core/tests/suite/completion_review.rs), [history tests](../../../core/src/context_manager/history_tests.rs) and [session tests](../../../core/src/session/completion_review_tests.rs) cover long reported-usage histories, near-limit frame reuse, multi-round encrypted reasoning, missing/zero usage, failed streams, successful empty responses and live/restarted continuation. Keep tests for actual `/compact` or model selection followed by `/continue`; restoring a snapshot alone does not exercise the advertised recovery path.

Use captured requests to distinguish notes and compaction from ordinary sampling. Verify that recoverable overflow resumes with the exact frame, and that an irreducible or still-over-budget prompt sends no ordinary follow-up. Live model acceptance, summary quality and cache behavior require separate evidence.
