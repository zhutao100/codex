# Design Proposal

## Target Base

This proposal targets `custom-0.114.0`.

The original version of this plan was authored against `custom-0.98.0` base. The intent and constraints are preserved, but some patch details are adjusted for drift in compaction behavior and tool output types.

## Design Summary

- Keep the existing `run_turn(...)` loop and turn-scoped `ModelClientSession`.
- Add one extra ordinary sampling round immediately before auto-compaction (both pre-sampling and mid-turn).
- Use that round to emit structured session work notes.
- Keep the prompt tool list unchanged, but reject tool execution at runtime during the note round.
- Compact from prepared history that excludes the transient work-notes request/response suffix.
- Inject preserved work notes verbatim into the compacted transcript, while preserving the mid-turn compaction invariant that the compaction marker remains the last prompt-visible item.
- Reuse the same prepared-history logic for both local and remote compaction.

This preserves the original design principles:

- minimize compaction loss for working state, not just final answers.
- preserve cache-friendly prompt prefixes.
- avoid tool side effects in the extra round.
- keep the patch localized and rebase-friendly.

## Baseline Control Flow (Current)

On `custom-0.114.0`, the relevant shape is:

- `run_turn(...)` performs pre-sampling compaction via `run_pre_sampling_compact(...)`.
- The main loop:
  - drains pending input via `sess.get_pending_input()`
  - replays drained items into history
  - builds prompt input from `sess.clone_history().await.for_prompt(...)`
  - calls `run_sampling_request(...)`
- After each sampling request:
  - recomputes token usage
  - if `token_limit_reached && needs_follow_up`, runs mid-turn compaction via `run_auto_compact(..., InitialContextInjection::BeforeLastUserMessage)`

Key compaction invariant on this base:

- `ContextManager::for_prompt(...)` strips ghost snapshots, so mid-turn compaction relies on the compaction marker being the last prompt-visible item for model training. This port therefore injects preserved work notes immediately *before* the compaction marker, not after it.

## Proposed Patch Shape

## 1. Add a tiny note-capture state machine to `run_turn(...)`

Introduce a turn-local state:

```rust
enum PreCompactNotesState {
    Idle,
    AwaitingNotes { attempts: u8 },
}
```

Use it to split the old "compact immediately" branch into two phases:

1. when `token_limit_reached && needs_follow_up`, inject a synthetic work-notes request and loop once more.
2. while `AwaitingNotes`, compact only after a valid assistant work-notes message is captured.

Validity check:

- require the assistant message to start with `<AUTO_COMPACT_WORK_NOTES>` (ignore tool-call-first / non-message outputs).
- retry note capture up to a small fixed attempt cap (e.g. 3); after that, compact without notes.

Also handle interruption:

- if real user input arrives while `AwaitingNotes`, do **not** drain pending input yet. Leave it queued so it is replayed after compaction and the note-capture round stays deterministic.

## 1b. Capture notes for pre-sampling compaction too

Pre-sampling compaction (including “previous model inline compaction” on model-switch to a smaller context window) can trigger repeatedly within a long session.

To avoid stale work notes across repeated compactions, run the same note capture logic before calling into `run_auto_compact(...)` from:

- `run_pre_sampling_compact(...)`
- `maybe_run_previous_model_inline_compact(...)`

## 2. Keep the note round cache-friendly

The extra round should reuse the existing prompt surface:

- same `ModelClientSession`
- same prompt tool list
- same base instructions/personality
- same output schema configuration, except when schema presence forces the feature to be skipped

The only additional prompt content should be one synthetic developer message appended at the end of history.

## 3. Inject the work-notes request as a synthetic developer message

Append a developer message with sentinel tags:

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
- it is easy to detect and strip from compaction source history

## 4. Suppress tool execution at runtime, not at prompt construction

Keep the prompt tool list unchanged, but extend `ToolCallRuntime` with:

```rust
enum ToolCallExecutionMode {
    Normal,
    RejectAll { reason: &'static str },
}
```

When `RejectAll`, tool calls return a synthetic failure output without dispatching:

- `FunctionCallOutput` / `CustomToolCallOutput`: `FunctionCallOutputPayload { body: Text(reason), success: Some(false) }`
- `McpToolCallOutput`: `McpToolOutput::from_error_text(reason)`

This prevents side effects even if the model ignores the instruction, while keeping prompt cacheability.

## 5. Degrade safely when `final_output_json_schema` is set

If `turn_context.final_output_json_schema.is_some()`, skip the note round and compact immediately. This avoids conflicts between schema-constrained output and free-form notes.

## 6. Prepare compaction source history before both local and remote compaction

Add a shared helper in `core/src/compact.rs`:

- strip the transient notes request/response suffix (detected by `<AUTO_COMPACT_WORK_NOTES_REQUEST>`)
- remove any already-injected preserved work-notes message from the compaction *source* history
- recover preserved notes from:
  - freshly captured note text (when provided)
  - the last preserved notes message already present in history

This helper is reused by both local and remote compaction so they stay aligned.

## 7. Inject preserved work notes while keeping the compaction marker last

To preserve the mid-turn training invariant, insert preserved notes immediately before:

- the local summary message (user-role message starting with `SUMMARY_PREFIX`) in local compaction, or
- the final `ResponseItem::Compaction` item in remote compaction (when present).

If neither anchor is found, append the notes at the end.

## Failure Handling

The feature degrades safely:

- Schema present: skip note capture.
- Note capture round errors: compact immediately without notes.
- Model tries to call tools during note capture: runtime rejection prevents side effects; compaction proceeds.
- Empty/whitespace notes: treated as absent.
- Work-notes request yields no valid work-notes message: retry note capture up to a small cap, then compact without notes.
- Real user input arrives during note capture: defer draining pending input until after compaction so the incoming input is not lost or re-ordered.

## Future Rebase Checklist

When rebasing this feature onto a newer upstream base, check these points first:

1. `run_turn(...)`
   - Is the post-sampling auto-compact trigger still `token_limit_reached && needs_follow_up`?
   - Is pending input still drained (vs peeked) and replayed?
2. `run_sampling_request(...)` / `try_run_sampling_request(...)`
   - Where is `ToolCallRuntime` constructed, and can it be configured per sampling round?
3. `ToolCallRuntime`
   - Have the response item shapes for tool outputs changed again?
4. `ContextManager::for_prompt(...)`
   - Does it still drop ghost snapshots (and only ghost snapshots)?
   - Does mid-turn compaction still rely on a “compaction marker last” invariant?
5. Local/remote compaction paths
   - Is there still a clean place to prepare compaction source history and then re-inject preserved notes into the replacement transcript?

## Bottom Line

This feature is not “make compaction summarize better.”

It is:

- run one extra ordinary round in the same turn
- ask for structured work notes
- keep the prompt surface stable
- prevent tool side effects at runtime
- compact from the pre-note source history
- preserve the notes verbatim across compactions without letting them be re-summarized
