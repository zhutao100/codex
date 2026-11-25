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
|Notes / streaming compact|The same outgoing work including sampled frames and any prior verbatim notes wrapper, then the capture marker / compaction trigger respectively. Check the incoming advisory before adding it to history, and the new frame before projecting it. If either addition would displace fitting work in either helper, leave that addition in the durable handoff. Admit the advice exactly once and activate the candidate on the installed history.|

Moving the frame to the growing transcript tail changes the previous input instead of extending it. Prepending it to all ordinary history also changes the established prefix. Keep mandatory snapshots and the client's prefix validation intact.

To diagnose a handoff, compare the preceding work including its output and each existing frame's bytes and anchor. A larger frame-token total can mean that a new frame was appended after an unchanged prefix; it does not prove replacement or explain a low cache receipt.

A checkpoint can precede advisory admission: reconciling an already delivered job can find the owner over budget without appending anything. Select the outgoing ordinary settings before the review guard, even on the same model: a changed personality, base or tier belongs to the incoming follow-up, not its helpers. Do not insert a new candidate at the last older developer message. Keep the outgoing frames and defer the candidate unless its own admitted advisory is the tail of the outgoing work.

The [client](../../../core/src/client.rs) uses `previous_response_id` only when input extends the previous input and output with compatible request settings. A new task or the notes/compact branch can require a full send even when the logical work prefix matches.

Compare ordered tools as well as input. Ordinary work, continuations and recovered helpers resolve the same skill-name conflicts against the available connectors. New rollouts retain typed app selections with consumed input; the Responses user message alone cannot reconstruct them because it omits typed mention items.

## Accounting

Select the optional reference once before admitting ordinary work. Persist its exact bytes and anchor before sampling. Use the aggregate projected frame cost minus the previous request-only credit; neither a frame's identity nor its position can be inferred from a token count. If the frozen prompt no longer fits, compact instead of rewriting an earlier item. Selection is fresh in the installed history. See [budget accounting](budget_accounting.md).

Scope bounds are deliberate: accepting queued input does not revoke the outgoing compaction source; consuming a new user revision does. Its first ordinary request can diverge at a former review frame and have zero frame credit. Do not keep stale instructions for a cache hit or attribute the provider's exact cached-token count to their removal. A clarification that makes the scope inexact also invalidates old frames, even without a review ticket. Rollback, fork and installed history replacement discard old anchors. Legacy selections cannot always reconstruct an exact hot prefix; never silently omit a required sampled frame whose boundary is unavailable.

## Regression entry points

- [WebSocket integration](../../../core/tests/suite/client_websockets.rs): `responses_websocket_completion_review_followup_uses_incremental_create` exercises main work, an independent reviewer and repeated tool rounds; it checks a full first follow-up followed by incremental requests containing only new tool results.
- [HTTP integration](../../../core/tests/suite/completion_review.rs): full input prefixes across budget crossings and oversized advisories/candidates; ordered tools across review, notes and recovered compaction with a skill/connector collision and an explicit app selection.
- [Session tests](../../../core/src/session/completion_review_tests.rs): advisory, steering without a handoff, encrypted compaction with notes, assistant-only input and empty input.

Scripted request-shape checks establish client behavior, not server cache hits. Use [completed-request comparisons](../responses-output-replay.md#diagnosing-a-drop) and reported usage for hosted observations.
