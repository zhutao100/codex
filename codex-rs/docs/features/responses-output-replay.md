# Responses Output Replay

Use `codex --enable output_token_replay` or set `output_token_replay = true` under `[features]` before starting a session. The feature is under development and off by default. It requests an opaque representation of sampled output from OpenAI so full input can more closely reproduce a healthy [WebSocket continuation](responses-websocket.md). It cannot recover fields that were previously discarded, force the endpoint to supply replay data, or guarantee a prompt-cache hit.

## Request and History Contract

|Destination|Ask for new `output.encrypted_content`|Send previously received sampled-output ciphertext|
|---|---|---|
|OpenAI, flag on|Yes, independently of the model's reasoning-summary setting.|Yes.|
|OpenAI, flag off|No.|Yes, including after resume.|
|Other provider, either flag|No.|No. Filter only the outbound copy, including compaction; keep the original in history.|

The [wire types](../../protocol/src/models.rs) retain optional ciphertext on assistant messages and function, custom tool, local shell and hosted search calls. They also retain message/function status and output-text annotations/logprobs, distinguishing absent from empty arrays. Preserve returned values; never invent a search result from its query. Prior reasoning/compaction ciphertext, the legacy server-item ID policy, tool-call IDs and ordered tool results are separate contracts. The opt-in does not enable server storage or change the reasoning include on non-reasoning models.

Ciphertext consumes wire and rollout bytes, but is not another copy of visible prompt text for local budgeting. Memory extraction omits top-level ciphertext. A retained message rewritten during compaction must not claim the original sampled text's replay data.

## Diagnosing a Drop

- `codex.websocket.continuation` counts WS send attempts (including failed sends), not successful completions. `mode` distinguishes `full` from `incremental`; `reason` distinguishes initial/restored history, a closed connection, missing response state, changed request properties, and shortened or mismatched item prefixes. Mismatch labels contain only item types. This transport sends generation requests, not upstream's history-bearing warmup requests.
- `codex.responses.reasoning_replay` counts completed response reports: unavailable, no reported removals, prefix mismatch, or other removals. `codex.responses.reasoning_items_removed` records a histogram per bounded reason, including zeros for usable arrays. Missing, null or non-array reports produce no histogram. An unknown/missing removal reason maps to `other`; non-reasoning entries and non-removal actions do not count. The sampling step supplies the metric tags; this release has no `product_sku` metadata to attach.
- Use transport and completion evidence together. A full replay can hit the prompt cache; a removal report alone does not prove a reconnect. Do not retry successful inference, edit history, or change compaction policy based on either metric.

## Verification

The [replay fixtures](../../core/tests/suite/output_token_replay.rs) exercise OpenAI and other providers, both flag states, recorder shutdown/resume and a rejected WS delta followed by a fresh-socket full replay. They assert the outgoing fields and tool-result order, not model behavior. The [metrics fixture](../../otel/tests/suite/snapshot.rs) checks report availability and removal counts against the common Responses parser. A hosted cache claim requires a separate live comparison at the same completed boundary.
