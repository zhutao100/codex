# Completion-Review Budget Accounting

## Audit Scope

|Source snapshot|Scope|
|---|---|
|`a6f32997a8`|Legacy fork, including `91aec90b3e`, `4050fb8f00` and `a6f32997a8`.|
|`67f0994b53`|Upstream source; contains reasoning-accounting fix `c62e9df39f` and usable-usage-boundary fix `7a030b9769`.|
|`89b8052566`|Customized descendant of `67f0994b53`; preserves those upstream accounting paths with cached item estimates.|

The audit confirmed that the reported premature rejection was fixed and the original rollout did not require repair. It also identified a missing-usage undercount in the legacy ledger. That remaining P2 is now fixed by the boundary contract below. Neither comparison snapshot implements this completion-review workflow, so forwarding the three original commits is not an applicable upstream fix.

## Usable Usage Boundary

The ledger tracks the history prefix actually covered by its usage baseline, independently of the latest model output.

- [Sampling](../../../core/src/session/turn.rs) captures the raw input-history count before the response. Positive completed usage covers that input and subsequent model output, including successful empty responses.
- [History accounting](../../../core/src/context_manager/history.rs) estimates every item after that boundary. Missing/zero usage, failed streams and cancellation leave it unchanged. An initial zero report still produces its UI event but covers no history; it cannot replace an existing positive baseline. Historical reasoning before the boundary is estimated only when excluded by the baseline; tail reasoning is never added twice.
- [Persistence](../../../core/src/session/mod.rs) writes optional `last_usage_item_count` in the existing `TokenUsageContext` record with the baseline, frame credit and reasoning semantics. This happens for every positive usage report, not only review frames.
- [Replay](../../../core/src/session/rollout_reconstruction.rs) restores the tuple while rebuilding history. Legacy records infer their boundary at the baseline record's position, not from later output. Duplicate UI token events preserve it; explicit counts also recover input-only responses and full local estimates.
- Continuation cleanup preserves the covered unchanged prefix and retains host metadata. Removing a covered suffix can conservatively retain its reported token cost; it cannot move coverage into unreported output.
- Compaction/local recomputation covers all installed items, including preserved notes. Rollback, history edits and full-window baselines keep the boundary in range and reset frame credit where required.

## Regression Evidence

The original undercount predated the three fixes in compaction accounting, but `91aec90b3e` also made the hard review guard depend on it. It was not the cause of the original premature rejection.

Executed HTTP regression:

1. Complete the initial answer with 4,000 reported tokens; obtain positive review advice.
2. Return a follow-up with 96,000 bytes of reasoning text, an `update_plan` call, and `response.completed` with no `usage` field.
3. The configured effective window is 22,800 tokens, leaving a 14,608-token follow-up budget after the output reserve.
4. The new reasoning alone estimates to approximately 24,000 tokens. Before the fix, a fourth Responses POST was sent. The fixed guard rejects it after three POSTs, both live and after restart.

The same HTTP sequence with a stream ending before completion retains the reasoning and rejects `/continue`, including after restart. Four cases executed actual localhost transport in 0.41 seconds combined test time. Unit coverage includes positive/zero/missing usage, empty successful responses, legacy and explicit replay boundaries, duplicate UI events, continuation cleanup and preserved compaction notes.

The implementation adopts usable-usage boundary semantics from `7a030b9769` while retaining this fork's session/history types, frame-credit contract and reasoning metadata. It additionally persists the boundary for restart and reconstructs legacy accounting in history order. No new rollout variant or public token-event field is introduced.

## What The Three Commits Fix

|Commit|Corrected failure|Assessment|
|---|---|---|
|`91aec90b3e`|The hard guard re-estimated the entire accumulated history from bytes while compaction used reported usage.|Uses the positive usage baseline plus its tail; retains byte-estimate fallback, the 8,192-token reserve, optional-reference omission and mandatory-overflow rejection. The later boundary fix makes that tail include unreported model output.|
|`4050fb8f00`|Reported usage already included the request-only review frame, but the next tool round charged that frame again.|Charges the replacement delta and independently bounds the new frame. Persists frame credit with the complete baseline; resets it for compaction/local baselines and preserves metadata during continuation cleanup.|
|`a6f32997a8`|Restart/task startup or an incomplete stream could lose or change whether the baseline already included historical reasoning.|Commits reasoning semantics only with completed usage and restores them with the baseline. Local estimates include retained reasoning; missing legacy flags remain conservative.|

The latter two fixes prevent additional false rejections. They are not both necessary to explain this particular original rollout: it contains one genuine user round and no `token_usage_context` records, so historical-reasoning inclusion cannot be recovered by inference. Old sessions establish new accounting on a successful request; the changes do not rewrite them.

## Original Rollout Evidence

|Check|Observed result|
|---|---|
|Parsing|All 1,213 JSONL records parse; 556 are response items and 13 are completion-review snapshots.|
|Tool pairing|All 122 function calls and 49 custom calls have matching outputs; no unmatched calls or outputs.|
|Continuation|Latest checkpoint is version 2, with exact text-only request provenance, a Started handoff, replacement-answer ownership and no pending input.|
|Usage|Last reported total is 245,691 tokens; the following tool output estimates to 25 tokens. Recorded effective window is 364,560 tokens, leaving 356,368 after the reserve.|
|Old heuristic|Summing serialized raw response-item estimates gives 357,459 tokens before base instructions and the injected review frame. This is a diagnostic raw-history estimate, not an exact tokenizer count or a reconstruction of the final normalized request.|
|Production recovery probe|Resume an isolated copy using the production loader and `/continue`, retaining the recorded model, candidate directory and effective window. One mocked Responses POST completes; exact user scope, candidate, initial-answer reference and replacement contract are present.|
|Original-file integrity|Unchanged SHA-256: `9c5d1666704fadeaa190b01dd5a10e1c33f6a69e120235ede5bcdd0ccdfdc5cd`. No original record was appended, removed or repaired.|

The fatal UI error itself is not in the persisted events. The diagnosis combines intact recovery state, the conflicting estimates, source tracing and successful copied-rollout recovery rather than claiming the log records the fatal event.

The recovery probe substitutes a localhost model response and disables automatic review and shell snapshots. It establishes replay/request transport and exact snapshot preservation, not live-model acceptance or completion of the original work.

## Upstream Comparison And Alignment

- Neither comparison snapshot has `ReviewCompletedTurn`, `auto_post_turn_completion_review`, the completion-review state record, or this hard payload guard. Generic `/review` and Guardian delegates are different workflows; the continued presence of `codex_delegate.rs` does not establish feature parity.
- `c62e9df39f` persists `last_usage_includes_reasoning: Option<bool>` directly in `TokenUsageInfo`, commits stream-local semantics with completed usage, and marks local estimates as reasoning-inclusive. This is the native equivalent of the shared reasoning fix; do not restore a session-global flag or add the legacy rollout record to duplicate it.
- `7a030b9769` tracks `last_server_reported_usage_item_count` during live sampling, including successful requests with no output. It estimates retained output after missing-usage/failed responses. The customized descendant retains that boundary while replacing repeated item estimation with cached estimates.
- Upstream's restored/rewritten histories still use an explicitly documented last-model-output approximation. The live-boundary fix is not evidence of complete persisted-boundary recovery for every missing-usage tail.
- There is no upstream request-only completion-review frame to credit or replace. If this feature is ported, keep its frame delta separate from the native usage baseline and preserve upstream request-history snapshots, envelopes and async attachment resolution.
- No runtime patch is required in either comparison snapshot for the reported error. The legacy fix follows the upstream accounting semantics without cherry-picking old session/protocol owners over their replacements.

## Verification Boundary

- Focused completion-review tests exercise reported-usage overflow, long histories, near-budget frame reuse, multi-round reasoning, live/restart continuation and compaction/model-selection recovery.
- The audit's temporary probes established private-rollout recovery and the original undercount. Permanent regressions now cover missing-usage/failed-stream rejection live and after restart; the private rollout remains unchanged.
- No live-model run, upstream rebuild or workspace-wide all-feature suite is claimed. The comparison is pinned to the source snapshots above, not a claim about a later remote tip.
