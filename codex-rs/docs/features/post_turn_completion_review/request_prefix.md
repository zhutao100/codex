# Completion-review request prefix

Read this when changing request-only review framing or its interaction with Responses WebSockets.

## Placement contract

The advisory is ordinary developer history. The exact request/candidate frame and replacement-answer contract are request-only; they are not appended transcript or rollout items.

[Sampling](../../../core/src/session/turn.rs), in `insert_completion_review_instructions`, inserts that frame after the last system/developer/user message or encrypted compaction item, before subsequent assistant reasoning and tool activity. With no such boundary it inserts at index zero. Derive placement from prepared history so steering, replay and compaction need no extra persisted anchor.

|Request|Logical input|
|---|---|
|First follow-up|History, advisory, fixed review frame.|
|Next tool round|The preceding input, preceding model output, new tool results.|

Moving the frame to the growing transcript tail changes the previous input instead of extending it. Prepending it to all ordinary history also changes the established prefix. Keep mandatory snapshots and the client's prefix validation intact.

The [client](../../../core/src/client.rs) uses `previous_response_id` only when current input equals previous input plus previous response output plus the new suffix, with compatible request settings. Physical socket reuse alone does not establish logical continuation. Ordinary tool rounds should preserve that equality; new scope, advisory, compaction, model/tool changes or optional-reference omission may legitimately require a full request.

## Accounting

The frame's tokens can already be present in reported usage despite being absent from stored history. Charge its replacement delta and count only real history items in the covered-history boundary. Placement must not alter this [budget contract](budget_accounting.md).

## Regression entry points

- [WebSocket integration](../../../core/tests/suite/client_websockets.rs): `responses_websocket_completion_review_followup_uses_incremental_create` exercises main work, an independent reviewer and repeated tool rounds; it checks a full first follow-up followed by incremental requests containing only new tool results.
- [HTTP integration](../../../core/tests/suite/completion_review.rs): full input prefixes across live/restarted review, near-budget tool rounds and compaction/model-selection recovery, with exact scope/candidate injection.
- [Session tests](../../../core/src/session/completion_review_tests.rs): advisory, steering without a handoff, encrypted compaction with notes, assistant-only input and empty input.

Scripted request-shape checks establish client chaining. They do not measure server cache hits. Assess cache behavior from actual chaining and server usage; synthetic usage counters or socket reuse are insufficient evidence.
