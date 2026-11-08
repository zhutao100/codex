# Completion-Review Request Prefix

## Contract

- The advisory is ordinary developer history. The exact request/candidate frame and replacement-answer contract remain request-only; they are not additional transcript or rollout items.
- [Sampling](../../../core/src/session/turn.rs) inserts the frame after the last system/developer/user message or encrypted compaction item. If none exists, insert at the beginning. This preserves the existing prefix through the advisory and keeps the frame before subsequent reasoning, assistant messages and tool activity.
- Derive placement from the prepared input, not a nonpersisted message ID, text search inside tool output, or a new saved history offset. Replay therefore reconstructs the same position; compaction and steering establish a new boundary without additional metadata.
- Ordinary tool rounds retain the entire preceding request and output as a prefix. New scope, advisory, compaction, model/tool changes or budget-driven reference omission may legitimately require a full request.
- Do not move the frame back to the transcript tail, prepend it to the entire ordinary history, omit mandatory snapshots, or bypass the client's prefix checks.
- Keep [budget accounting](budget_accounting.md) unchanged: the frame is request-only, its previously reported cost receives replacement credit, and input-history coverage counts only actual history items.

```text
Before:
  H + advisory + frame
  H + advisory + response_1 + tool_results_1 + frame

After:
  H + advisory + frame
  H + advisory + frame + response_1 + tool_results_1
```

The [WebSocket client](../../../core/src/client.rs) requires `current_input = previous_input + previous_response_output + delta` before using `previous_response_id`. Physical socket reuse does not establish this logical continuation.

## Historical Classification

|Commit|Date|Relevant behavior|
|---|---|---|
|`57d219cd6d`|2026-05-07|Original post-turn review records its advisory once through `record_conversation_items`, then schedules main continuation. There is no per-sampling request-only frame.|
|`ad119181e3`, `575c803502`|2026-06-20|WebSocket prefix comparison uses borrowed inputs; physical connections can span logical turns while response chains remain turn-scoped.|
|`cee5afba34`|2026-10-29|Durable bounded episodes introduce `completion_review_main_instructions` and append its request-only frame on every sampling iteration. This introduces the prefix regression.|
|`f3f48e2066`|2026-10-29|Makes missing mandatory follow-up snapshots fatal; retains tail placement.|
|`91aec90b3e`, `4050fb8f00`, `a6f32997a8`, `69c46178c1`|2026-11-07|Correct usage, frame credit, reasoning semantics and covered-history boundaries; do not fix request ordering.|

The prefix problem is a regression in review request assembly, not an original missing feature in WebSocket continuation. Before this fix, review HTTP tests checked snapshot presence and budgets while WebSocket tests checked append-only requests independently; neither exercised their interaction.

## Observed Signature And Limits

- The investigated retained conversation contains 117 distinct usage updates. Its first 52 responses grow the cached prefix; all 65 subsequent main responses report 122,368 cached tokens while input grows from 123,862 to 210,770.
- Deduplicate usage notifications by cumulative accounting before counting responses. The retained record has 118 repeated notifications and one without usage.
- An offline item-order projection preserves the request/output prefix for all 51 pre-review transitions and breaks it at the frame for all 63 within-follow-up transitions. It is not a reconstruction of the server-rendered prompt.
- The real localhost WebSocket regression reproduces the client failure: the second main follow-up lacks `previous_response_id` before the fix.
- Losing incremental continuation does not alone prove a frozen server cache. Consecutive full requests can still share growing prefixes; backend cache lookup/write behavior and usage reporting were not captured. Three later sampling iterations are also absent from the retained rollout, so its totals do not cover every process request.

## Verification

- [WebSocket regression](../../../core/tests/suite/client_websockets.rs): initial main answer, independent positive review and three main follow-up requests use two physical connections. The first follow-up is full; the next two reference the immediately preceding responses and contain only their tool results. Reasoning returned by the first follow-up remains in the logical response chain.
- [HTTP regressions](../../../core/tests/suite/completion_review.rs): compare full input prefixes across live/restarted review, near-budget tool rounds, pause/shutdown/replay, and actual compaction or model-selection recovery followed by another tool round. Exact scope/candidate contracts and genuine-overflow rejection remain enforced.
- [Placement cases](../../../core/src/session/completion_review_tests.rs): recorded advisory, steering without a handoff, encrypted compaction with exact notes, assistant-only compacted history and empty history.
- These are scripted transport and request-shape checks, not live-model cache measurements. A live comparison must inspect actual request chaining and server usage; synthetic usage counters cannot establish that the observed 122,368-token plateau is repaired.
