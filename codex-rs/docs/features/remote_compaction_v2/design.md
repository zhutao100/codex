# Streaming-compaction design

## Request and collector

[The implementation](../../../core/src/compact_remote_v2.rs) uses the ordinary Responses client rather than changing the URL of the unary compact API. The two protocols have different request, response and retention contracts. Reuse an active client for mid-turn compaction; standalone requests create a session for the resolved source provider.

Project history first, then append exactly one request-only `CompactionTrigger`. Reuse ordinary sampling tools when compatible with the source model; otherwise derive them through the normal tool router. Set `output_schema` to `None` for the compact request and restore normal sampling afterward. Request-local turn metadata adds compaction reason/phase/implementation without mutating cached sampling metadata.

The collector counts completed compaction items, not all output items. It accounts completed usage even if cardinality is invalid, but installs only one completed encrypted summary after `Completed`. EOF, cancellation or stream failure discards the candidate. Unrelated assistant, tool and reasoning items are ignored.

Outer retries are capped at `min(provider.stream_max_retries(), 2)` per transport. Ordinary WebSocket-to-HTTP fallback resets that allowance once. HTTP opening retries and authentication recovery remain client-owned. Permanent 4xx errors other than 408/429 and completed protocol-validation errors stop; no semantic fallback to unary compaction or local summarization is inferred.

## Source and destination contexts

`CompactionContext` separates request/source from replacement/destination. [Turn orchestration](../../../core/src/session/turn.rs) resolves the previous model's provider and effective instructions for downshift capture; it never changes the selected regular model merely to compact. The destination supplies effective window/automatic limit, replacement instructions/tools, canonical context, adjacent reference context and installation accounting.

Selected-model/personality requests retain session instructions, including resumed instructions. A temporary previous-model context resolves explicit config instructions, then the model's final override, then effective model instructions. Provider-resolution errors remain errors, not an excuse to use the destination's provider.

The notes request stores `Compaction source model: "JSON-encoded-model"` on its second line. Recovery uses it independently of current usage and the resume-selected destination; malformed explicit metadata fails before mutation. Older captures can infer a larger previous model from prior-turn settings without trusting usage reported by the notes response. The capture request is excluded from compaction input.

## Retention and fitting

[Retention](../../../core/src/compact_remote_v2_history.rs) gives newest eligible user messages priority, retaining full messages in original order. Plaintext summaries and interruption markers remain eligible; stale contextual messages, old encrypted summaries and assistant/tool history are summarized instead of retained verbatim.

For a boundary message, middle-truncate text while charging JSON escaping, item overhead and the notice. With images, prefer later content and keep each image with its adjacent labels. Keep base64 intact and charge both inline and remote images with the image estimate without fetching remote URLs. A dropped boundary image does not cause older messages to reappear.

The context target is the smaller available positive effective window or positive automatic-compaction threshold minus one; the subtraction applies to the automatic threshold. Subtract fixed costs for the encrypted summary, exact notes, instructions, tools and canonical context, then cap remaining retention at 64,000 approximate tokens. Reserve canonical context even when it will be seeded on the next turn. Reject fixed-content overflow. Arbitrary new input arriving after pre-turn compaction cannot be budgeted in advance.

The separate [tool-output fitter](../../../core/src/compact_remote.rs) rewrites only a supported contiguous trailing output suffix, ignoring local snapshots. Preserve call IDs, arguments and success flags; stop at other content and never enlarge an already-small output. Keep the accumulator unclamped until the final estimate. This is not a general fitting policy for oversized user input or instructions.

## Notes transaction and recovery

[The shared scanner](../../../core/src/compact.rs), `pending_work_notes`, identifies the capture request and its bounded segment. The next real user message ends that segment. Preserve known abort controls, snapshots and all later source history; unidentified work inside the segment fails without mutation. Recover this transaction before adding new input or ordinary sampling.

- Match rejected function/MCP/custom-tool outputs to capture call IDs and the exact rejection text. They may follow completed notes because tool futures drain after the response stream.
- Preserve incomplete capture segments during live/durable dangling-call cleanup so notes recovery can handle a missing rejected output.
- Concatenate assistant output-text parts without separators for both live capture and replay. Bypass plan rendering during capture, including in output helpers, so split sentinels and multiline `<proposed_plan>` content remain exact.
- After completed notes, accept trailing reasoning and assistant messages containing only empty/whitespace output-text. Preserve the whole notes text, including a suffix after its closing tag. Nonempty later text, nontext content and unmatched activity remain ambiguous.
- Request final-answer placement and both delimiters, but do not make that guidance a new parser prerequisite: existing prefix-only captures remain valid.
- If incomplete capture has later input, use available notes or the no-notes path without requesting notes across that boundary. Propagate cancellation before considering a no-notes fallback.

Failed compaction remains unfinished main work. Acknowledged steering retained during finalization becomes later source history rather than disappearing into the capture. Successful inline compaction defers queued input until its first follow-up.

Local summarization buffers output per attempt. EOF, failure and retries discard that buffer; only a completed attempt supplies the replacement summary. Appending uncommitted summarizer output to live history would make the preceding notes transaction ambiguous and block recovery.

## Installation, snapshots and replay

The replacement contains retained messages plus the encrypted summary, followed by exact notes and local snapshots. When injecting canonical context inline, place it before the last real user message, otherwise the last plaintext summary, otherwise the encrypted summary, otherwise at the end. Image-only input is a real user anchor. Notes intentionally follow the encrypted summary to preserve explicit carryover.

`replace_compacted_history` writes the exact replacement and appropriate adjacent reference context and recomputes usage once. The full local baseline covers all installed items, including notes and retained reasoning; persist its covered-item count so live accounting and replay add only later items. See the shared [usage contract](../post_turn_completion_review/budget_accounting.md).

Before installation, cancellation leaves live history intact. Once installation starts, an owned task holds the installation guard through memory replacement, persistence and completion notification; task-stop joins it before building stopped state. This prevents cancellation from splitting that operation, not power-loss atomicity.

Background snapshot recording uses the same guard. Snapshots completed during the remote request enter the replacement; later writers append after its checkpoint. Snapshots stay out of model requests.

After successful installation, clear logical WebSocket continuation and retain only a healthy completed physical connection. Failed/cancelled requests must remain marked unfinished; a different local/unary request must not accidentally make an unfinished notes socket cacheable.

Standalone/pre-turn replacement clears the reference baseline. Inline replacement retains the matching destination reference. Replay uses that inline provenance to infer unfinished work even with encrypted-only history, then respects subsequent completion and rollback. Pending captures keep the resume-selected destination while retaining their separate source. Standalone compaction must not consume the existing interrupted/paused main checkpoint.

## Scope

Keep local and unary selection available explicitly. Automatic server-side context management, other provider families and cross-turn logical WebSocket caching are separate changes. Do not import new history-envelope/window abstractions solely to implement this protocol; integrate with the existing history, replay and task lifecycle contracts.
