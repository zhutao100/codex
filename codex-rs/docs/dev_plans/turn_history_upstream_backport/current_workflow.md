# Current Agent-Loop and History Workflow

## 1. State model

This branch has four related but non-identical representations.

|Representation|Owner|Purpose|
|---|---|---|
|Raw conversation history|`ContextManager.items` in `core/src/context_manager/history.rs`|Canonical in-memory `ResponseItem` transcript used for later turns.|
|Prompt projection|`ContextManager::prepare_items_for_prompt_with_modalities`|A cloned, normalized, modality-compatible request input.|
|Rollout log|`RolloutItem` records filtered by `core/src/rollout/policy.rs`|Durable resume/fork/rollback source.|
|Turn metadata|`reference_context_item`, `previous_turn_settings`, `pending_continuation`|Context-diff baseline, latest committed user-turn settings, and branch-local continuation state.|

These representations intentionally differ. A raw item can be retained but omitted from the prompt, or persisted in an untruncated form and reconstructed into a truncated live form.

## 2. Regular turn sequence

`run_turn` rejects empty explicit input and calls `run_turn_inner`. The important ordering in `core/src/session/turn.rs` is:

1. Emit `TurnStarted`. The event is delivered but filtered out of the rollout in this branch.
2. If the model changed and the current context is too large for the new model, optionally compact using the previous model.
3. If the current history already exceeds the automatic compaction limit, run pre-turn compaction. The branch-local work-notes path may first ask the model to produce preserved notes.
4. Call `record_context_updates_and_set_reference_context_item`.
   - Append a full initial context when no reference baseline exists.
   - Otherwise append settings/context deltas, or a full fallback bundle for older baseline shapes.
   - Persist `RolloutItem::TurnContext`.
   - Set the live reference baseline.
   - Do **not** update `previous_turn_settings`.
5. Resolve skills, connectors, and dependencies.
6. Record the explicit user `ResponseItem` in history and rollout.
7. Commit `previous_turn_settings` to the current model.
8. Emit the user turn item events; `EventMsg::UserMessage` is persisted by the rollout policy.
9. Append branch-local skill/context injections, if any.
10. Optionally start a `GhostSnapshot` for undo.
11. Enter the multi-round sampling loop.

The ordering separates a candidate context baseline from a committed real user turn. A failure after step 4 but before step 6 can leave a durable `TurnContext` with no corresponding real user boundary; this is the central replay association problem.

## 3. Multi-round sampling within one logical turn

For every sampling round, `run_turn_inner` calls `sess.prompt_history(turn_context)` unless it is in the special pre-compaction work-notes capture state. `prompt_history` clones the current raw history and applies prompt projection. Therefore each round includes everything durably completed before it:

```text
initial/context items
real user input
model reasoning/message/tool call
completed tool output
queued steering input, when drained
next model round
...
```

Completed model items are recorded as stream items complete. Tool calls are persisted before execution; their outputs are persisted after the tool future resolves. A follow-up model request therefore sees the completed call/output pair.

Partial stream deltas that never become a completed `ResponseItem` are not conversation history. An interrupted process or stream can therefore preserve completed items while losing an incomplete assistant text delta, a live tool future, or unrecorded process state.

### Fresh input versus queued steering

For a new explicit turn, `can_drain_pending_input` starts as `false`. The explicit input is recorded and sampled before pending input is drained. After a successful sampling round it becomes `true`, and queued steering is appended before a later sampling round.

For `/continue`, explicit input is empty and `can_drain_pending_input` starts as `true`. Continuation first cleans the interrupted tail and then permits queued input to be appended.

### Client-session scope

`ModelClientSession` is reused across retries and rounds within one logical turn. It is not reused across separate explicit user turns. This distinction matters for the WebSocket `previous_response_id` optimization described below.

## 4. Raw-history admission and mutation

`ContextManager::record_items` processes oldest-to-newest and admits the following shapes.

|Item|Raw-history behavior|Rollout behavior|Prompt behavior|
|---|---|---|---|
|Non-system `Message`|Preserved exactly|Persisted|Sent, subject to image stripping and later compaction/rollback.|
|System-role `Message`|Dropped|A directly persisted system response item would be filtered by live admission on replay|Not sent from history. Base instructions use a separate request field.|
|`Reasoning`|Preserved|Persisted|Sent. Encrypted content participates in token estimates.|
|Function/custom/local-shell call|Preserved|Persisted|Sent; missing outputs are synthesized in the prompt clone.|
|Function/custom tool output|Truncated at live admission using the turn truncation policy plus serialization headroom|The original item passed to `record_conversation_items` is persisted|Reconstructed sessions reapply the resume-time truncation policy.|
|`WebSearchCall`|Preserved|Persisted|Sent.|
|`Compaction` response item|Preserved|Persisted|Sent when present as an API item. Distinct from `RolloutItem::Compacted`.|
|`GhostSnapshot`|Preserved as a branch-local exception|Persisted|Removed from the prompt clone.|
|`Other`|Dropped|Not persisted|Not sent.|

### Mutating operations

The raw vector is append-only during ordinary turns, but the following operations rewrite it:

- local or remote compaction calls `replace_compacted_history`;
- thread rollback truncates at a real user boundary and removes immediately preceding contextual update items;
- invalid-image recovery replaces images in the latest tool output within the latest real user turn;
- local/remote compaction prompt fitting can remove old or trailing items from a cloned compaction source;
- `/continue` can remove an interrupted abort marker and trim an incomplete dangling-call tail.

## 5. Prompt projection

`ContextManager::prepare_items_for_prompt_with_modalities` operates on a clone. It does not normally rewrite raw history.

The projection applies these deterministic transformations:

1. Insert synthetic function/custom-tool outputs containing `"aborted"` immediately after calls that have no output.
2. Remove outputs whose matching call is absent.
3. When the selected model lacks image input, replace message and tool-output images with a text placeholder.
4. Remove every `GhostSnapshot`.

Consequences:

- raw history and model-visible history can differ;
- a dangling call may remain in raw history while every request sees a synthetic completed pair;
- switching between image-capable and text-only models can change the projected historical prefix even without mutating raw history;
- prompt projection is stable for the same raw items and model modalities.

The normal agent loop waits for a tool output before issuing a follow-up request. Synthetic outputs primarily protect interrupted, resumed, or malformed tails.

## 6. Cross-turn context carryover

Two metadata values serve different purposes.

### `reference_context_item`

This is the baseline for context-diff generation. Before a new user item, `record_context_updates_and_set_reference_context_item` compares the current `TurnContext` with this baseline and appends only the required update items when possible.

A compaction using `InitialContextInjection::DoNotInject` clears this baseline because the replacement no longer contains canonical current-context items. A mid-turn compaction using `BeforeLastUserMessage` reinserts canonical context immediately before the last real user message and sets a new baseline.

### `previous_turn_settings`

This represents the newest committed real user turn. In this branch it currently contains the model slug. It is committed only after the user `ResponseItem` is recorded. Previous-model compaction reads it to decide whether an oversized history should first be compacted by the model that produced the prior conversation.

Live compaction does not clear it. The replay implementation currently derives it from the last replayed `TurnContext`, which is not equivalent and causes confirmed resume/rollback defects.

## 7. Compaction preservation semantics

Compaction is an intentional cache and history rebase. It does not preserve the detailed transcript token-for-token.

### Local compaction

`core/src/compact.rs` builds a replacement from:

- selected recent real user messages and persisted turn-abort markers, bounded by `COMPACT_USER_MESSAGE_MAX_TOKENS`;
- a generated summary represented as a user message;
- optional current canonical context inserted before the last real user message for mid-turn compaction;
- optional preserved session work notes;
- every `GhostSnapshot`, so `/undo` remains available.

It omits detailed assistant messages, reasoning, tool calls, and tool outputs except insofar as the summary carries their meaning. New compactions persist the exact replacement in `CompactedItem.replacement_history`.

The local compaction request itself uses a cloned source history plus a synthetic compaction prompt. If the request exceeds the model window, the implementation removes oldest items from that cloned source until it fits. The source comment says this preserves a prefix cache, but removing the oldest item necessarily changes the prefix; the operative goal is keeping recent history, not preserving the prior exact prefix.

### Remote compaction

`core/src/compact_remote.rs` sends the projected history to the compact endpoint and receives a replacement history. It then optionally injects canonical context, preserved work notes, and `GhostSnapshot` items, and persists the exact resulting replacement.

### Replay

When `replacement_history` is present, replay can reproduce the compacted raw history exactly. For legacy compactions where it is absent, this branch currently rebuilds the summary using resume-time initial context; that historical reinjection is one selected backport defect.

## 8. Pause and continuation

`/pause` and `/continue` are branch-local features. Lifecycle events for them are not persisted by `core/src/rollout/policy.rs`.

Continuation does not create a synthetic user message and does not repeat normal-turn context, skill, or connector injection. It:

1. optionally removes the trailing interrupted abort marker;
2. trims the incomplete history tail beginning at the first dangling call after the latest real user boundary;
3. applies equivalent cleanup to the rollout;
4. emits transient `TurnContinued` state;
5. resumes sampling from the remaining completed history.

It preserves completed assistant/reasoning/call/output items. It cannot preserve partial text deltas, an in-flight tool future, a child process's live state, or an unpersisted continuation event.

`history_needs_continuation` excludes contextual user/developer items when examining the tail. However, a compacted replacement normally ends in a summary encoded as a real user message. Without compaction provenance, a standalone compaction can therefore be misclassified as an unfinished regular turn after replay.

## 9. Rollback behavior

`ContextManager::drop_last_n_user_turns` counts only `is_user_turn_boundary` messages:

```text
role == "user" && content is not contextual state
```

If all real user turns are removed, it preserves any prefix that existed before the first real user boundary. For a partial rollback it also walks backward from the cut and removes contiguous contextual developer/user update items attached to the rolled-back turn.

Current rollout reconstruction applies the same numeric rollback independently to a `Vec<TurnContextItem>`. Since bare context records and compaction tasks can add `TurnContext` without adding a real user boundary, history and metadata can select different surviving turns.

## 10. Token accounting

The history tracks a per-item estimate and a server-reported token snapshot.

The current total is approximated as:

```text
latest server total
+ locally estimated items after the last model-generated item
+ older encrypted reasoning estimates when the server total did not include them
```

Inline base64 image transport bytes are replaced with a fixed model-visible image estimate rather than counted as text. `GhostSnapshot` contributes zero estimated tokens because it is removed before prompting.

These estimates guide auto-compaction but are not tokenizer-exact.

## 11. Prefix-cache and WebSocket effects

There are two separate reuse mechanisms.

### Server-side prompt prefix cache

Every request uses the conversation ID as `prompt_cache_key`. The key can help route related requests, but reusable content still depends on an exact token prefix.

Ordinary turns are deliberately append-oriented:

```text
request N input
+ model output/tool output
+ context deltas
+ next user input
```

`core/tests/suite/prompt_caching.rs` verifies that later request input preserves earlier input as a prefix and that settings changes are appended rather than rewritten.

### WebSocket incremental continuation

Within a turn, `ModelClientSession::get_incremental_items` sends only a suffix with `previous_response_id` when:

- non-input request properties are unchanged;
- the new input begins with the previous request input;
- the remaining suffix begins with items returned by the previous server response.

This is stricter than server prefix caching and is turn-scoped. Compaction explicitly clears WebSocket continuation state.

### Operation matrix

|Operation|Raw-history effect|Server prefix-cache effect|WebSocket continuation effect|
|---|---|---|---|
|Normal completed item|Append|Preserves prior prefix|Eligible when request properties and returned-item ordering match.|
|Context/settings delta|Append|Preserves prior prefix|May be eligible in the same turn; across explicit turns a new client session is used.|
|Queued steering|Append|Preserves prior prefix|Eligible after committed server output.|
|Prompt-only normalization|Raw unchanged; projected suffix/pairs may differ|Stable when raw history and modalities are unchanged|Must still satisfy exact projected input checks.|
|Compaction|Replace|Old prefix is lost; replacement becomes a new cache base|Cleared.|
|Rollback|Truncate/rewrite|Reuses only whatever surviving prefix still exactly matches a prior request|Prior continuation cannot be assumed.|
|Invalid-image recovery|Mutate latest tool output|Breaks the prefix at the mutated item|Incremental check fails until a new base is established.|
|Image-capability switch|Raw unchanged; old projected images change|Breaks projected prefix at the first affected image|Request-property/modal input mismatch prevents continuation.|
|Model/tool/instruction/schema change|Input may remain append-only|Input prefix may still cache, but non-input request changes can reduce overall reuse|Rejected by the incremental request-property comparison.|

Replay metadata bugs can indirectly reduce cache reuse by causing unnecessary full-context reinjection or incorrect model-switch updates. They are first correctness defects; cache loss is a secondary symptom.
