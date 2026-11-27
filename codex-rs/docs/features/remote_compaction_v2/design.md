# Streaming-compaction design

## Request and collector

[The implementation](../../../core/src/compact_remote_v2.rs) uses the ordinary Responses client rather than changing the URL of the unary compact API. The two protocols have different request, response and retention contracts. Reuse an active client for mid-turn compaction; standalone requests create a session for the resolved source provider.

Project the ordinary work prefix, including its request-only review frames and previous notes wrapper in place, then append exactly one `CompactionTrigger` instead of the capture suffix. If notes completed in this live checkpoint, reuse their exact ordered tool declarations. Otherwise, including structured work with no notes and recovery from a saved decision, resolve tools from the outgoing task and its work input through the normal router. A refused review or shared connection supplies no tools. Use the selected declarations for formatting, fitting and transmission. Set `output_schema` to `None` for V2; the replacement uses its own task settings.

The collector counts completed compaction items, not all output items. It accounts completed usage even if cardinality is invalid, but installs only one completed encrypted summary after `Completed`. EOF, cancellation or stream failure discards the candidate. Unrelated assistant, tool and reasoning items are ignored.

Outer retries are capped at `min(provider.stream_max_retries(), 2)` per transport. Ordinary WebSocket-to-HTTP fallback resets that allowance once. HTTP opening retries and authentication recovery remain client-owned. Permanent 4xx errors other than 408/429 and completed protocol-validation errors stop; no semantic fallback to unary compaction or local summarization is inferred.

## Source and destination contexts

`CompactionContext` separates request/source from replacement/destination. [Turn orchestration](../../../core/src/session/turn.rs) resolves the previous model's provider and effective instructions for downshift capture; it never changes the selected regular model merely to compact. The destination supplies effective window/automatic limit, replacement instructions/tools, canonical context, adjacent reference context and installation accounting.

An outgoing sampled source restores its exact [recorded instructions](../history-replay.md#replay-metadata-and-compaction-epochs). Without that reference, selected-model/personality requests retain session instructions and temporary previous-model requests resolve explicit config, then the model's final override, then effective model instructions. Resolve the source slug's provider from current configuration. Keep the original work receipt and sampled controls through a login change; use them only on their [own route](../session-work-notes.md#fit-and-prefix-reuse). For `parallel_tool_calls`, keep a sampled `false` when the catalog enables parallelism, but never send `true` after support is withdrawn. Other routes and older records missing the field use the current request setting. A resolution error is not permission to use the destination's provider.

Use the outgoing task for both helper requests and the incoming task for installation and ordinary follow-up. Equality of controls on the current endpoint does not imply equality on the work endpoint after a login change. A compatible transport can be reused without substituting the destination's settings. Restore the source recorded with the compaction decision or frame deferral; the notes marker also stores `Compaction source model: "JSON-encoded-model"` for older captures. Malformed explicit metadata fails before mutation. The notes response cannot measure its sibling's work and is excluded from compaction input.

Activate the source working directory and sandbox policy on capable MCP servers before capture or source tool lookup. When the destination differs, switch to it after the source responds, before resolving replacement tools and sampling ordinary work. Compare task settings, not just model names: an outstanding response can cross the limit on the same model while the next task has a different schema, directory or tool policy.

## Retention and fitting

[Retention](../../../core/src/compact_remote_v2_history.rs) starts from the fitted input of the send that returned the accepted summary, without the request-only trigger. Do not independently refit from a potentially newer login or from a failed attempt. Give newest eligible user messages priority, preserving their order. Plaintext summaries and interruption markers remain eligible; stale contextual messages, old encrypted summaries and assistant/tool history are summarized instead of retained verbatim.

For a boundary message, middle-truncate text while charging JSON escaping, item overhead and the notice. With images, prefer later content and keep each image with its adjacent labels. Keep base64 intact and charge both inline and remote images with the image estimate without fetching remote URLs. A dropped boundary image does not cause older messages to reappear.

The context target is the smaller available positive effective window or positive automatic-compaction threshold minus one; the subtraction applies to the automatic threshold. Subtract fixed costs for the encrypted summary, exact notes, instructions, tools, destination response format and canonical context, then cap remaining retention at 64,000 approximate tokens. Reserve canonical context even when it will be seeded on the next turn. Reject fixed-content overflow. Arbitrary new input arriving after pre-turn compaction cannot be budgeted in advance.

For a pending [ordinary review](../post_turn_completion_review/budget_accounting.md#recovering-from-overflow), use the smaller of that remaining installation space and the ordinary space left after mandatory scope, advice and output reserve. Price destination tools without discarded transcript mentions; if retained user text selects additional connectors, reduce optional retention to pay for their schemas. No source frame, verbatim notes or mandatory scope is trimmed to retain an optional message.

The [tool-output fitter](../../../core/src/compact_remote.rs) uses the [pre-capture work estimate](../session-work-notes.md#fit-and-prefix-reuse). Resolve the source tools and format their paired results before fitting. V2 subtracts the final trigger from its effective window, using the same source limit as review admission. Exactly fitting input stays unchanged; over budget, rewrite only supported contiguous trailing results, never enlarging an output or changing its call. Unary has no trigger charge. This is not a general fitting policy for oversized user input or instructions.

## Notes transaction and recovery

[The shared scanner](../../../core/src/compact.rs), `pending_work_notes`, identifies the capture request and its bounded segment. The next real user message ends that segment. Preserve known abort controls, snapshots and all later source history; unidentified work inside the segment fails without mutation. Recover this transaction before adding new input or ordinary sampling.

- Record capture rejections after model items, without executing tools. On retry, append only missing rejections to the open capture in call order before normalizing the request. A cut between outputs must neither duplicate the recorded result nor synthesize an ordinary `aborted` result. Match by wire call kind and ID; accept earlier clients' exact failed-batch/MCP rejection formats only for a corresponding capture call.
- Preserve incomplete capture segments during live/durable dangling-call cleanup. Do not append a repair after a later real user boundary. If a tagged output item was recorded before a disconnect, use its exact bytes rather than asking for more notes.
- Concatenate assistant output-text parts without separators for both live capture and replay. Bypass plan rendering during capture, including in output helpers, so split sentinels and multiline `<proposed_plan>` content remain exact.
- After tagged notes, accept trailing reasoning, whitespace-only assistant output and the exact journal-owned local calls from that same response. Finish a missing denial before retry or compact; a claim without its call needs no denial. Preserve the full notes text, including any suffix after its closing tag. Unmatched activity remains ambiguous.
- Request final-answer placement and both delimiters, but do not make that guidance a new parser prerequisite: existing prefix-only captures remain valid.
- If incomplete capture has later input, use available notes or the no-notes path without requesting notes across that boundary. Propagate cancellation before considering a no-notes fallback.

Failed automatic compaction remains unfinished main work. Its host checkpoint precedes the notes marker or schema-only compact and ends at `Compacted`, not `response.completed`. A stop at the notes marker may require resampling notes; a saved notes answer does not. A compact receipt without a persisted replacement requires another compact from the frozen source, not an ordinary request on the old history. Fresh pre-turn capture keeps `DoNotInject` through these cuts; inline capture keeps its original placement. An older marker without this checkpoint uses the available legacy evidence.

Before that checkpoint exists, distinguish a completed task from an interrupted one. Price a fresh destination on its own route; replaying an acceptance alone does not make a finished task's bill a measurement of another endpoint. An over-limit outgoing response that still owes a tool follow-up or precedes a queued steer can establish a source obligation even after a rebind. Do not use that bill to fit the helper or count it again after installation.

Admission is the boundary for routing the prompt. Before admitting B's input, a downshift may compact A's outgoing history on A. After B's input is admitted, A's previous completion cannot authorize sending that input back to A. If B refuses it, `/continue` uses the admitted task's settings for any newly selected notes and trigger. Standalone `/compact` uses that owner even when restart defaults to A, but still requests no fresh notes. An explicit retarget is priced independently; it does not undo a previously journaled checkpoint.

`/compact` and `/continue` recover the same checkpoint: send the remaining helpers on the sampled source and fit the replacement for the first ordinary destination, including its response format. Admission requires an idle thread and, for review-derived work, its original directory; refuse before cleanup or inference. `/compact` stops at installation without consuming the continuation or injecting its default context. An interrupt belongs to the command, not ordinary history: do not write a user-abort marker or replace the ordinary turn ID, model or schema. Count the actual compaction attempt once, not the command and the recovered attempt separately.

Local summarization buffers output per attempt. EOF, failure and retries discard that buffer; only a completed attempt supplies the replacement summary. Appending uncommitted summarizer output to live history would make the preceding notes transaction ambiguous and block recovery.

## Installation and replay

The replacement contains retained messages plus the encrypted summary, followed by exact notes. When injecting canonical context inline, place it before the last real user message, otherwise the last plaintext summary, otherwise the encrypted summary, otherwise at the end. Image-only input is a real user anchor. Notes intentionally follow the encrypted summary to preserve explicit carryover.

`replace_compacted_history` writes the exact replacement and appropriate adjacent reference context and recomputes usage once. The full local baseline covers all installed items, including notes and retained reasoning; persist its covered-item count so live accounting and replay add only later items. See the shared [usage contract](../post_turn_completion_review/budget_accounting.md).

Before installation, cancellation leaves live history intact. Once installation starts, an owned task holds the installation guard through memory replacement, persistence and completion notification; task-stop joins it before building stopped state. This prevents cancellation from splitting that operation, not power-loss atomicity.

Legacy worktree snapshots are readable from older rollouts but excluded from model requests. There is no background snapshot producer to join during installation.

After successful installation, clear logical WebSocket continuation and retain only a healthy completed physical connection. Failed/cancelled requests must remain marked unfinished; a different local/unary request must not accidentally make an unfinished notes socket cacheable.

Standalone/pre-turn replacement clears the reference baseline. Inline replacement commits the matching destination reference with the replacement and its local usage ledger; the following context and usage records remain readable by older clients. Replay does not require either later write to determine inline provenance or to send the first ordinary request without a second compact. The marker is not measured work and cannot by itself create a standalone owner. See [history replay](../history-replay.md#replay-metadata-and-compaction-epochs) for legacy and rollback behavior.

## Scope

Keep local and unary selection available explicitly. Automatic server-side context management, other provider families and cross-turn logical WebSocket caching are separate changes. Do not import new history-envelope/window abstractions solely to implement this protocol; integrate with the existing history, replay and task lifecycle contracts.
