I traced the current path from `core/src/codex.rs:3365-3599` outward, and the cleanest drop-in design is:

* keep the existing `run_turn` loop and `client_session`
* add **one extra ordinary sampling round** right before compaction
* make that round emit **structured work notes**
* then compact using the **original session history** as the compaction source
* inject the work notes **verbatim** into the post-compaction history using existing `replacement_history` support

That gives you the “one more round in the same session” behavior without changing tools/base instructions/output schema for that round, so the unchanged prompt prefix remains cacheable.

## What `run_turn` is doing now

From `core/src/codex.rs:3365-3599`, the relevant control flow is:

1. `run_turn(...)` starts, emits `TurnStarted`, and may compact immediately if the session is already over `auto_compact_limit` before the new turn starts (`3384-3386`).
2. It resolves skills/apps/dependencies, records the incoming user input into session history, maybe injects skill items, then creates one `client_session` for the turn (`3464-3484`).
3. Inside the main loop:

   * pending user input is appended (`3489-3500`)
   * prompt input is built from `sess.clone_history().await.for_prompt()` (`3497-3501`)
   * `run_sampling_request(...)` is invoked (`3515-3524`)
4. `run_sampling_request(...)`:

   * lists MCP tools
   * filters connectors
   * builds `ToolRouter`
   * builds `Prompt { input, tools, parallel_tool_calls, base_instructions, personality, output_schema }` (`3723-3770`)
   * calls `try_run_sampling_request(...)`
5. `try_run_sampling_request(...)` streams response events (`4256+`):

   * tool calls go through `handle_output_item_done(...)`, get recorded immediately, and set `needs_follow_up = true`
   * assistant messages get recorded and update `last_agent_message`
   * `Completed` updates token usage and returns `SamplingRequestResult { needs_follow_up, last_agent_message }` (`4425-4441`)
6. Back in `run_turn`, after each sampling request:

   * it recomputes token usage (`3531-3545`)
   * if `token_limit_reached && needs_follow_up`, it calls `run_auto_compact(...)` and continues (`3548-3550`)
   * otherwise it either finishes the turn or loops again

So the current auto-compact trigger is exactly the branch you pointed at.

## Why the current compaction is lossy in the cases you care about

### Local compaction

`run_auto_compact()` dispatches to `run_inline_auto_compact_task()` unless remote compaction is enabled (`3600-3605`).

Local compaction in `core/src/compact.rs`:

* synthesizes a compact prompt (`43-55`)
* runs a separate compacting model pass
* then rebuilds history as:

  * initial context
  * a token-limited subset of prior **user** messages
  * one summary user message (`177-205`, `267-333`)

Key consequence: assistant/tool-derived operational state only survives if the compaction summary restates it well.

That is exactly where you lose:

* validated vs ruled-out hypotheses
* file relevance/irrelevance findings
* “don’t reread these files”
* in-progress edit state

### Remote compaction

`core/src/compact_remote.rs` sends the full prompt history to `compact_conversation_history(...)` and replaces history with the returned transcript (`70-99`).

That preserves more structure than local compaction, but it is still an opaque compression step. It does not guarantee verbatim preservation of the specific “loss traps” you care about.

## Proposed design

### Core idea

When this branch would currently compact:

```rust
if token_limit_reached && needs_follow_up {
    run_auto_compact(&sess, &turn_context).await;
    continue;
}
```

change the behavior to:

1. **first** run one extra sampling round in the same `run_turn` loop
2. that round asks the model to emit **session work notes only**
3. capture those notes
4. compact
5. inject the notes **unchanged** into the new session history
6. continue the original work

### Why this preserves prefix cache

The important detail is that the note-generation round should be just another ordinary `run_sampling_request(...)` iteration using the same:

* `client_session`
* `ToolRouter` / tool specs
* `base_instructions`
* `personality`
* `output_schema`

The only prompt change is appending **one synthetic suffix item** to history before the next loop iteration.

That means the big historical prefix remains identical, so prefix caching can still hit on the entire existing transcript prefix. You are not changing the tool list, not creating a special no-tools prompt, and not swapping instruction blocks.

## Concrete patch shape

## 1) Add a tiny state machine in `run_turn`

Add turn-local state, conceptually:

```rust
enum PreCompactNotesState {
    Idle,
    AwaitingNotes,
}
```

and a slot for captured notes:

```rust
let mut pre_compact_notes_state = PreCompactNotesState::Idle;
let mut preserved_work_notes: Option<String> = None;
```

Then reshape the post-sampling branch like this:

```rust
if matches!(pre_compact_notes_state, PreCompactNotesState::AwaitingNotes) {
    preserved_work_notes = sampling_request_last_agent_message;
    run_auto_compact(&sess, &turn_context, preserved_work_notes.take()).await;
    pre_compact_notes_state = PreCompactNotesState::Idle;
    continue;
}

if token_limit_reached && needs_follow_up {
    inject_pre_compact_notes_request(&sess, &turn_context).await;
    pre_compact_notes_state = PreCompactNotesState::AwaitingNotes;
    continue;
}
```

### Important behavioral detail

The note round is not the end of the user-visible turn. After the notes are produced, you compact immediately and continue the loop.

That keeps the original task alive.

## 2) Inject the notes request as a synthetic suffix item

Use a **developer** message, not a user message.

Why:

* it is an internal orchestration instruction
* it should not count as preserved user content
* `collect_user_messages(...)` already ignores developer messages

Something like:

```text
<AUTO_COMPACT_WORK_NOTES_REQUEST>
Token limit is approaching and follow-up work is still needed.

Respond with exactly one assistant message and do not call tools.

Produce structured SESSION WORK NOTES for the next model after compaction.
Prioritize:
- current objective
- validated findings
- ruled-out hypotheses / dead ends
- remaining open hypotheses
- relevant files inspected and why
- irrelevant files inspected and why they can be skipped
- edits made / edits in progress
- next concrete step

Keep it concise but loss-resistant.
Begin with: <AUTO_COMPACT_WORK_NOTES>
</AUTO_COMPACT_WORK_NOTES_REQUEST>
```

Use a sentinel marker so the code can recognize the transient orchestration suffix later.

I would record this into history and rollout, but not treat it as durable user content.

## 3) Change `run_auto_compact(...)` to accept optional preserved notes

Current signature:

```rust
async fn run_auto_compact(sess: &Arc<Session>, turn_context: &Arc<TurnContext>)
```

Proposed:

```rust
async fn run_auto_compact(
    sess: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
    preserved_work_notes: Option<String>,
)
```

Only two call sites change:

* the early pre-turn compact at `3384-3386` passes `None`
* the mid-turn post-note compact passes `Some(notes)` or `None`

That is a very small, low-conflict signature change.

## 4) Strip the transient note-request round from the compaction source history

This is the subtle but important part.

If you simply compact the live history after the note round, the compaction source transcript now includes:

* the synthetic developer request
* the assistant work-notes response

That is undesirable because:

* the request is orchestration noise
* the notes themselves are supposed to be preserved **verbatim**, not re-compressed by the compactor

So before local or remote compaction, build a **prepared compaction source history**:

* start from `sess.clone_history().await`
* find the last synthetic `<AUTO_COMPACT_WORK_NOTES_REQUEST>` marker
* if present at the end-of-session suffix, drop that item and everything after it from the compaction source
* separately retain `preserved_work_notes`

Conceptually:

```rust
struct PreparedCompactionInput {
    source_history: ContextManager,
    preserved_work_notes: Option<String>,
}
```

This helper should live in `compact.rs`, because both local and remote compaction need it.

## 5) Inject preserved work notes into the new history as a durable message

Use an explicit user message with a prefix like:

```text
Immediately before compaction, the previous model emitted the following preserved session work notes.
These notes are verbatim and are intended to prevent duplicate work and repeated dead ends:

<AUTO_COMPACT_WORK_NOTES>
...
```

Why user message here:

* current local compaction already uses a user summary message as the continuation anchor
* putting the notes into history as a user message makes them high-salience for the next model turn
* it avoids needing any protocol change

### Do not compress these notes

They should bypass the normal `COMPACT_USER_MESSAGE_MAX_TOKENS` budget. The note prompt itself should keep them small enough.

## 6) Exclude preserved notes from future “user message” selection

`collect_user_messages(...)` in `compact.rs:227-241` already filters summary messages via `is_summary_message(...)`.

Extend that same pattern to filter **preserved work notes messages** too.

Otherwise repeated compactions will duplicate the preserved notes as ordinary user content.

So add something like:

```rust
fn is_preserved_work_notes_message(message: &str) -> bool
```

and filter it in `collect_user_messages(...)`.

That gives you a nice repeated-compaction behavior:

* ordinary summary stays summary
* preserved notes stay a single durable handoff block
* user content selection remains about actual user turns

## Local compaction changes

Current local compaction builds the new history from:

* `collect_user_messages(history_items)`
* `summary_text`

Proposed local behavior:

1. Prepare compaction source history with the transient suffix removed.
2. Run the compacting model pass from that prepared source history.
3. Build:

   * `summary_text` from the compaction model output
   * `user_messages` from the prepared source history
   * `preserved_work_notes` from the captured note round
4. Build new history as:

```text
initial_context
+ selected real user messages
+ compact summary message
+ preserved work notes message (if any)
+ ghost snapshots
```

### One more important implementation detail

Today local compaction persists:

```rust
CompactedItem {
    message: summary_text.clone(),
    replacement_history: None,
}
```

That is not enough once preserved notes are injected.

Why: `reconstruct_history_from_rollout(...)` rebuilds local compactions from `message` only when `replacement_history` is `None` (`core/src/codex.rs:1769-1780`). That rebuild path has no way to reproduce verbatim preserved notes.

So for compactions that inject preserved work notes, local compaction should persist:

```rust
CompactedItem {
    message: summary_text.clone(),
    replacement_history: Some(new_history.clone()),
}
```

Use `replacement_history` only on the new notes-preserving path if you want to keep the behavioral delta narrow.

## Remote compaction changes

Remote compaction is even simpler.

Current path:

* prepare prompt from cloned history
* call `compact_conversation_history(...)`
* append ghost snapshots
* replace history
* persist `replacement_history: Some(new_history)`

Proposed path:

1. Prepare compaction source history with the transient note suffix removed.
2. Call remote compaction on that source history.
3. Append preserved work notes message **after** the remote compacted transcript.
4. Append ghost snapshots.
5. Persist the exact resulting history in `replacement_history`.

Because remote compaction already uses `replacement_history`, this is a very small change.

## Recommended note schema

The work notes round should be optimized for the exact loss modes you described, not for generic “summary.”

I would require these sections:

* `Objective`
* `Current status`
* `Validated findings`
* `Ruled-out hypotheses / dead ends`
* `Open hypotheses / unresolved questions`
* `Relevant files / why`
* `Irrelevant files / why skip`
* `Edits made`
* `Edits in progress / intended edits`
* `Next best step`

This is much better than a vague prose summary because it explicitly captures:

* what not to repeat
* what not to reread
* what is still unresolved

## Failure handling

This should degrade cleanly.

### If the note round misbehaves

If the model calls tools instead of emitting notes:

* do **not** keep looping on the note round
* compact immediately
* preferably discard the transient note-request suffix from compaction input
* proceed without preserved notes

### If the note round errors

If `AwaitingNotes` is active and the extra sampling round hits:

* `ContextWindowExceeded`
* stream retry exhaustion
* similar non-abort errors

then compact immediately with `preserved_work_notes = None` and continue.

That preserves availability and prevents the turn from dying just because the optional note round failed.

### If the user interrupts during note generation

Best-effort behavior is fine, but the safest policy is:

* if pending user input appears before the note round completes cleanly, skip notes and compact immediately

That avoids mixing a synthetic orchestration round with a real user interruption.

## Why this is low-conflict for future rebases

This is the main reason I like this shape.

It avoids:

* protocol/schema expansion
* new task types
* new model endpoints
* invasive changes to `run_sampling_request(...)` / `try_run_sampling_request(...)`
* any changes to tool selection or prompt construction semantics

The changes stay localized to:

* `core/src/codex.rs`

  * tiny turn-local state in `run_turn`
  * one new helper to inject the synthetic note request
  * `run_auto_compact(...)` signature change
* `core/src/compact.rs`

  * helper(s) for note request markers / preserved note message
  * filter logic in `collect_user_messages(...)`
  * prepared compaction source helper
  * local history rebuild path with optional preserved notes
* `core/src/compact_remote.rs`

  * apply the same prepared-history helper
  * append preserved notes to returned history

That is exactly the kind of drop-in delta that is likely to rebase cleanly over upstream churn.

## Suggested pseudo-diff structure

### In `core/src/codex.rs`

* add `PreCompactNotesState`
* add `inject_pre_compact_notes_request(...)`
* modify the `3548` branch into two-phase logic
* on note-round completion, call `run_auto_compact(..., notes)`

### In `core/src/compact.rs`

* add constants / sentinel helpers
* add `is_preserved_work_notes_message(...)`
* extend `collect_user_messages(...)` to ignore preserved notes
* add `prepare_history_for_compaction(...)`
* extend `build_compacted_history(...)` to accept optional preserved notes item
* persist `replacement_history` when preserved notes are injected

### In `core/src/compact_remote.rs`

* build prompt from prepared source history
* append preserved notes message after remote compact result
* keep using `replacement_history: Some(...)`

## Bottom line

The best design is **not** “change compaction to summarize better.”

It is:

* **before compaction**, do one extra ordinary model round in the same turn/session
* make that round emit **structured work notes**
* compact from the **pre-note** source history
* inject the notes **verbatim** into the post-compaction transcript
* continue normally

That directly addresses your two failure modes:

* repeated hypothesis churn
* repeated file re-reading / missed file relevance state

while keeping the patch narrowly scoped and rebase-friendly.
