# Session work notes

Read this when changing compaction preparation or preserving in-progress work. Work notes retain decisions, rejected approaches, and next steps that a general conversation summary can omit. See [streaming compaction](remote_compaction_v2/README.md) for replacement installation and recovery details.

## Capture and preservation

1. Before automatic compaction, request notes through ordinary sampling with a synthetic developer message tagged `<AUTO_COMPACT_WORK_NOTES_REQUEST>`. Capture also applies to pre-turn compaction and supported previous-model downshifts.
2. Keep the normal model/instruction/tool surface. Reject tool execution at runtime with `ToolCallExecutionMode::RejectAll`; hiding tools would change the request prefix and model behavior.
3. Reserve output headroom by trimming generated tool output only in a temporary capture prompt. Do not alter durable source history or the capture request to fit that prompt.
4. Accept the assistant's output text when it begins, after leading whitespace, with `<AUTO_COMPACT_WORK_NOTES>`. Concatenate multipart output text exactly; do not parse plan tags or trim the stored notes, including text after the closing tag.
5. Remove only the identified capture segment from compaction input, preserve control/snapshot items and later acknowledged input, and compact the remaining work history.
6. Append the selected notes verbatim in a contextual user message after compaction. Preserve them in the exact replacement history used for replay.

Capture makes at most two attempts. A final JSON output schema skips a new free-form notes capture. Failed/empty capture can compact using previously preserved notes; cancellation stops the operation instead of silently completing it. Manual compaction preserves existing or recoverable notes through the common preparation logic; it does not always initiate a new notes round.

## Invariants

- Pending steering waits during notes capture and through the required first post-compaction model continuation. This keeps the capture about the work being compacted and prevents steering from replacing an unfinished tool/model continuation.
- `prepare_history_for_compaction` selects explicit fresh notes, then recovered capture notes, then the latest existing preserved notes. Older preserved-note wrappers are removed from the compaction source to avoid summarizing and accumulating copies.
- Preserved notes are contextual state, so they do not create an ordinary user-turn boundary, count as a fresh request, or independently imply unfinished work.
- The scanner accepts trailing reasoning, whitespace-only assistant output text, and exact rejection outputs for calls made during capture. It rejects unrelated work or mismatched tool activity instead of deleting an uncertain suffix.
- A later real user boundary bounds the capture segment. Acknowledged input beyond it must survive cleanup, finalization, and replay.
- Notes recovery runs before ordinary sampling and new input installation. The recorded source model/provider/instructions belong to capture; the destination context belongs to the replacement and resumed work. A pending model downshift cannot be inferred from token usage alone.
- Notes and ghost snapshots remain exact across local, unary remote, and streaming remote compaction. Do not truncate mandatory retained items to make a replacement fit; use the streaming compaction budget/failure contract.
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
