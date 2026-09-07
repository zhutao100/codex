# Streaming remote compaction

Implemented on `custom-0.98.0`. [Design and backport decisions](design.md). [Validation and deployment evidence](validation.md).

## Selection

|Configured provider|`remote_compaction`|`remote_compaction_v2`|Implementation|
|---|---|---|---|
|OpenAI|true|true (default)|Normal Responses stream ending in `{"type":"compaction_trigger"}`.|
|OpenAI|true|false|Legacy unary `responses/compact`.|
|Any|false|Either|Local summarization.|
|Other provider name|Either|Either|Local summarization.|

OpenAI eligibility retains the branch's existing provider-name check; it does not test the authentication mode or rewrite configured URLs. Both ChatGPT and API-key requests use the selected provider's ordinary Responses client. With the ChatGPT backend base, v2 uses `responses` instead of the failing `responses/compact` route. WebSockets use the same Responses connection.

Explicit local fallback:

```toml
[features]
remote_compaction = false
```

Disabling only `remote_compaction_v2` selects the legacy endpoint and can reproduce the reported 404. There is no automatic v2-to-legacy or v2-to-local fallback.

## Request and replacement contract

- Advertise the stable `remote_compaction_v2` key in `x-codex-beta-features`.
- Reuse normal authentication, provider URLs, tools/order, reasoning, service tier, cache key, formatting and transport behavior. The compact request omits the final-output schema; subsequent sampling restores it.
- Append exactly one trigger after history projection. Never record, persist, retain or replay a trigger.
- Buffer stream output privately. Install only after `Completed` with exactly one encrypted `Compaction` item. Ignore other output items without executing tools or recording assistant work.
- Retain recent eligible user messages and existing plaintext summaries, then append the new encrypted summary. Older typed compactions, assistant/tool history and stale context are summarized rather than retained verbatim.
- Retain at most 64,000 approximate tokens, including image costs and message overhead. Further reduce retention against the effective context window and auto-compaction threshold after reserving summary, notes, canonical instructions and tool costs.
- Keep work notes exact and outside the truncatable retention budget. Reject installation if fixed content alone exceeds the context target.

## Branch lifecycle

- Manual compaction preserves existing notes without requesting new notes.
- Pre-turn and model-downshift compaction share bounded notes capture; the latter uses the previous model/provider while fitting retained history to the destination model's limits.
- Mid-turn compaction borrows the active client and preserves deferred-input ordering.
- Recover an outstanding notes transaction before ordinary work or incoming user input is appended. Recognize completed notes and abort markers from durable history. A later real user message ends the capture segment; preserve it and subsequent work in the retry source. Reject ambiguous work inside the capture segment.
- Failed compaction keeps continuation available immediately and after restart. Queued steering acknowledged when the failed turn finishes is persisted outside the capture segment; notes are never treated as the final user answer.
- Preserve undo snapshots, including snapshots arriving during notes capture, the compaction stream or interrupted tool work. Snapshot recording and replacement installation share a guard. Snapshots remain absent from model requests.
- Install canonical context before the last real user, then the last plaintext summary, then the encrypted summary, or append it if none exists. Image-only input is a real user anchor.
- Append notes after the encrypted summary and snapshots after notes. Persist exactly this vector plus the appropriate adjacent reference context.
- Cancellation before installation preserves live history. Once installation starts, task-stop waits for its memory/persistence/completion boundary.
- Clear logical WebSocket continuation after installation; reuse only healthy physical connections. Unfinished requests remain marked unfinished on failure or cancellation.
- Replay uses inline-compaction provenance to infer pending work even when retention removes all real users. Later completion and rollback suppress that inference.

## Maintenance

The backport uses existing branch types and persistence rather than upstream history envelopes, step contexts, provider traits or window IDs. Keep the legacy endpoint for explicit configuration; do not rename it to implement v2.

When changing retention or notes handling, exercise collector, transport, pause/restart, replay, output-rewrite and full-context budget tests together. Loopback fixtures must use the existing loopback-aware test guard under `cargo-local`; a network-disabled early return is not transport coverage.
