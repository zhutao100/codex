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
|Notes / streaming compact|The same outgoing work including sampled frames and any prior verbatim notes wrapper, then the capture marker / compaction trigger respectively. Only when the [destination needs a recoverable compaction](budget_accounting.md#recovering-from-overflow), check whether incoming advice or an unsampled frame would displace otherwise fitting work in either helper. Leave that addition in the durable handoff; admit it exactly once on the installed history.|

Moving the frame to the growing transcript tail changes the previous input instead of extending it. Prepending it to all ordinary history also changes the established prefix. Keep mandatory snapshots and the client's prefix validation intact.

To diagnose a handoff, compare the preceding work including its output and each existing frame's bytes and anchor. A larger frame-token total can mean that a new frame was appended after an unchanged prefix; it does not prove replacement or explain a low cache receipt.

A checkpoint can precede advisory admission: reconciling an already delivered job can find the owner over budget without appending anything. Select the outgoing ordinary settings before the review guard, even on the same model: a changed personality, base or tier belongs to the incoming follow-up, not its helpers. Do not insert a new candidate at the last older developer message. Keep the outgoing frames and defer the candidate unless its own admitted advisory is the tail of the outgoing work.

The [client](../../../core/src/client.rs) uses `previous_response_id` only when input extends the previous input and output with compatible request settings. A new task or the notes/compact branch can require a full send even when the logical work prefix matches.

Compare ordered tools as well as input. A locally refused follow-up cannot supply [V2's tools](../remote_compaction_v2/design.md), even on the same model and connection. Ordinary work, continuations and recovered helpers resolve the same skill-name conflicts against available connectors. Rollouts retain typed app selections with consumed input because the Responses user message omits typed mentions.

## Accounting

Select the optional reference at first dispatch. The ordinary request proposes its frame and any unrecorded advice. If the [authenticated send](../../../core/src/client.rs) cannot fit the proposed full frame but can fit its mandatory version, replace only that unsent frame at its projected index, journal the chosen bytes, and send once. Use the dispatched frame's token credit in the receipt. A retry cannot downgrade a previously dispatched full frame or restore a previously omitted reference, even without positive usage.

If the actual send refuses both versions, check checkpoint feasibility before recording a deferral. Keep all outgoing frames and results in place. Only the last completed ordinary request proves which route owns that prefix, even when it returned no usage. On that route, a fitting advisory can extend the prefix; include its mandatory candidate frame only if it also fits both helpers. If login can change between helpers, budget additions against both local and attributable measured work. An older size receipt or current login cannot establish placement. On a different or unknown route, keep the unadmitted handoff pending. If steering supersedes an unsent candidate, rebuild without its advice; preserve an already-owed first response before consuming later input.

Use the aggregate projected frame cost minus the previous request-only credit. When reconstructing an older writer that replaced frames, add the other same-revision frames that the current writer would retain in that epoch; do not add the last frame twice or count optional-reference flaps as separate reviews. Several handoffs can move compaction earlier. Exact prefix equality at a fitting checkpoint does not prove the same prefix fits at an older boundary. See [budget accounting](budget_accounting.md).

Scope bounds are deliberate: accepting queued input does not revoke the outgoing compaction source; consuming a new user revision does. Its first ordinary request can diverge at a former review frame and have zero frame credit. Do not keep stale instructions for a cache hit or attribute the provider's exact cached-token count to their removal. A clarification that makes the scope inexact also invalidates old frames, even without a review ticket. Rollback, fork and installed history replacement discard old anchors. Legacy selections cannot always reconstruct an exact hot prefix; never silently omit a required sampled frame whose boundary is unavailable.

## Regression entry points

- [WebSocket integration](../../../core/tests/suite/client_websockets.rs): `responses_websocket_completion_review_followup_uses_incremental_create` exercises main work, an independent reviewer and repeated tool rounds; it checks a full first follow-up followed by incremental requests containing only new tool results.
- [HTTP integration](../../../core/tests/suite/completion_review.rs): full input prefixes across budget crossings and oversized advisories/candidates; ordered tools across review, notes and recovered compaction with a skill/connector collision and an explicit app selection.
- [Session tests](../../../core/src/session/completion_review_tests.rs): advisory, steering without a handoff, encrypted compaction with notes, assistant-only input and empty input.

Scripted request-shape checks establish client behavior, not server cache hits. Use [completed-request comparisons](../responses-output-replay.md#diagnosing-a-drop) and reported usage for hosted observations.
