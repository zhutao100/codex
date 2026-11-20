# Plan: recoverable tool output

Status: proposed. No output-artifact store, retrieval contract, or corresponding runtime setting is implemented. Implement this before reducing previews or relying on narrower exploration reports.

## Problem and scope

Unified execution keeps a bounded head/tail buffer before rendering its preview; legacy shell capture keeps a bounded prefix. Output can also be lost before that buffer: the unified process receiver skips lagged broadcast messages. Persisting the rendered response, replaying the rollout, or rerunning the command cannot recover omitted original bytes reliably.

Read the [process receiver](../../../core/src/unified_exec/process.rs), [head/tail buffer](../../../core/src/unified_exec/head_tail_buffer.rs), [pipe reader](../../../utils/pty/src/pipe.rs), [PTY reader](../../../utils/pty/src/pty.rs), and [legacy capture](../../../core/src/exec.rs) before choosing an insertion point. Capture capacity and model preview budget are separate limits. The current per-wait collector also accumulates repeated buffer drains into a `Vec<u8>` without an equivalent bound; the [background completion plan](background_completion.md) requires bounded collection before extending wait durations. Neither a bigger preview nor durable storage removes that memory requirement.

The first implementation covers local unified execution and legacy shell, including unsuccessful commands. A process owns one artifact across its initial call, later polls, and completion. Extending the same storage interface to other tool families is subsequent work, not a reason to redesign their schemas now. Automatic completion delivery is a separate scheduling concern; finalizing an artifact does not itself start model inference.

## Required contract

1. Capture reader-observed bytes before the first destructive cap or lossy fanout. Keep existing bounded preview memory and command execution semantics.
2. Provide a stable artifact identity and a model-readable location for retained bytes, with truthful capture and availability states. Retrieval must not depend on a live process handle or rerun the command.
3. Keep command outcome, capture completeness, preview truncation, and artifact availability distinct. A failed command can have complete output; a successful command can have incomplete output.
4. Preserve finalized bytes and previously recorded model results. Raw artifact content remains outside prompt history and rollout response items until a bounded retrieval requests it.
5. Bound per-artifact bytes, session storage, queued payload bytes plus in-flight writer buffers, per-wait collection memory, and finalization time. A queue item count alone is not a byte bound unless the maximum item allocation is enforced. On exhaustion or I/O failure, continue draining the child and report the degraded artifact state.

## Capture and finalization

Use one session-owned storage service with execution-owned artifact identifiers. Unified execution must tap the pipe/PTY byte readers before broadcast fanout; adding a subscriber after fanout is insufficient. Legacy `read_capped` must send observed bytes to the same service before `append_capped` discards them. Keep the bounded in-memory head/tail preview as a separate consumer.

Spool from the first observed byte in the initial implementation. An optimization that stages small output in memory is acceptable later only if it flushes all staged bytes before any loss and preserves the same failure contract. Do not copy an already-capped transcript and label it complete.

Preserve PTY output as the merged bytes observed by the master reader, after any terminal-driver transformations; do not claim identity with the child's original writes. Preserve stdout/stderr identity for separate pipes where available; do not infer a total ordering across independent OS streams. Store reader-observed bytes before the harness applies display decoding, newline normalization, or terminal escape handling. Retrieval can present a decoded view, but invalid UTF-8 must not destroy the stored source bytes.

For the initial implementation, one process artifact has a shared byte quota and separate per-stream observed/committed frontiers. A range reference names its stream; concatenated stdout/stderr offsets are not a valid global timeline. The first storage gap seals retention for the artifact, including both pipe streams, after acknowledging the earlier queued prefix. Continue counting each stream independently. This policy bounds metadata and avoids fictional continuity; per-stream reserved capacity or fairness tuning is a later measured change, not an implied guarantee.

The capture sink must not introduce an unbounded queue or block pipe draining indefinitely. Enforce the byte budget before enqueueing and include the writer's in-flight allocation until released. If the bounded queue fills or the writer fails, stop retention for that artifact, keep the successfully committed contiguous prefix, mark the reason, and continue draining/counting output. Do not silently resume appending after a dropped chunk: that would make byte offsets appear contiguous when they are not. A richer segmented representation can be considered only if a measured use case requires it. A dedicated writer thread or database is not a prerequisite; choose the simplest ownership model that satisfies these limits.

Publish a completed capture only after all relevant readers reach EOF, bounded output draining finishes, and the writer acknowledges the retained prefix. A process exit event alone is insufficient because trailing output may still be in transit. Keep reader shutdown separate from the token that stops the child: after timeout or cancellation, drain remaining output within a bounded post-exit grace period. A cancelled command can still have complete capture. Descendants may keep descriptors open after the direct child exits; if the drain deadline or reader error prevents EOF, record that cutoff and an unknown remainder rather than waiting indefinitely or declaring completion.

Handle partial writes explicitly. Preserve the successfully acknowledged prefix across retry or cancellation; do not restart a partially completed append from its beginning or assume that a failed/cancelled write retained no bytes. Tokio's [`write_all` cancellation contract](https://docs.rs/tokio/latest/tokio/io/trait.AsyncWriteExt.html#method.write_all) makes this distinction concrete. Keep unacknowledged trailing bytes outside the advertised committed range until reconciled; do not discard known-written data through an automatic truncation policy merely because catalog publication lagged.

## State and accounting

These are semantic states, not a committed wire-format or new public configuration schema.

|Capture state|Meaning|Retrieval behavior|
|---|---|---|
|Writing|The process or output readers are active; a committed prefix is available|Read only the reported committed range; a later read may expose a longer prefix|
|Complete|All observed streams reached EOF and all their bytes were finalized without retention loss|Read the finalized immutable bytes|
|Capped|An explicit artifact or session retention limit stopped storage|Read the retained prefix; report the limit and known omitted bytes|
|Failed|Allocation, writer, reader, queue, or finalization failure prevented complete capture|Return any committed prefix and the failure reason; never claim full output|
|Interrupted|Capture ended without establishing stream completion, including an unresolved running artifact found after a crash|Return the committed prefix and explicitly unknown remainder|

Availability is independent: an artifact can later be available, expired, missing, or inaccessible under the effective execution policy. Preserve its last known capture metadata even when bytes are unavailable. Never recreate an empty file for an old locator.

Record stream termination, reader-observed byte count, committed prefix length, and acknowledged durability separately. A single `complete` flag must not stand in for all four. State whether the observed count is final or still growing, and whether an unobserved remainder is unknown. Token estimates must remain labeled as estimates of the counted material; the existing post-capture `original_token_count` is not evidence of every byte the process emitted.

The initial recovery target is a CLI process restart while the OS/filesystem remains available. A committed prefix requires completed writes and applicable userspace/async flushes, followed by acknowledged catalog publication; queue acceptance alone is insufficient. Preserve that ordering before a rollout checkpoint promises recovery. The current [rollout writer](../../../core/src/rollout/recorder.rs) acknowledges `flush`, which must not be relabeled as an `fsync` guarantee.

Power-loss durability is a stronger, separate contract. A successful write or [`tokio::fs::File::flush`](https://docs.rs/tokio/latest/tokio/fs/struct.File.html) does not establish it. Claim synchronized durability only after the implementation's required data, catalog, and filesystem-metadata synchronization has been acknowledged, including directory entries where relevant. Track synchronization failure separately from complete reader capture; do not let `complete` imply power-loss recovery. Choose and document the release's synchronization policy explicitly rather than adding a new durability tier by implication.

Nonzero exit, signal, timeout, cancellation, sandbox denial, and pre-spawn failure retain their existing execution meaning. Finalization runs on all paths that acquired output. A command that never started has no fabricated output artifact; its ordinary error remains visible. A storage error must not replace an otherwise valid command result.

## Model result and bounded retrieval

Results that omit output or refer to a still-active process contain their ordinary bounded preview plus a compact artifact reference, the state at that result's creation, the committed byte range, and any omission reason. Keep the same artifact identity across the initial call and subsequent polls; distinguish newly displayed output from total captured output. Final metadata belongs to the final result or host record, not a retroactive edit of earlier results.

Budget the complete model result once. Reserve space for execution status, artifact location/state, omission markers, and required framing before choosing preview bytes. The [unified result formatter](../../../core/src/tools/handlers/unified_exec.rs) and [history output processing](../../../core/src/context_manager/history.rs) must agree on that allowance, so a later truncation layer cannot silently remove the recovery reference. Very small requested preview budgets can omit the body; an overall configured result budget that cannot hold the mandatory envelope must produce an explicit configuration failure rather than a misleading partial envelope.

Small successful output keeps its current useful rendering; extra artifact details are needed only when output is omitted, a process remains active, or recovery status matters. Do not automatically add the entire stored output on a later turn.

Initial retrieval must provide exact bounded byte ranges through an actual model tool and the effective sandbox. Reuse or extend a validated read handler when it can supply that contract; otherwise add a narrowly scoped artifact-read handler. The existing line-only, shortened display is insufficient for an arbitrarily long line. Register the selected retrieval capability with a stable tool schema for the opted-in session; do not inject new definitions only after a large output arrives or widen general filesystem permissions as a shortcut.

Typed artifact readback is a non-persisting projection of the existing artifact: its bounded result gives the source stream/range and next range without creating another output artifact. Do not recognize this exemption by parsing arbitrary shell command strings. General shell transformations retain ordinary capture behavior and are new tool executions; they are not the guaranteed zero-recursion retrieval path. Test exact bounded readback before claiming output recovery is complete.

Byte offsets are authoritative for raw storage. Text ranges and search are optional views over retained bytes; they must identify omitted or undecodable material and obey an output budget. A read from a writing artifact uses a fixed committed boundary for that call. Reading an expired or inaccessible artifact returns that state, without rerunning its source command.

## History, compaction, resume, and ownership

The artifact catalog is durable host metadata keyed by source session and execution identity. It is not a replacement for ordered conversation history. Record the compact reference inside the original paired tool result; a later retrieval has its own call/result pair. UI output events and a WebSocket continuation ID are not persistence mechanisms.

Previously recorded previews, references, and opaque reasoning/compaction items remain byte-stable. Compaction may remove the model-visible message containing a locator, so physical artifact retention and conversational rediscovery are separate contracts. The initial implementation guarantees retrieval from a known locator; important references can be recorded through [existing work notes](../../features/session-work-notes.md). It does not inject an ever-growing artifact inventory after every compaction. Evaluate any future bounded catalog-discovery tool separately.

Resume reopens existing metadata and retained bytes. A writing artifact whose producing process cannot be reattached becomes interrupted; it must not be declared complete from file existence alone. Forks may reference source-session artifacts read-only, subject to their actual access policy and retention lifetime. They do not take ownership of the source artifact or implicitly extend its lifetime.

Keep active artifacts and in-flight reads out of expiration. Cleanup may remove eligible finalized bytes under the documented retention policy, while leaving bounded metadata sufficient to explain a stale reference. Retention policy and quotas must be explicit before release; choose their values from fixture and usage measurements rather than copying another product's limits. Session export/relocation is unsupported until it carries referenced artifacts and rewrites access metadata coherently.

## Reference patterns and limits

Claude Code's `src/utils/toolResultStorage.ts` and `src/tools/BashTool/BashTool.tsx` provide a useful file-reference-plus-preview pattern. The inspected Bash implementation also caps persisted data and has error paths that bypass successful-output persistence; this plan deliberately requires truthful bounded retention on both success and failure. Its character/byte constants are not defaults for this branch.

The newer Codex `core/src/tools/context.rs::ExecCommandToolOutput` provides a useful example of reserving metadata space before history truncation. Its bounded pending/transcript buffers and code-mode result handling do not constitute a full-output artifact store. Adopt the sizing principle without importing unrelated tool schemas or assuming that server-backed web output uses the same pipeline.

## Implementation order

1. Add the bounded byte sink, artifact states, and durable catalog with injected quota/queue/I/O failures. Establish the capture point and finalization ordering before changing previews.
2. Connect unified pipe and PTY execution, initial results, polls, completion, and unsuccessful outcomes. Then integrate legacy shell through the same storage contract.
3. Make formatter/history budgeting preserve the compact reference and exercise retrieval through actual tools and sandbox configuration.
4. Exercise recorder/replay, real compaction, cancellation, crash recovery, expiration, and fork ownership. Set measured retention limits and document the supported access lifetime.
5. Move the implemented contract into `docs/features/` and remove this plan when complete; use the [evaluation plan](evaluation_plan.md) for behavioral outcomes.

## Acceptance evidence

|Case|Required result|
|---|---|
|Small output|Useful model result and command semantics remain unchanged|
|Output beyond preview and capture caps|Recover a seeded middle marker through an actual bounded tool read without rerunning the command|
|PTY, separate streams, and lagged preview receiver|Recover reader-observed bytes with truthful stream labels and completeness|
|Nonzero exit, timeout, cancellation, sandbox denial, pre-spawn failure|Preserve execution outcome and independently accurate artifact status|
|Cancellation followed by EOF; descendant-held descriptors|A cancelled process can retain complete output; a drain deadline instead reports cutoff and unknown remainder|
|Slow writer, queue overflow, disk full, artifact/session quota|Bounded resource use and pipe draining; retained prefix and omission reason are accurate|
|Partial writes, cancellation during writing, and failed flush/synchronization|No duplicated append or invented committed bytes; capture and durability claims reflect separate acknowledgments|
|Invalid UTF-8 and long lines|Raw bytes survive; a tail marker on one oversized line is recoverable in bounded byte ranges without secondary artifacts|
|Split-stream quota exhaustion|Per-stream ranges remain meaningful when one stream consumes the shared quota; both retained frontiers and subsequent omitted bytes are truthful|
|Small preview and tight history budgets|Mandatory recovery metadata survives without double truncation|
|Poll racing with completion|One stable artifact identity, consistent committed ranges, no false completion|
|Crash around artifact publication|A persisted receipt never promises more bytes than the acknowledged recoverable prefix, including writing-state receipts|
|Recorder/replay, real compaction, restart, and forks|Known locators resolve to retained evidence or explicit unavailability; call/result pairing and prior message bytes stay stable|
|Expiration and failed reopen|No deletion of an active read and no empty replacement for a stale locator|

These deterministic checks establish storage and context contracts. They do not establish answer-quality improvements; those require repeated tasks with complete parent, child, and helper usage accounting.
