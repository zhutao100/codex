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

Source and destination contexts have different roles: the notes/compaction request uses the source model's provider and effective instructions; retention and subsequent work use the selected destination. Persist the source in the capture request so low notes usage, pause or restart cannot erase a pending downshift. Explicit recovery model selection changes the destination only.

Standalone compaction preserves paused/interrupted main ownership. Inline replacement uses its adjacent reference context to support replay even when no real user message remains. Review-derived work additionally retains its [exact snapshots and budget contract](../post_turn_completion_review/budget_accounting.md).

Follow the [design contracts](design.md) before modifying notes parsing, installation or replay. [Source selection](../../../core/src/compact.rs), [streaming implementation](../../../core/src/compact_remote_v2.rs) and [retention](../../../core/src/compact_remote_v2_history.rs) define the implemented behavior.
