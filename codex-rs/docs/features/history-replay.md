# Conversation history and replay

Read this when changing prompt projection, rollout reconstruction, fork/resume, rollback, or context metadata. [Work notes](session-work-notes.md), [pause and continue](pause-continue.md), [compaction](remote_compaction_v2/README.md), and [review budget accounting](post_turn_completion_review/budget_accounting.md) cover their specialized contracts.

## Distinct representations

|Representation|Contract|
|---|---|
|Raw history (`ContextManager`)|Admitted response items in conversation order; may contain host-owned ghost snapshots and truncated tool output.|
|Prompt projection|A cloned, normalized, modality-compatible view; request-only frames may be added without changing raw history.|
|Rollout log|Persisted response items plus host metadata/events; enough to reconstruct surviving history and its associated state.|
|`reference_context_item`|Baseline for deciding which canonical context/settings updates to append.|
|`previous_turn_settings`|Model of the latest committed real user boundary, used by previous-model compaction and continuation.|

Do not substitute one representation for another. `record_conversation_items` admits items into live history with truncation but persists the supplied original items; replay reapplies admission/truncation. Completed items enter history; partial stream deltas do not.

## Live history rules

- Record a real user item before committing `previous_turn_settings`. Merely recording a `TurnContext`, changing settings, or running a non-user task must not impersonate committed user work.
- Contextual user/developer messages, including preserved work notes, are excluded by the shared `is_user_turn_boundary` predicate. Use that predicate for rollback, continuation, and replay instead of checking only `role == "user"`.
- Ordinary sampling appends completed response/tool items and accepted steering. Each request is rebuilt from current history, then projected; it is not reconstructed from TUI display text.
- Prompt projection supplies missing tool outputs as `aborted`, removes orphan outputs, replaces unsupported images with placeholders, and excludes ghost snapshots. These changes normally affect a clone, not stored history.
- Local compaction intentionally summarizes detailed assistant/reasoning/tool history. Remote compaction installs a returned replacement. Both preserve required work notes and snapshots and persist exact `replacement_history` for new compactions.
- Compaction can clear the reference-context baseline while retaining previous-turn settings. Re-inject canonical context at the documented replacement boundary, without rewriting older context messages merely to reflect new settings.

## Replay metadata and compaction epochs

`reconstruct_history_from_rollout` processes history and metadata together. A `TurnContext` is a pending candidate until the matching real user boundary commits it. Bare or canceled context records cannot hydrate previous-turn settings. A review's synthetic user item can update its reference context without claiming a new ordinary previous model.

An exact compaction replacement starts a new replay epoch: clear pending context and the reference baseline, preserve previous-turn settings, and use the replacement as the opaque history base. An immediately adjacent context record can establish the replacement's reference baseline for mid-turn continuation; it does not change previous-turn settings.

Legacy compaction records without `replacement_history` are rebuilt from surviving user messages and their summary with an empty initial-context prefix. Injecting resume-time initial context at a historical compaction point changes history and is incorrect.

Rollback uses the same real-user boundaries for history and metadata checkpoints. Context-only tasks do not consume rollback slots. When rollback reaches beyond known checkpoints into an opaque compaction base, clear uncertain metadata instead of attributing it to guessed historical turns. Retain the surviving history prefix according to `ContextManager::drop_last_n_user_turns`.

## Continuation and retained ownership

Ordinary recovery infers unfinished work from a real user boundary followed by incomplete/tool/reasoning output; transient start/complete/pause/continue events are not persisted. Standalone or pre-turn compaction alone must not infer a regular continuation. Mid-turn encrypted compaction can preserve unfinished work even when no visible user item survives.

Pending work-notes capture is recovered before ordinary sampling. Its source model is resolved separately from the destination model chosen for resumed work. Host-owned completion-review records restore accepted input, current request provenance, reviewer/main ownership, and prepared/started handoffs; unsupported or missing provenance must remain unavailable. Never recover those guarantees by searching assistant text for a plausible request.

Token accounting also has host-owned state: preserve the reported usage baseline, its `last_usage_item_count`, reasoning-inclusion semantics, and request-only context credit. Do not reset the covered-history boundary to the replay tail, infer it from the last displayed answer, or infer request-frame credit from prompt text. See the [budget contract](post_turn_completion_review/budget_accounting.md) before changing this ledger.

## Cache consequences

Normal append-only input, stable tools, and appended settings deltas preserve reusable prefixes. Compaction, rollback, image replacement/modality changes, and changed model/instruction/tool/schema properties may invalidate reuse. A `prompt_cache_key` alone does not make differing prefixes equivalent. [WebSocket continuation](responses-websocket.md) additionally requires the exact prior request plus returned response items and never replaces durable local history.

## Implementation and checks

|Concern|Entry point|
|---|---|
|Admission, projection, rollback, usage boundary|[`context_manager/history.rs`](../../core/src/context_manager/history.rs), [`normalize.rs`](../../core/src/context_manager/normalize.rs)|
|Replay epochs and metadata ownership|[`session/rollout_reconstruction.rs`](../../core/src/session/rollout_reconstruction.rs): `ReplayMetadata`, `ReplayEpoch`, `CompactionTailKind`|
|Live context and user commitment|[`session/mod.rs`](../../core/src/session/mod.rs), [`session/turn.rs`](../../core/src/session/turn.rs)|
|Persistence filter and continuation cleanup|[`rollout/policy.rs`](../../core/src/rollout/policy.rs), [`rollout/recorder.rs`](../../core/src/rollout/recorder.rs)|

Existing regression tests in [`session/tests.rs`](../../core/src/session/tests.rs) cover bare context, review context, compaction epochs, rollback across the opaque base, and incomplete-history inference. [`compact_resume_fork.rs`](../../core/tests/suite/compact_resume_fork.rs) checks resumed/forked model history; [`prompt_caching.rs`](../../core/tests/suite/prompt_caching.rs) checks append-oriented requests. Changes involving encrypted-only continuation or review provenance also require the corresponding compaction/review recovery tests, rather than a metadata-only unit test.
