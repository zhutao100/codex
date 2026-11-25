# Completion-review request prefix

Read this when changing request-only review framing or its interaction with Responses WebSockets.

## Placement contract

The advisory is ordinary developer history. Exact request/candidate frames are request-only developer messages. Their selections are saved in host review metadata, not appended to the transcript. Independent reviewer packets still contain only the current scope, initial reference and latest candidate.

[Ordinary sampling](../../../core/src/session/turn.rs), notes and compaction use [the same anchors](../../../core/src/completion_review.rs). On first admission, record the exact frame and its position after the last system/developer/user message or encrypted compaction item in the work (zero if absent). Count those boundaries, not raw item offsets: ghost snapshots and normalized tool results do not move the frame. Capture controls, retries and later acknowledged input are not anchors for earlier work.

|Request|Logical input|
|---|---|
|First follow-up|History, advisory, fixed review frame.|
|Next tool round|The preceding input, preceding model output, new tool results.|
|Another review of the same consumed request|The previous work including its anchored frames and latest answer, then the new advisory and current frame. Identical advice after intervening work is a new handoff. Only the latest frame defines the current candidate.|
|Notes / streaming compact|The same outgoing work including sampled frames and any prior verbatim notes wrapper, then the capture marker / compaction trigger respectively. Append an unsampled handoff frame only if both helper budgets permit it. Otherwise defer that frame through recovery, keep the recorded advisory and activate the exact new candidate on the installed history.|

Moving the frame to the growing transcript tail changes the previous input instead of extending it. Prepending it to all ordinary history also changes the established prefix. Keep mandatory snapshots and the client's prefix validation intact.

The [client](../../../core/src/client.rs) uses `previous_response_id` only when input extends the previous input and output with compatible request settings. A new task or the notes/compact branch can require a full send even when the logical work prefix matches.

## Accounting

Select the optional reference once before admitting ordinary work. Persist its exact bytes and anchor before sampling. Use the aggregate projected frame cost minus the previous request-only credit; neither a frame's identity nor its position can be inferred from a token count. If the frozen prompt no longer fits, compact instead of rewriting an earlier item. Selection is fresh in the installed history. See [budget accounting](budget_accounting.md).

Scope bounds are deliberate: accepting queued input does not revoke the outgoing compaction source; consuming it does. A human clarification that makes the frozen scope inexact also invalidates the old frames, even in an already-steered follow-up with no review ticket. Rollback, fork and installed history replacement discard old anchors. A legacy rollout without selections can project fitting mandatory context but cannot reconstruct a previous review's exact hot prefix. Never silently omit a required sampled frame if its recorded boundary is unavailable.

## Regression entry points

- [WebSocket integration](../../../core/tests/suite/client_websockets.rs): `responses_websocket_completion_review_followup_uses_incremental_create` exercises main work, an independent reviewer and repeated tool rounds; it checks a full first follow-up followed by incremental requests containing only new tool results.
- [HTTP integration](../../../core/tests/suite/completion_review.rs): full input prefixes across live/restarted review, near-budget tool rounds and compaction/model-selection recovery, with exact scope/candidate injection.
- [Session tests](../../../core/src/session/completion_review_tests.rs): advisory, steering without a handoff, encrypted compaction with notes, assistant-only input and empty input.

Scripted request-shape checks establish client behavior, not server cache hits. Use [completed-request comparisons](../responses-output-replay.md#diagnosing-a-drop) and reported usage for hosted observations.
