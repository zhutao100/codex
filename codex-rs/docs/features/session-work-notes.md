# Session work notes

Read this when changing compaction preparation or preserving in-progress work. Work notes retain decisions, rejected approaches, and next steps that a general conversation summary can omit. See [streaming compaction](remote_compaction_v2/README.md) for replacement installation and recovery details.

## Capture and preservation

1. Before automatic compaction, request notes through ordinary sampling with a synthetic developer message tagged `<AUTO_COMPACT_WORK_NOTES_REQUEST>`. Capture also applies to pre-turn compaction and supported previous-model downshifts.
2. Keep the normal model/instruction/tool surface, but do not dispatch capture calls. Return the capture rejection for each call; hiding tools would change the request prefix.
3. Reserve output headroom against the literal source-model window. Fit from [work usage](#fit-and-prefix-reuse), not the serialized size of an already sampled prefix. Trim generated tool output newest first only in the temporary capture prompt; do not alter durable history or the capture request.
4. Accept the assistant's output text when it begins, after leading whitespace, with `<AUTO_COMPACT_WORK_NOTES>`. Concatenate multipart output text exactly; do not parse plan tags or trim the stored notes, including text after the closing tag.
5. Remove only the identified capture segment from compaction input. Preserve the previous notes wrapper in place, control/snapshot items and later acknowledged input. The summarizer sees the same work as capture, not the newly generated checkpoint.
6. Append the selected notes verbatim in a contextual user message after compaction. Preserve them in the exact replacement history used for replay.

Capture budgets two completed responses, not two assistant messages. Commentary and final text in the first response do not disqualify valid notes in the second. Journal completions even with only reasoning, rejected calls or no token usage. An interrupted stream can retry without spending a completed attempt, but a tagged output item already recorded goes directly to compaction. Close capture before sending compact. Older unjournaled captures use a conservative message-count estimate. A final JSON output schema skips free-form capture. Failed/empty capture can use previously preserved notes; cancellation stops the operation. Standalone `/compact` preserves available notes without starting a capture. If an automatic checkpoint is pending, `/compact` finishes its remaining helpers but leaves the first ordinary request to `/continue`.

## Fit and prefix reuse

Separate the decision to compact from the decision to shorten a helper request. For an attributable work receipt, start from the reported total, add uncovered items once and subtract the previous request's frame credit. Preserve encrypted reasoning without reserving it twice. A newer, unmeasured real user boundary retains the capacity reserve.

The model slug, provider key **and effective API endpoint** own a measurement. The completed stream supplies the [endpoint fingerprint](../../core/src/model_provider_info.rs). Secure versus plain transport, host, nondefault port and deployment path matter; a WebSocket upgrade or equivalent trailing slash does not. The checkpoint contains no URL, query, credential or header values. This identifies a client route, not an upstream machine behind a proxy.

Freeze the correction with its owner, not a previously fitted input. The [sender](../../core/src/client.rs) fits notes and V2 from the unshortened projection **after resolving auth for that attempt**, including after a rejected WebSocket upgrade or authentication recovery. If ownership is missing, the URL is ambiguous or the selected route differs, size that send locally. Recovery on a different route does not erase the qualified correction: a later send back on its original model, provider and endpoint can use it again. No warm-up request, login pinning or retry of a successful request is needed.

Calibrate and fit the representation the source will send. A text-only source measures image placeholders; freeform `apply_patch` formats paired shell and patch results instead of sending their stored JSON envelopes. Price surviving covered items in both the measured and current formats and apply the **full signed difference**. Separately, if continuation deleted sampled calls, cap that credit at the receipt's output tokens. Charge each uncovered result in the new format just once. Fitting does not edit the raw rollout or billed usage.

The work receipt already includes its advertised tools and any `text.format`. Subtract those measured costs before adding the surface actually sent. V2 has tools but no response format; ordinary admission and replacement reserve the destination format. Price the serialized format including its name and strict wrapper, not just the schema body. This is a local estimate, not a provider token breakdown. A legacy receipt without a format cost earns no format credit. Coverage, frame credit and reasoning inclusion without a measured model, endpoint and tool cost do not establish a usable baseline: an upgraded session fits locally and can still trim where a newly recorded session would not.

Notes, V2 and local summarization receipts contribute to lifetime usage but do not measure another ordinary request. Record the helper's requested model, provider key and authenticated endpoint fingerprint with its bill; preserve the work receipt, coverage, route, frame and format costs separately until a new ordinary measurement or an installed replacement. A helper's context-limit rejection without usage contributes no bill and cannot mark ordinary work full. A paid but invalid or oversized V2 summary cannot force `/compact` to trim fitting work on retry, including after restart. Old records that did not distinguish a helper from work cannot in general supply that missing ownership.

Freeze the work cost and corresponding estimate before the marker. Neither a fitted notes answer nor its usage measures the original work. On retry or restart, refit the entire capture with its frames and output reserve. A retry may need to shorten an older result in its temporary input; V2 independently fits the original projected work, including its trigger. The compatibility unary endpoint keeps its own budget.

For fitting work, let `W` be the preceding ordinary input plus newly produced work, tool results and admitted handoff context. Capture sends `W + notes request`; streaming compaction sends `W + compaction_trigger`. Both retain the previous verbatim notes wrapper and every [review frame](post_turn_completion_review/request_prefix.md) at its position in that work, even across a new review that cannot sample before compacting. Neither the marker nor a retry moves a frame. Earlier contracts and notes are evidence for the checkpoint, not instructions to give another user-facing answer. Only the selected notes survive installation as a verbatim wrapper.

The equality is for the logical work prefix, not the transport: after notes, the trigger branches from `W`, not the notes response. A full send can still reuse cached work. Real overflow, different request settings or a changed source can reduce reuse; only completed server usage measures cache hits.

Replay raw function and custom-tool results with the admission limit recorded for that call, not the resume model's current limit. The raw rollout result remains available unchanged. Older results without a per-call policy use the recorded turn context where available; a legacy continuation without a recorded policy cannot prove byte equality across a model switch.

## Invariants

- Pending steering waits during notes capture and through the required first post-compaction model continuation. This keeps the capture about the work being compacted and prevents steering from replacing an unfinished tool/model continuation.
- `prepare_history_for_compaction` selects explicit fresh notes, then recovered capture notes, then the latest existing preserved notes. Do not deduplicate the source. Exclude superseded wrappers and the request-only review frame from the installed replacement; append the selected wrapper exactly once, even if a unary response echoed its input.
- Preserved notes are contextual state, so they do not create an ordinary user-turn boundary, count as a fresh request, or independently imply unfinished work.
- A hosted search is service output: preserve it on a retry and leave it out of compact. Tagged commentary can precede local calls in the same response. Journal each such call's capture position and content digest before recording it; complete only the matching rejection, without execution, even after restart. A tag does not authorize discarding later work.
- A later real user boundary bounds the capture segment. Acknowledged input beyond it must survive cleanup, finalization, and replay.
- Before any new sampling, a checkpoint belongs to the last completed ordinary request. This includes a fresh task, a new review and a plain `/continue` from an over-limit tool result written before the compaction decision. Restore the sampled personality, directory, tier, exact [recorded base](history-replay.md#replay-metadata-and-compaction-epochs) and effective reasoning effort, including an absent effort. A catalog refresh must not resolve the outgoing default a second time. The destination owns the replacement and subsequent work; a fresh structured destination skips a new free-form capture but does not cancel an already-selected one. Older records cannot restore settings they never recorded.
- Notes remain exact across local, unary remote, and streaming remote compaction. Legacy worktree snapshots are not model input. Do not truncate mandatory retained items to make a replacement fit; use the streaming compaction budget/failure contract.
- Local summarization output stays private until that attempt succeeds. A failed summary must not become a completed notes capture in live history or replay.

## Implementation and checks

|Concern|Entry point|
|---|---|
|Capture scheduling, output reserve, source-model recovery|[`session/turn.rs`](../../core/src/session/turn.rs): `run_pre_turn_compact`, `sampling_input_for_pre_compact_work_notes`, `work_notes_request_context`|
|Segment scanner and source preparation|[`compact.rs`](../../core/src/compact.rs): `pending_work_notes`, `prepare_history_for_compaction`, `preserved_work_notes_message`|
|Contextual wrapper recognition|[`preserved_work_notes.rs`](../../core/src/preserved_work_notes.rs), [`event_mapping.rs`](../../core/src/event_mapping.rs)|
|Capture rejection and retry|[`session/turn.rs`](../../core/src/session/turn.rs): `record_missing_capture_rejections`; compare whole retry bodies in the [recorder-cut tests](../../core/tests/suite/compact_remote_v2_capture_phase.rs).|
|Replacement and replay|[Streaming compaction design](remote_compaction_v2/design.md), [history replay](history-replay.md)|

Existing checks include scanner tests in `compact.rs`, headroom tests in `session/turn.rs`, [`compact_local_recovery.rs`](../../core/tests/suite/compact_local_recovery.rs), [`compact_remote_v2_recovery.rs`](../../core/tests/suite/compact_remote_v2_recovery.rs), [`compact_remote_v2_downshift.rs`](../../core/tests/suite/compact_remote_v2_downshift.rs), and the compaction/steering ordering tests in [`pending_input.rs`](../../core/tests/suite/pending_input.rs). Test exact note bytes and actual replay, including cancellation after notes and before replacement installation.
