# Session work notes

Read this when changing compaction preparation or preserving in-progress work. Work notes retain decisions, rejected approaches, and next steps that a general conversation summary can omit. See [streaming compaction](remote_compaction_v2/README.md) for replacement installation and recovery details.

## Capture and preservation

1. Before automatic compaction, request notes through ordinary sampling with a synthetic developer message tagged `<AUTO_COMPACT_WORK_NOTES_REQUEST>`. Capture also applies to pre-turn compaction and supported previous-model downshifts.
2. Keep the normal model/instruction/tool surface. Reject tool execution at runtime with `ToolCallExecutionMode::RejectAll`; hiding tools would change the request prefix and model behavior.
3. Reserve output headroom against the literal source-model window. Fit from [work usage](#fit-and-prefix-reuse), not the serialized size of an already sampled prefix. Trim generated tool output newest first only in the temporary capture prompt; do not alter durable history or the capture request.
4. Accept the assistant's output text when it begins, after leading whitespace, with `<AUTO_COMPACT_WORK_NOTES>`. Concatenate multipart output text exactly; do not parse plan tags or trim the stored notes, including text after the closing tag.
5. Remove only the identified capture segment from compaction input. Preserve the previous notes wrapper in place, control/snapshot items and later acknowledged input. The summarizer sees the same work as capture, not the newly generated checkpoint.
6. Append the selected notes verbatim in a contextual user message after compaction. Preserve them in the exact replacement history used for replay.

Capture budgets two completed responses. Journal each completion even if it has only reasoning, rejected calls or no token usage; output items are not attempts. Close capture before sending the compaction request. A restart can retry an unacknowledged in-flight request, but does not reopen spent attempts. A final JSON output schema skips free-form capture. Failed/empty capture can use previously preserved notes; cancellation stops the operation. Manual compaction preserves existing or recoverable notes without requiring a new capture.

## Fit and prefix reuse

Separate the decision to compact from the decision to shorten a helper request. The capacity ledger may reserve older encrypted reasoning when the endpoint does not report it as included. If the positive input measurement names the helper's model and covers the current user boundary, fit both helper prefixes to `reported total + uncovered items − prior frame credit`. Preserve the ciphertext without charging that reserve again. A newer unmeasured user boundary retains the capacity reserve. Missing or different measurement ownership requires a local estimate: a legacy last-user-model hint can select a route but cannot authorize a discount. Genuinely oversized tool results can still require trimming.

Freeze the work cost and corresponding transcript estimate before recording the capture marker. Rollout replay reconstructs the same pair at that marker; neither a fitted notes answer nor its usage measures the original work. Apply this calibration on retry and restart. Count projected frames once and keep the previously installed notes wrapper as ordinary history. The compatibility unary endpoint keeps its own fit.

For fitting work, let `W` be the preceding ordinary input plus newly produced work, tool results and admitted handoff context. Capture sends `W + notes request`; streaming compaction sends `W + compaction_trigger`. Both retain the previous verbatim notes wrapper and every [review frame](post_turn_completion_review/request_prefix.md) at its position in that work, even across a new review that cannot sample before compacting. Neither the marker nor a retry moves a frame. Earlier contracts and notes are evidence for the checkpoint, not instructions to give another user-facing answer. Only the selected notes survive installation as a verbatim wrapper.

The equality is for the logical work prefix, not the transport: after notes, the trigger branches from `W`, not the notes response. A full send can still reuse cached work. Real overflow, different request settings or a changed source can reduce reuse; only completed server usage measures cache hits.

Replay raw function and custom-tool results with the admission limit recorded for that call, not the resume model's current limit. The raw rollout result remains available unchanged. Older results without a per-call policy use the recorded turn context where available; a legacy continuation without a recorded policy cannot prove byte equality across a model switch.

## Invariants

- Pending steering waits during notes capture and through the required first post-compaction model continuation. This keeps the capture about the work being compacted and prevents steering from replacing an unfinished tool/model continuation.
- `prepare_history_for_compaction` selects explicit fresh notes, then recovered capture notes, then the latest existing preserved notes. Do not deduplicate the source. Exclude superseded wrappers and the request-only review frame from the installed replacement; append the selected wrapper exactly once, even if a unary response echoed its input.
- Preserved notes are contextual state, so they do not create an ordinary user-turn boundary, count as a fresh request, or independently imply unfinished work.
- The scanner accepts trailing reasoning, whitespace-only assistant output text, and exact rejection outputs for calls made during capture. It rejects unrelated work or mismatched tool activity instead of deleting an uncertain suffix.
- A later real user boundary bounds the capture segment. Acknowledged input beyond it must survive cleanup, finalization, and replay.
- Notes recovery runs before ordinary sampling and new input installation. The first durable review deferral freezes the outgoing task settings, even before the notes marker. Retargeting the destination does not change the source model, effort, tier or tool policy used by the helpers. The destination owns the replacement and resumed work. Older model-only records cannot restore settings or completed attempts they never recorded.
- Notes remain exact across local, unary remote, and streaming remote compaction. Legacy worktree snapshots are not model input. Do not truncate mandatory retained items to make a replacement fit; use the streaming compaction budget/failure contract.
- Local summarization output stays private until that attempt succeeds. A failed summary must not become a completed notes capture in live history or replay.

## Implementation and checks

|Concern|Entry point|
|---|---|
|Capture scheduling, output reserve, source-model recovery|[`session/turn.rs`](../../core/src/session/turn.rs): `run_pre_turn_compact`, `sampling_input_for_pre_compact_work_notes`, `work_notes_request_context`|
|Segment scanner and source preparation|[`compact.rs`](../../core/src/compact.rs): `pending_work_notes`, `prepare_history_for_compaction`, `preserved_work_notes_message`|
|Contextual wrapper recognition|[`preserved_work_notes.rs`](../../core/src/preserved_work_notes.rs), [`event_mapping.rs`](../../core/src/event_mapping.rs)|
|Execution rejection|[`tools/parallel.rs`](../../core/src/tools/parallel.rs): `ToolCallExecutionMode`|
|Replacement and replay|[Streaming compaction design](remote_compaction_v2/design.md), [history replay](history-replay.md)|

Existing checks include scanner tests in `compact.rs`, headroom tests in `session/turn.rs`, [`compact_local_recovery.rs`](../../core/tests/suite/compact_local_recovery.rs), [`compact_remote_v2_recovery.rs`](../../core/tests/suite/compact_remote_v2_recovery.rs), [`compact_remote_v2_downshift.rs`](../../core/tests/suite/compact_remote_v2_downshift.rs), and the compaction/steering ordering tests in [`pending_input.rs`](../../core/tests/suite/pending_input.rs). Test exact note bytes and actual replay, including cancellation after notes and before replacement installation.
