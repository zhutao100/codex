# Streaming remote compaction

Compaction over the ordinary Responses transport. Read this before changing compaction dispatch, retained history, work-notes recovery or continuation after compaction.

|Need|Read|
|---|---|
|Retention, source/destination contexts and durable installation|[Design](design.md)|
|Collector, transport and recovery regressions|[Validation](validation.md)|
|Optional deployed-endpoint check|[Native acceptance](native_acceptance.md)|

## Selection

|Provider|`compaction_mode`|Implementation|
|---|---|---|
|`provider.is_openai()`|`auto` (default)|Ordinary Responses stream with a final `compaction_trigger` input item.|
|`provider.is_openai()`|`unary`|Explicit compatibility route using `responses/compact`.|
|Any|`local`|Local summarization.|
|Not OpenAI|Any|Local summarization.|

Set the mode at the top level or in the active profile. Eligibility uses the selected source provider's existing name check, not the authentication mode. There is no automatic streaming-to-unary or remote-to-local fallback; normal WebSocket-to-HTTP transport recovery remains available. The `remote_compaction_v2` beta capability is advertised independently of the chosen route.

## Core contracts

- Advertise `remote_compaction_v2` in `x-codex-beta-features`. Preserve ordinary authentication, tools/order, reasoning, service tier, cache key and transport behavior; omit the final-output schema only for compaction.
- Append one trigger after history projection. Never persist it or replay it as conversation history.
- Collect privately until `Completed` contains exactly one completed encrypted compaction item. Other output items do not execute tools or enter history. Incomplete/invalid results never install a replacement.
- Retain recent eligible user messages and plaintext summaries within an approximate budget capped at 64,000 tokens. Reserve encrypted summary, exact notes, instructions, canonical context and tools before choosing the retention allowance.
- Keep work notes exact and outside truncatable retention. Fixed content that exceeds the destination limit rejects installation.
- Preserve the exact replacement/reference checkpoint across cancellation. No task captures worktree snapshots during a turn.
- Reset logical WebSocket continuation after replacement; only a healthy completed physical connection may be reused.

## Lifecycle constraints

Manual compaction preserves existing notes without requesting fresh notes. Automatic/pre-turn and model-downshift paths recover a bounded notes transaction before ordinary work. Notes are never a completed answer to the user's task; failed compaction retains continuation and acknowledged later input.

Check outgoing work before admitting a fresh prompt, including on `/continue` after acceptance but before the first capture marker. A previous final answer does not exempt the new task from this check. On a downshift at or above the destination's threshold, both helpers use the larger model that measured the work, with its provider and instructions. A review that needs compaction below that threshold uses the same source until the destination samples ordinary work.

Before either helper, persist the automatic compaction decision: source settings (including whether notes are free-form), fitting adjustment and placement of canonical context. A positive compact receipt accounts for the call; it does not install the replacement or cancel the decision, even below the trigger or when a schema skips notes. Restart finishes the outstanding phase. Explicit retargeting changes the destination, not that source. Legacy capture markers identify the source model but cannot prove the original placement.

Standalone compaction does not create a work owner. A regular task owns its selected model and schema before capture. After `Compacted` the first ordinary request goes to the destination without another automatic compaction, even at a marker-only cutoff. Fresh pre-turn replacements omit canonical context; [prompt admission](../steering-and-queue.md#request-ordering) installs it once. Inline replacements commit the already-injected reference in the marker. Older records lacking ownership or placement evidence cannot prove it. Review-derived work retains its [snapshots and budget contract](../post_turn_completion_review/budget_accounting.md).

Follow the [design contracts](design.md) before modifying notes parsing, installation or replay. [Source selection](../../../core/src/compact.rs), [streaming implementation](../../../core/src/compact_remote_v2.rs) and [retention](../../../core/src/compact_remote_v2_history.rs) define the implemented behavior.
