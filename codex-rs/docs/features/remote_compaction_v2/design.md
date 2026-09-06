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
|A stale request marker could delete all later history.|Validate the suffix; reject later user/work records without mutation. Preserve late ghost snapshots.|
|Pre-turn capture errors included cancellation.|Propagate cancellation before no-notes fallback.|
|Previous-model compaction bypassed fresh notes.|Share pre-turn capture under the previous provider/model.|
|64,000 retained tokens can exceed smaller windows once fixed context is added.|Use it as a ceiling; reserve all known fixed costs and apply the destination model's limits on downshift.|
|A valid encrypted-only replacement could not continue after restart.|Use the existing inline-compaction epoch and tail-completion semantics; fix dangling-call cleanup to respect the same epoch.|
|Clearing unfinished WebSocket tracking could make a bad socket cacheable.|Reset after successful installation; discard an unfinished notes socket when local/legacy compaction completed through a different request. Preserve completed physical connections.|
|The replacement writer alone was not cancellation-safe.|Shield installation in a short child task and join its guard from task-stop before pause/abort state is built.|
|Legacy trim tests and mock guards could conceal behavior.|Select legacy explicitly, update call-preserving assertions, and execute localhost fixtures through the existing guard.|

## Stream and retry semantics

The collector counts completed compaction items, not all output items. It accounts completed usage even if cardinality is invalid, but never installs a partial or invalid result. EOF and stream errors discard the candidate. Extra tools/reasoning/assistant items are ignored.

The outer retry allowance is `min(provider.stream_max_retries(), 2)` per transport. Existing WebSocket-to-HTTP fallback resets that allowance once. HTTP opening retries and authentication recovery remain owned by the normal client. Permanent 4xx errors, except 408/429, stop the outer loop. No completed protocol-validation failure triggers transport or semantic fallback.

Request-local `x-codex-turn-metadata` preserves existing workspace fields and adds `request_kind = compaction`, trigger, reason, phase, implementation `responses_compaction_v2` and strategy `memento`. Missing workspace metadata is supported; sampling's cached metadata is unchanged.

## Retention and estimates

Newest eligible messages receive budget first; retained full messages preserve their original structure and order. Text boundaries use middle truncation with the notice itself charged. Image boundaries prefer later content and keep each image with its adjacent labels. A dropped boundary image does not cause older messages to reappear.

Inline base64 remains intact and uses the branch's fixed image estimate. Remote image URLs are also charged as images without fetching them. Retention conservatively includes serialized item overhead and JSON escaping; it is not tokenizer-exact.

The target is the smaller available positive effective window or auto-compaction threshold minus one. The fixed estimate includes instructions, tools, canonical context even when deferred to the next turn, encrypted summary and preserved notes. The remaining budget is capped at 64,000. Pre-turn compaction cannot predict arbitrary incoming user input recorded afterward.

Output fitting rewrites only a contiguous supported trailing output suffix, ignoring local snapshots. It retains call IDs, arguments and success flags, stops at a non-output boundary, and never enlarges a small output. An unclamped accumulator avoids subtracting from an already saturated token total. This is not a general fitting policy for oversized user input or instructions.

## Persistence and cancellation

`replace_compacted_history` remains the single replacement writer and recomputes token usage once. The shielded installation orders the exact replacement, adjacent reference context and completion event before task-stop constructs its pause/abort result. This prevents cancellation from splitting the existing operation; it does not introduce power-loss atomicity or a new disk format.

Standalone/pre-turn replacements clear the reference baseline. Inline replacements retain a matching reference context. Replay infers unfinished encrypted-only work only from inline provenance, applies ordinary completion semantics to the subsequent tail, and respects rollback across the compaction epoch.

Notes intentionally follow the encrypted summary. This preserves the branch's established carryover contract, whereas upstream normally keeps the summary last. The deployment check must therefore cover an automatic notes-bearing compaction and its follow-up.

## Deferred

No Azure/Bedrock expansion, compatibility hashes, previous-model error fallback, legacy payload/timeout overhaul, automatic server-side `context_management`, new hooks, history envelopes, window IDs or cross-turn logical WebSocket caching.
