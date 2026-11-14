# Plan: recoverable tool output

Status: proposed; no output-artifact store or corresponding runtime setting is implemented.

## Confirmed gap

Unified execution retains a bounded head/tail buffer before rendering its tool preview. Legacy shell capture keeps a bounded prefix. Neither path provides a durable address for every omitted byte. A rollout of the truncated result cannot reconstruct that output, and rerunning a stateful command is not equivalent recovery.

Read [unified execution](../../../core/src/unified_exec/mod.rs), its [process receiver](../../../core/src/unified_exec/process.rs), [head/tail buffer](../../../core/src/unified_exec/head_tail_buffer.rs), and [legacy capture](../../../core/src/exec.rs) before changing limits. The current capture cap is 1 MiB; the unified tool's default preview is 10,000 tokens. These are different truncation layers.

## Required contract

1. Capture observed bytes before the first destructive limit or lossy fanout. A copy of the rendered tool response is insufficient.
2. Keep existing memory caps, small-result formatting, command exit semantics and terminal behavior. Bound artifact bytes, session total, writer queue memory and shutdown time separately.
3. Give omitted output an address readable through the effective sandbox, a completeness state, retained/observed byte counts and a bounded preview. Retrieval must not depend on the original process handle or rerun the command.
4. Preserve completed artifacts and recorded previews as immutable evidence. Quota, expiration, interruption and I/O failure must produce explicit incomplete/unavailable states, never a false claim of complete retention.
5. Reserve room for the address/status footer through tool rendering and history truncation. Keep raw artifact bytes out of prompt history and rollout response items.

## Implementation decisions

- Share a session-owned writer between unified execution and legacy shell. For unified execution, tap the byte readers in [pipe](../../../utils/pty/src/pipe.rs) and [PTY](../../../utils/pty/src/pty.rs) handling before broadcast fanout: the existing process receiver skips lagged broadcast messages.
- Preserve PTY output as a merged stream. Separate pipe streams may retain stream identity; do not invent a cross-stream ordering the OS did not supply. Store raw bytes independently from display decoding.
- Continue draining child output when storage fills or fails. A slow/full writer must not cause unbounded memory or a pipe deadlock. Publish complete status only after successful finalization.
- Use runtime-owned paths and call-owned names. Check actual model-readable permissions before selecting a location. Reopening after resume/fork must preserve source-session ownership; do not create an empty replacement for a saved address.
- Initial retrieval uses existing bounded file/shell tools. Add a new retrieval tool only if those tools cannot satisfy the access contract. Leave concrete quotas configurable or internal until fixture measurements justify defaults.
- Compaction may summarize away the original locator. Important addresses can use existing work notes; retrieval tests supply a known address after compaction rather than assuming automatic rediscovery.

## Remaining work

1. Build the bounded writer and result formatter with injected quota/I/O failures.
2. Integrate unified execution, polling and process completion, then legacy shell with the same outcome type.
3. Exercise actual persistence, compaction, restart, cancellation and expiration before changing preview size.
4. Document retention/access behavior and measured limits in a feature doc; remove this plan when implemented.

## Acceptance evidence

|Case|Required result|
|---|---|
|Small output|Unchanged model result and exit behavior.|
|Output beyond preview and capture caps|Recover a seeded middle/tail marker with no command rerun.|
|PTY, split streams and lagged preview receiver|Retain reader-observed bytes with accurate stream/completeness labels.|
|Nonzero exit, timeout, cancellation, spawn failure|Original command outcome remains visible; artifact status is truthful.|
|Slow writer, disk full, quota, invalid UTF-8|Bounded resources, no deadlock, byte-preserving retention where available.|
|Real compaction, restart and fork ownership|Known artifact address remains retrievable or explicitly unavailable, never silently recreated.|
|Later requests|Previously recorded result bytes stay stable; full output is not reinjected automatically.|

Use scoped deterministic tests and the [evaluation plan](evaluation_plan.md). Storage/transport tests establish recovery behavior; live answer-quality claims require separate repeated tasks.
