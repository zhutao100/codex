# Backport decisions

## Evidence and scope

|Reference|Revision|
|---|---|
|Customized source baseline|`1c74f684550ef379dd50f023e5eb3a29a30f01eb`|
|Inspected local upstream `main`|`ac192cd7937b0d73edc6dffe009940ae53782dd4`|

The current and alternative proposals were cross-checked against these Git worktrees. Upstream still defines `responses/compact`; its enabled-by-default v2 implementation appends `CompactionTrigger` to ordinary Responses input and constructs replacement history locally. A URL-only change would use the wrong request, response and retention contracts.

Source references: upstream `core/src/compact_remote_v2.rs`, `compact_remote_v2_attempt.rs`, `compact_remote_v2_images.rs`, `compact_remote.rs`, `responses_metadata.rs`, `analytics/src/facts.rs`, and the branch's existing client, notes, replay and task-stop implementations.

## Corrections from cross-validation

|Gap in the original plan|Implemented resolution|
|---|---|
|Abandoned notes state disappears on pause/restart.|Infer the bounded capture phase and exact completed notes from existing history before sampling or recording new user input.|
|A stale request marker could delete later history or block retry after interruption.|Bound the capture at the next real user message. Preserve known abort controls, snapshots and subsequent user/work records; reject unidentified work inside the capture segment.|
|Failed compaction finalized notes as a completed turn and cleared continuation.|Keep continuation and skip completion hooks while a notes transaction remains. Persist acknowledged steering after the bounded segment so retry and restart retain it.|
|Rejected tool outputs can be recorded after completed notes.|Recognize exact rejections for matching capture call IDs before rejecting ambiguous later work. Preserve capture segments during live and durable dangling-call cleanup.|
|Pre-turn capture errors included cancellation.|Propagate cancellation before no-notes fallback.|
|Previous-model compaction bypassed fresh notes and could retain destination provider/instructions.|Share pre-turn capture and resolve the previous model's provider and effective instructions; report provider-resolution errors and preserve the selected model.|
|Failed/paused downshift recovery lost the source model after notes lowered reported usage.|Persist the JSON-encoded source model in the notes request; use separate source request and destination replacement contexts during recovery.|
|Replay selected the previous user-turn model for unfinished downshift work.|Pending captures keep the resume-selected destination; installed inline replacements continue with their adjacent reference context's model.|
|Multipart capture recovery inserted newlines and could alter split sentinels or code.|Reuse live assistant output-text concatenation, and bypass plan-tag parsing during notes capture.|
|64,000 retained tokens can exceed smaller windows once fixed context is added.|Use it as a ceiling; reserve all known fixed costs and apply the destination model's limits on downshift.|
|Recomputed usage counted preserved notes twice.|Store a baseline at the last model item, excluding adjustments subsequently added by live/replayed context accounting.|
|A valid encrypted-only replacement could not continue after restart.|Use the existing inline-compaction epoch and tail-completion semantics; fix dangling-call cleanup to respect the same epoch.|
|Clearing unfinished WebSocket tracking could make a bad socket cacheable.|Reset after successful installation; discard an unfinished notes socket when local/legacy compaction completed through a different request. Preserve completed physical connections.|
|The replacement writer alone was not cancellation-safe.|Shield installation in a short child task and join its guard from task-stop before pause/abort state is built.|
|Snapshots completing during the stream were absent from the prepared source.|Gather current snapshots under the installation guard and use that guard for background snapshot recording.|
|Legacy trim tests and mock guards could conceal behavior.|Select legacy explicitly, update call-preserving assertions, and execute localhost fixtures through the existing guard.|

## Stream and retry semantics

The collector counts completed compaction items, not all output items. It accounts completed usage even if cardinality is invalid, but never installs a partial or invalid result. EOF and stream errors discard the candidate. Extra tools/reasoning/assistant items are ignored.

The outer retry allowance is `min(provider.stream_max_retries(), 2)` per transport. Existing WebSocket-to-HTTP fallback resets that allowance once. HTTP opening retries and authentication recovery remain owned by the normal client. Permanent 4xx errors, except 408/429, stop the outer loop. No completed protocol-validation failure triggers transport or semantic fallback.

Request-local `x-codex-turn-metadata` preserves existing workspace fields and adds `request_kind = compaction`, trigger, reason, phase, implementation `responses_compaction_v2` and strategy `memento`. Missing workspace metadata is supported; sampling's cached metadata is unchanged.

## Previous-model instructions

Requests for the selected model/personality keep the session's instructions, including resumed-session instructions. A temporary previous-model context resolves explicit config instructions, then the model's final instruction override, then effective model instructions, without mutating the selected session model. Notes headroom, sampling and compaction use this request context; replacement retention reserves the selected destination model's instructions.

`CompactionContext` pairs the request/source context with the replacement/destination context. The destination supplies effective window and automatic limits, tool costs, canonical context, reference context and installation accounting. Local and legacy implementations share this separation at their existing replacement boundaries.

The developer capture request's second line is `Compaction source model: "JSON-encoded-model"`. Recovery resolves that model independently of the continuation model and current usage. Older requests without the line can infer a larger previous model before the new user boundary; the inference intentionally ignores reported notes usage. Malformed explicit metadata fails before mutation. No new rollout variant is required, and preparation excludes the request from compaction input.

## Retention and estimates

Newest eligible messages receive budget first; retained full messages preserve their original structure and order. Text boundaries use middle truncation with the notice itself charged. Image boundaries prefer later content and keep each image with its adjacent labels. A dropped boundary image does not cause older messages to reappear.

Inline base64 remains intact and uses the branch's fixed image estimate. Remote image URLs are also charged as images without fetching them. Retention conservatively includes serialized item overhead and JSON escaping; it is not tokenizer-exact.

The target is the smaller available positive effective window or auto-compaction threshold minus one. The fixed estimate includes instructions, tools, canonical context even when deferred to the next turn, encrypted summary and preserved notes. The remaining budget is capped at 64,000. Pre-turn compaction cannot predict arbitrary incoming user input recorded afterward.

Output fitting rewrites only a contiguous supported trailing output suffix, ignoring local snapshots. It retains call IDs, arguments and success flags, stops at a non-output boundary, and never enlarges a small output. An unclamped accumulator avoids subtracting from an already saturated token total. This is not a general fitting policy for oversized user input or instructions.

## Persistence and cancellation

`replace_compacted_history` remains the single replacement writer and recomputes token usage once. The shielded installation orders the exact replacement, adjacent reference context and completion event before task-stop constructs its pause/abort result. This prevents cancellation from splitting the existing operation; it does not introduce power-loss atomicity or a new disk format.

The persistence regression blocks a real recorder after memory replacement, invokes actual pause/interrupt handling, and waits for its grace timeout to abort the parent. Task-stop must remain blocked until the installer completes. The test reads the file without an extra flush, compares the exact replacement/reference pair and event ordering, and replays it against live history.

Recomputation stores the estimated baseline at the last model-generated item, subtracting the local-tail and reasoning adjustments already handled by `get_total_token_usage`. Preserved notes after the encrypted summary are therefore added once. Existing `TokenCount` records restore the same baseline after restart, and newly appended input adds its cost once; no new token-info format is needed.

The installer gathers current undo snapshots while holding the same guard as the background snapshot writer. A snapshot recorded during the remote request enters the replacement; a writer acquiring the guard after installation appends after that checkpoint.

Standalone/pre-turn replacements clear the reference baseline. Inline replacements retain a matching reference context. Replay infers unfinished encrypted-only work only from inline provenance, applies ordinary completion semantics to the subsequent tail, and respects rollback across the compaction epoch.

An outstanding notes capture remains unfinished across errors, interruption and restart. Its bounded segment accepts known abort controls and ends at the next real user message. Preparation removes only verified capture items, preserving controls and all later source history. Unidentified work within the segment fails without mutation and retains continuation. If incomplete capture already has following input, recovery uses existing notes or the no-notes fallback instead of requesting notes across that boundary.

Function, MCP and custom-tool rejection outputs may arrive after the assistant's notes because sampling drains tool futures after the stream. Accept exact known rejections only for matching capture calls. If interruption precedes output recording, live and durable continuation cleanup preserve the capture for notes recovery; ordinary dangling work after a later real user boundary is still trimmed.

Live capture, recovery and the native acceptance checker concatenate only assistant output-text parts, without adding separators. This preserves split sentinels and code tokens. Capture disables ordinary plan-mode parsing so literal `<proposed_plan>` blocks survive unchanged.

When a failed regular turn finishes, pending steering keeps the existing acknowledgement/persistence behavior. It becomes later source history for the retry, and continuation remains available instead of treating notes as a final answer or running successful-completion hooks. Successful inline compaction still defers queued input until its first follow-up. These semantics use existing history and continuation records; no persisted queue format is added.

Notes intentionally follow the encrypted summary. This preserves the branch's established carryover contract, whereas upstream normally keeps the summary last. The deployment check must therefore cover an automatic notes-bearing compaction and its follow-up.

## Deferred

No Azure/Bedrock expansion, compatibility hashes, previous-model error fallback, legacy payload/timeout overhaul, automatic server-side `context_management`, new hooks, history envelopes, window IDs or cross-turn logical WebSocket caching.
