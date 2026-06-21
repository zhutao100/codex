# Current Turn and History Workflow

## 1. State model

This branch has three related but distinct representations:

|Representation|Primary location|Purpose|
|---|---|---|
|Raw in-memory history|`ContextManager::items` in `core/src/context_manager/history.rs`|Canonical live transcript, oldest to newest|
|Prepared prompt history|`ContextManager::prepare_items_for_prompt_with_modalities`|A cloned, normalized, modality-compatible request body|
|Rollout history|`RolloutItem` records governed by `core/src/rollout/policy.rs`|Durable reconstruction, rollback, resume, and analysis input|

Two metadata values influence later turns:

- `ContextManager::reference_context_item`: the model-visible settings baseline used to emit only context changes on the next regular turn.
- `SessionState::previous_turn_settings`: settings from the latest surviving real user turn, currently only the model slug, used by pre-turn model-downshift compaction.

These values describe different facts and must not be treated as aliases:

- the reference baseline says which context snapshot is represented in model-visible history;
- previous-turn settings say which model accepted the latest surviving real user turn.

Compaction can intentionally clear the first while preserving the second.

## 2. Regular turn sequence

`core/src/session/turn.rs::run_turn` rejects empty input and enters `run_turn_inner`.

The regular-turn path is:

1. Emit `TurnStarted`. This is a client lifecycle event and is not persisted by this branch's rollout policy.
2. Run previous-model inline compaction when switching from a larger context-window model and the history exceeds the new model's limit.
3. Run current-model automatic compaction if still at or above the active limit. The branch-specific preserved-work-notes flow may sample notes before compacting.
4. Call `record_context_updates_and_set_reference_context_item`:
   - inject full initial context if no reference baseline exists;
   - otherwise append settings changes relative to the reference baseline;
   - persist a `RolloutItem::TurnContext` even when no model-visible update was required;
   - update both the live reference baseline and, currently, `previous_turn_settings`.
5. Resolve skills, connectors, and dependencies.
6. Record the explicit user prompt through `record_user_prompt_and_emit_turn_item`.
7. Record skill-injection items.
8. Start the branch-specific ghost snapshot.
9. Enter the sampling loop.

The durable ordering for an ordinary turn is therefore approximately:

```text
[context-update ResponseItem ...]
TurnContext
user ResponseItem
UserMessage event
[skill/context ResponseItem ...]
[model item, tool output, model item ...]
```

`TurnStarted` and `TurnComplete` are not durable boundaries in this branch.

### Current early-commit gap

Step 4 happens before dependency resolution and before the explicit user item is durable. A cancellation in that interval can leave a `TurnContext` with no real user boundary. Live state also advances `previous_turn_settings` too early. The backport proposal corrects this without reordering model-visible context items.

## 3. Multi-round sampling within one turn

Each sampling iteration builds a request from the complete live history:

```rust
sess.prompt_history(turn_context.as_ref()).await
```

Completed model response items and completed tool outputs are appended to history. The next request therefore contains:

```text
all prior turns
+ current user input
+ model response items completed so far
+ tool outputs completed so far
+ any pending input admitted at the current boundary
```

The loop terminates when the model no longer requires follow-up and no other follow-up source remains.

### Pending input ordering

This branch already has the important upstream ordering behavior:

- a fresh explicit turn initializes `can_drain_pending_input` to `false`, so the explicit input is sampled first;
- a continuation initializes it to `true`, because there is no new explicit user item;
- after a successful response, pending input may be drained before the next request;
- during preserved-work-notes capture, pending input is deferred;
- after mid-turn compaction, pending input remains deferred while model/tool continuation still needs the next request.

No additional pending-input backport is needed.

## 4. `/pause` and `/continue`

`continue_turn` calls the same inner loop with an empty input and a `PendingContinuation`.

Before sampling it:

1. optionally removes the trailing interrupted marker;
2. trims an incomplete dangling tool-call tail from in-memory history;
3. applies equivalent cleanup to the rollout;
4. emits `TurnContinued`;
5. deliberately skips context update injection, a new user item, and new skill injection.

Continuation resumes from the last durable model-visible boundary. It does not create a synthetic user turn and must not create a new metadata checkpoint during replay.

What survives a pause:

- completed `ResponseItem`s already recorded in history;
- completed tool outputs;
- the real user boundary and its committed context metadata;
- pending-continuation metadata reconstructed from an interrupted abort or an incomplete history tail.

What does not survive as a resumable execution object:

- partial stream deltas that never became a completed `ResponseItem`;
- an in-flight process or tool future as a live OS/runtime object;
- a dangling call tail that continuation cleanup removes before resampling.

## 5. Preservation at each layer

`record_conversation_items` sends the same input slice down three different paths in this order:

```text
record_into_history(items)             // processed live copy
persist_rollout_response_items(items)  // original input items
send_raw_response_items(items)         // original input items
```

That ordering creates an important distinction between live history and durable lineage.

### 5.1 Live raw history

`ContextManager::record_items` accepts model/API items plus the branch-specific `GhostSnapshot`.

|Item|Live raw-history behavior|
|---|---|
|Non-system `Message`|Cloned exactly at record time|
|`Reasoning`|Cloned exactly, including encrypted content|
|`FunctionCall`, `CustomToolCall`, `LocalShellCall`, `WebSearchCall`, `Compaction`|Cloned exactly|
|`FunctionCallOutput`|Recorded after text/content truncation using the turn policy multiplied by `1.2`|
|`CustomToolCallOutput`|Recorded after output-text truncation using the same serialization budget|
|`GhostSnapshot`|Retained specially even though it is not an API message|
|System-role `Message`|Dropped|
|`ResponseItem::Other`|Dropped|

Thus, later requests in the same live session see the processed/truncated output.

### 5.2 Normal rollout records and resume

`persist_rollout_response_items` clones the original `ResponseItem`s supplied to `record_conversation_items`; it does not serialize the processed copies held by `ContextManager`.

|Item|Normal rollout behavior|Reconstruction behavior|
|---|---|---|
|Persistable non-output item|Original item is stored|Recorded into live history under normal acceptance rules|
|Function/custom tool output|Original, pre-truncation item is stored|Truncated again using the resume turn's active truncation policy|
|System-role `Message` supplied through this path|Stored because rollout policy persists all messages|Dropped again by `ContextManager::record_items`|
|`ResponseItem::Other`|Filtered by rollout policy|Never reconstructed|
|`GhostSnapshot`|Stored|Restored to raw history and omitted from model prompts|

Consequences:

- output truncation is stable for the remainder of one live session;
- a normal resume reconstructs outputs from the original rollout item and may produce a different historical item if the active truncation policy changed;
- the upstream branch retains this replay behavior, so changing it is not part of the minimal upstream-alignment proposal.

### 5.3 Compaction replacement checkpoints

A `CompactedItem.replacement_history` is different from normal item-by-item rollout lineage. It stores the already constructed live replacement vector, and reconstruction installs it with `ContextManager::replace` without reprocessing or retruncating its items.

The checkpoint therefore preserves that replacement vector exactly, including any output truncation, summary selection, preserved work notes, and ghost snapshots already present when compaction completed. Older rollout records remain on disk but no longer contribute to later reconstructed prompts before the checkpoint.

## 6. What prompt preparation changes or omits

Prompt construction clones raw history and applies transformations to the clone. Except for invalid-image recovery, these transformations do not rewrite raw history.

|Transformation|Prompt effect|Raw-history effect|
|---|---|---|
|Missing output after function/custom/shell call|Insert synthetic output with `"aborted"`|None|
|Output whose matching call is absent|Remove orphan output|None|
|Model lacks image modality|Replace message/tool images with a fixed text placeholder|None|
|`GhostSnapshot`|Remove from prompt|None|

This means exact preservation must be stated at the correct layer:

- most accepted items are exact in live raw history;
- live tool outputs may already be truncated;
- normal rollout lineage retains original tool outputs and retruncates them on reconstruction;
- compacted replacement checkpoints retain their already processed vector;
- the prompt is a normalized projection, not a byte-for-byte copy of raw history.

## 7. Destructive history operations

These operations intentionally stop carrying the full prior transcript forward:

### Compaction

`core/src/compact.rs` builds replacement history from selected historical user messages plus a generated summary. Depending on `InitialContextInjection`, it may insert current initial context before the last real user message. Preserved work notes and ghost snapshots are then appended. `CompactedItem.replacement_history` is the canonical durable checkpoint.

Everything omitted from replacement history is no longer part of later prompts, even though older rollout records may still exist before the checkpoint.

### Rollback

`ContextManager::drop_last_n_user_turns`:

- identifies a boundary as every user-role `ResponseItem::Message` that is not recognized as contextual;
- excludes user instructions, skill instructions, session prefixes, shell-command wrappers, and preserved work notes;
- counts admitted pending/steer user messages independently because each is another non-contextual user `ResponseItem`;
- also counts synthetic user-role records not covered by the contextual predicate, including compacted summary messages and retained turn-aborted markers;
- truncates from the selected boundary;
- walks backward over contiguous contextual user/developer update messages immediately preceding that boundary.

This per-message boundary is the existing rollback unit in this branch. The current rollout reconstruction then truncates its `context_stack` by the same numeric count, independently of which boundaries survived. That independent counting is the main replay bug addressed by this proposal.

### Continuation cleanup

`prepare_history_for_continuation` may remove a trailing interrupted marker and incomplete tool tail. `RolloutRecorder::clean_for_continue` mirrors the durable cleanup.

### Invalid-image recovery

`replace_last_turn_images` rewrites the most recent eligible function output image to text. Its current reverse scan stops at any user-role message, including contextual user messages. Upstream stops at a real user-turn boundary; that narrower fix is included in the backport.

The rewrite currently changes only live raw history. It does not amend the already persisted original `ResponseItem`, so a restart before a later replacement checkpoint can reconstruct the original image and encounter the same provider rejection again. Upstream does not provide a directly backportable durability fix in the inspected path; changing rollout rewrite semantics is outside the mandatory proposal.

## 8. Token accounting

The branch maintains per-item local estimates and combines them with server usage:

- the server's last reported total is the base;
- locally recorded items after the last model-generated item are added;
- historical encrypted reasoning estimates are additionally included when the server did not account for them;
- inline base64 image payload bytes are replaced by a fixed model-visible image estimate rather than counted as text.

The post-model-tail accounting is already present and does not need backport work.

## 9. Prefix-cache and WebSocket reuse

Two mechanisms should be kept distinct.

### Server prompt-prefix cache

Requests carry `prompt_cache_key = conversation_id`. Across turns, the client sends the complete prompt history. Appending context diffs, a user message, and later response/tool items while leaving the old history unchanged preserves the old request as a structural prefix of the new request.

This is the cache-friendly path described in [Unrolling the Codex agent loop](https://openai.com/index/unrolling-the-codex-agent-loop/): prior conversation items are included in later requests, and an exact old prefix can be reused.

### Turn-scoped WebSocket continuation

A single `ModelClientSession` is reused across retries and sampling rounds within one logical turn. It sends only an incremental suffix with `previous_response_id` when both conditions hold:

1. the new input begins with the previous input followed by the server-returned items;
2. non-input request properties are exactly equal.

Compared properties include model, instructions, tools, tool choice, parallel-call setting, reasoning, store, stream, include, service tier, prompt-cache key, and text/output configuration.

A fresh `ModelClientSession` is created for the next logical turn, so this `previous_response_id` optimization is primarily within-turn in this branch. Server-side prefix caching can still span turns through the stable prompt-cache key and append-only input.

### Operations and cache impact

|Operation|Prefix effect|
|---|---|
|Append a normal user turn or completed tool round|Preserves the previous prompt prefix|
|Append only settings differences|Preserves the previous prompt prefix and minimizes suffix growth|
|Live output truncation|Cache-friendly for the remainder of that live session because the processed item is stable|
|Resume with a different truncation policy|Can change an old output reconstructed from original rollout lineage and therefore break the exact historical prefix|
|Replacement-history checkpoint|Preserves the checkpoint vector exactly on reconstruction; establishes a new durable base|
|Prompt normalization with unchanged raw tail|Usually deterministic; a newly completed call/output can change normalization only near the tail|
|Compaction|Replaces history; invalidates continuity from the rewrite point|
|Rollback|Truncates history; invalidates continuation from the latest request, although an older prefix may still be cached|
|Continuation cleanup|Rewrites the tail and clears direct continuation assumptions|
|Invalid-image sanitization|Rewrites an earlier item in the current turn|
|Switch model/tools/instructions/reasoning/schema/service tier|May preserve input history but disables WebSocket incremental reuse because request properties differ|
|Unnecessary full context reinjection|Keeps older bytes but grows a redundant suffix and reduces effective cache/context efficiency|

The proposed replay fixes primarily improve cache behavior indirectly: they prevent stale metadata from causing duplicate context reinjection and ensure rollback/resume choose the same surviving baseline as the history they reconstruct.
