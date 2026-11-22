# Completion-review request prefix

Read this when changing request-only review framing or its interaction with Responses WebSockets.

## Placement contract

The advisory is ordinary developer history. The exact request/candidate frame and replacement-answer contract are request-only; they are not appended transcript or rollout items.

[Ordinary sampling](../../../core/src/session/turn.rs), notes capture and compaction use the [same frame placement](../../../core/src/completion_review.rs): after the last system/developer/user message or encrypted compaction item in the work being projected, before subsequent assistant reasoning and tool activity. With no such boundary insert at index zero. The synthetic capture segment is not work, even on retries or when later acknowledged input survives recovery.

|Request|Logical input|
|---|---|
|First follow-up|History, advisory, fixed review frame.|
|Next tool round|The preceding input, preceding model output, new tool results.|
|Notes / streaming compact|The work input including the selected frame and previous notes wrapper, followed by the capture marker / compaction trigger, respectively.|

Moving the frame to the growing transcript tail changes the previous input instead of extending it. Prepending it to all ordinary history also changes the established prefix. Keep mandatory snapshots and the client's prefix validation intact.

The [client](../../../core/src/client.rs) uses `previous_response_id` only when current input equals previous input plus previous response output plus the new suffix, with compatible request settings. Physical socket reuse alone does not establish logical continuation. Ordinary tool rounds should preserve that equality; new scope, advisory, compaction, model/tool changes or optional-reference omission may legitimately require a full request.

## Accounting

The frame's tokens can already be present in reported usage despite being absent from stored history. Charge its replacement delta and count only real history items in the covered-history boundary. Keep the sampled reference choice across live notes and compaction, even if the next ordinary request would have dropped its optional reference. On a fresh task or restart, reconstruct from active review state and the persisted frame credit: use the full frame when the credit identifies it, otherwise the mandatory form. No active replacement review means no frame. See the [budget contract](budget_accounting.md).

## Regression entry points

- [WebSocket integration](../../../core/tests/suite/client_websockets.rs): `responses_websocket_completion_review_followup_uses_incremental_create` exercises main work, an independent reviewer and repeated tool rounds; it checks a full first follow-up followed by incremental requests containing only new tool results.
- [HTTP integration](../../../core/tests/suite/completion_review.rs): full input prefixes across live/restarted review, near-budget tool rounds and compaction/model-selection recovery, with exact scope/candidate injection.
- [Session tests](../../../core/src/session/completion_review_tests.rs): advisory, steering without a handoff, encrypted compaction with notes, assistant-only input and empty input.

Scripted request-shape checks establish client chaining. They do not measure server cache hits. Assess cache behavior from actual chaining and server usage; synthetic usage counters or socket reuse are insufficient evidence.
