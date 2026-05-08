# Design Proposal

## Status

Implemented with rollout learnings incorporated into the design: pre-turn auto-compaction must also capture notes, and pending input must be deferred until after compaction so notes capture is not short-circuited.

## Target Base

This proposal targets the `fe8b474acd43cb8894d661944c6b5c8db0ef0ad1` code shape.

The goal is to preserve high-value session state across auto-compaction without changing the prompt prefix surface more than necessary, and to do so in a way that is easy to rebase onto future upstream changes.

## Design Summary

- Keep the existing `run_turn(...)` loop and turn-scoped `ModelClientSession`.
- Add one extra ordinary sampling round immediately before auto-compaction.
- Use that round to emit structured session work notes.
- Keep the prompt tool list unchanged, but reject tool execution at runtime during the note round.
- Compact from prepared history that excludes the transient work-notes request/response suffix.
- Inject preserved work notes verbatim after compaction.
- Reuse the same prepared-history logic for both local and remote compaction.
- Also capture notes before pre-turn auto-compaction when a turn starts over the limit (unless schema constraints force a safe fallback).

That preserves the original design principles:

- minimize compaction loss for working state, not just final answers.
- preserve cache-friendly prompt prefixes.
- avoid tool side effects in the extra round.
- keep the patch localized and rebase-friendly.

## Baseline Control Flow On `fe8b474...`

### `run_turn(...)`

On this base, the relevant shape is:

1. Start the turn and emit `TurnStarted`.
2. Compact immediately if total usage is already above `auto_compact_limit`.
3. Resolve skills, apps, dependencies, and skill injections.
4. Record the incoming user message into history.
5. Create a single turn-scoped `ModelClientSession` and resolve `turn_metadata_header`.
6. Inside the main loop:
   - drain pending input with `sess.get_pending_input()`
   - replay each drained item into history
   - build prompt input from `sess.clone_history().await.for_prompt()`
   - call `run_sampling_request(...)`
7. After each sampling request:
   - recompute total token usage
   - if `token_limit_reached && needs_follow_up`, compact immediately
   - otherwise either finish the turn or continue

This matters because the feature has to slot into an already stateful loop that owns:

- the current `client_session`
- the current `turn_metadata_header`
- the current pending-input replay path

### Local compaction

Local compaction on this base:

- clones history
- appends the synthetic compact prompt
- runs a summarizing model turn
- rebuilds history from:
  - initial context
  - token-limited collected user messages
  - a single summary user message
  - ghost snapshots
- persists only `message` in `CompactedItem`, unless a replacement history is explicitly provided

### Remote compaction

Remote compaction on this base:

- clones history
- trims codex-generated items against the context window using `base_instructions`
- submits the remaining prompt to `compact_conversation_history(...)`
- appends ghost snapshots
- persists `replacement_history: Some(new_history)`

## Proposed Patch Shape

## 1. Add a tiny note-capture state machine to `run_turn(...)`

Introduce a turn-local state:

```rust
enum PreCompactNotesState {
    Idle,
    AwaitingNotes,
}
```

Use it to split the old "compact immediately" branch into two phases:

1. when `token_limit_reached && needs_follow_up`, inject a synthetic work-notes request and loop once more.
2. when the next assistant message arrives while `AwaitingNotes`, compact immediately using the captured note text.

Conceptually:

```rust
if matches!(pre_compact_notes_state, PreCompactNotesState::AwaitingNotes) {
    if let Some(notes) = sampling_request_last_agent_message
        && notes.trim_start().starts_with("<AUTO_COMPACT_WORK_NOTES>")
    {
        run_auto_compact(&sess, &turn_context, Some(notes)).await;
        pre_compact_notes_state = PreCompactNotesState::Idle;
        continue;
    }

    // If we didn't get a valid notes message, retry capture a small number of times,
    // then compact without notes.
    continue;
}

if token_limit_reached && needs_follow_up {
    if turn_context.final_output_json_schema.is_some() {
        run_auto_compact(&sess, &turn_context, None).await;
        continue;
    }

    inject_pre_compact_work_notes_request(&sess, &turn_context).await;
    pre_compact_notes_state = PreCompactNotesState::AwaitingNotes;
    continue;
}
```

This keeps the extra round inside the same session and the same turn loop.

## 2. Keep the note round cache-friendly

The extra round should reuse the existing prompt surface:

- same `client_session`
- same `turn_metadata_header`
- same tool list in the prompt
- same base instructions
- same personality
- same output schema configuration, except when schema presence forces the feature to be skipped

The only additional prompt content should be one synthetic developer message appended at the end of history.

This is the key design choice that preserves prefix cache reuse for the large historical prefix.

## 3. Inject the work-notes request as a synthetic developer message

Add a helper in `core/src/codex.rs` that records a developer message with sentinel tags:

```text
<AUTO_COMPACT_WORK_NOTES_REQUEST>
Token limit is approaching and follow-up work is still needed.

Respond with exactly one assistant message and do not call tools.
Produce structured SESSION WORK NOTES for the next model after compaction.

Required sections:
- Objective
- Current status
- Validated findings
- Ruled-out hypotheses / dead ends
- Open hypotheses / unresolved questions
- Relevant files / why
- Irrelevant files / why skip
- Edits made
- Edits in progress / intended edits
- Next best step

Keep it concise but loss-resistant.
Begin with: <AUTO_COMPACT_WORK_NOTES>
</AUTO_COMPACT_WORK_NOTES_REQUEST>
```

Why developer message:

- it is orchestration, not user content
- `collect_user_messages(...)` should not treat it as durable user history
- it can later be detected and removed from the compaction source transcript

## 4. Suppress tool execution at runtime, not at prompt construction

The note round should not change the prompt tool list. Instead:

- extend `ToolCallRuntime` with `ToolCallExecutionMode`
- use `Normal` for ordinary sampling rounds
- use `RejectAll { reason }` for the note round

Why runtime rejection is the right place on this base:

- prompt/tool inventory stays unchanged, which preserves prefix cacheability
- tool side effects are prevented even if the model ignores the instruction
- the change stays localized to `core/src/tools/parallel.rs`

The main implementation note for this base:

- `ToolCallRuntime::handle_tool_call(...)` still returns an `impl Future`, not an already-`async fn` value in the baseline
- `FunctionCallOutputPayload` uses `body: FunctionCallOutputBody::Text(...)`, not a flat `content` field

That payload shape is a common rebase drift point and should be checked first on future bases.

## 5. Preserve pending user input if the note round is interrupted

This base drains pending input eagerly via `sess.get_pending_input()`.

That creates a subtle edge case:

- if a real user message arrives while the feature is waiting for the note round to complete, draining and replaying it into the notes round can perturb ordering and can short-circuit notes capture.

The recommended fix is to defer draining pending input while `AwaitingNotes`, so:

- the notes reflect the pre-interruption history
- the system can still capture fresh notes (replacing stale preserved notes)
- pending input is preserved and replayed immediately after compaction when the state returns to `Idle`

This behavior is essential on `fe8b474...` and should be preserved during future rebases as long as pending input is still drained rather than merely peeked.

## 6. Extend `run_auto_compact(...)` to accept optional preserved notes

Change the helper signature to:

```rust
async fn run_auto_compact(
    sess: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
    preserved_work_notes: Option<String>,
)
```

This keeps the control-flow change in `core/src/codex.rs` small:

- pre-turn compaction captures notes when possible, and passes `None` only when schema constraints are present or capture fails
- schema-guarded fallback compaction passes `None`
- successful note capture passes `Some(notes)`

## 7. Prepare compaction source history before both local and remote compaction

Add a shared helper in `core/src/compact.rs`:

```rust
struct PreparedCompactionInput {
    source_history: ContextManager,
    preserved_work_notes: Option<String>,
}
```

Its job is:

- remove the transient developer request suffix identified by `<AUTO_COMPACT_WORK_NOTES_REQUEST>`
- drop any already-injected preserved notes message from the source history
- recover preserved notes from either:
  - freshly captured note text passed in by `run_turn(...)`
  - an existing preserved notes message already present in history

This helper is the right shared abstraction because local and remote compaction need the same source preparation rules.

## 8. Local compaction behavior

After the compacting model round completes:

1. derive `summary_text` as before
2. collect user messages from the prepared source history
3. rebuild new history as:

```text
initial_context
+ selected real user messages
+ compact summary message
+ preserved work notes message (if any)
+ ghost snapshots
```

4. persist `replacement_history: Some(new_history.clone())` when preserved notes are present

Why `replacement_history` matters here:

- on this base, local compaction normally persists only the summary text
- reconstructing from summary text alone cannot faithfully recreate verbatim preserved notes
- `replacement_history` avoids that loss during rollout reconstruction

Also extend `collect_user_messages(...)` to ignore preserved work-notes messages, so repeated compactions do not duplicate them as ordinary user content.

## 9. Remote compaction behavior

Remote compaction should reuse the same prepared-history helper:

1. call `prepare_history_for_compaction(...)`
2. trim the prepared source history to fit the context window
3. send the prepared history to `compact_conversation_history(...)`
4. append the preserved work-notes message after the remote compacted transcript
5. append ghost snapshots
6. persist `replacement_history: Some(new_history)`

This keeps remote compaction aligned with local compaction while preserving the base's existing `base_instructions`-aware trimming path.

## 10. Recommended work-notes schema

The notes should be optimized for loss resistance, not generic summarization.

Required sections:

- `Objective`
- `Current status`
- `Validated findings`
- `Ruled-out hypotheses / dead ends`
- `Open hypotheses / unresolved questions`
- `Relevant files / why`
- `Irrelevant files / why skip`
- `Edits made`
- `Edits in progress / intended edits`
- `Next best step`

This schema directly targets the failure modes that make lossy compaction expensive:

- repeated hypothesis churn
- repeated file rereads
- loss of in-progress edit intent

## Failure Handling

The feature should degrade safely:

- If `final_output_json_schema` is set, skip the note round and compact immediately.
- If the note round errors, compact immediately with `preserved_work_notes = None`.
- If the model tries to call tools during the note round, runtime rejection should prevent side effects and the turn should still compact afterward.
- If the model returns empty notes or a non-notes message (missing `<AUTO_COMPACT_WORK_NOTES>`), retry capture a small number of times and then compact without notes.
- If real user input arrives during the note round, defer draining/replaying it until after compaction.

## Why This Stays Rebase-Friendly

The proposal deliberately avoids:

- protocol changes
- new RPC surfaces
- a dedicated compact-notes model path
- changes to prompt tool selection
- broad restructuring of `run_turn(...)`

The patch stays localized to four files:

- `core/src/codex.rs`
- `core/src/compact.rs`
- `core/src/compact_remote.rs`
- `core/src/tools/parallel.rs`

## Future Rebase Checklist

When rebasing this feature onto a newer upstream base, check these points first:

1. `run_turn(...)`
   - Is the auto-compact trigger still after post-sampling token accounting?
   - Does it still own `client_session` and `turn_metadata_header`?
   - Is pending input still drained, or has it changed to a peek-based flow?
2. `run_sampling_request(...)` / `try_run_sampling_request(...)`
   - Has the parameter list changed?
   - Is `ToolCallRuntime` still created in one place?
3. `ToolCallRuntime`
   - Does rejection still need to build `FunctionCallOutputPayload { body, success }`?
   - Has `handle_tool_call(...)` changed from returning an `impl Future` to an `async fn`, or vice versa?
4. `compact.rs`
   - Does local compaction still rebuild from summary text plus collected user messages?
   - Does rollout reconstruction still require `replacement_history` to preserve verbatim injected notes?
5. `compact_remote.rs`
   - Does remote compaction still trim against `base_instructions` before submission?
   - Where are ghost snapshots appended now?

## Bottom Line

The design is not "make compaction summarize better."

It is:

- run one extra ordinary round in the same turn
- ask for structured work notes
- keep the prompt surface stable
- prevent tool side effects at runtime
- compact from the pre-note source history
- inject the notes verbatim after compaction

That preserves the original intent while staying grounded in the exact `fe8b474...` base shape and remaining useful as a future rebase reference.
