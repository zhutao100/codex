The cleanest drop-in design is to add a **pre-compaction handoff round** at the `run_turn()` decision point, keep the **existing prompt prefix intact**, and make compaction preserve the resulting handoff note as a **first-class, uncompressed artifact** in the replacement history.

## Recommended design

When this branch fires in `core/src/codex.rs:3548-3550`:

```rust
if token_limit_reached && needs_follow_up {
    run_auto_compact(&sess, &turn_context).await;
    continue;
}
```

change the behavior to:

1. run **one extra model round** in the **current session**
2. ask the model to emit a **structured session work note**
3. keep the existing prompt prefix unchanged by:

   * reusing the same `client_session`
   * reusing the same tool surface / tool selection
   * reusing the same instructions/personality
   * only **appending one synthetic user message at the end**
4. suppress actual tool execution in that round at the **runtime layer**, not by changing the prompt
5. then compact
6. preserve the latest work note **verbatim** in the compacted history

That gives you the handoff note you want without turning the normal turn loop inside out.

---

## Why this satisfies the objective

### 1) It uses the existing cached prefix

The current sampling input is rebuilt from `sess.clone_history().await.for_prompt()` in `core/src/codex.rs:3496-3501`, and the normal sampling request builds the prompt from that `input` plus tools/instructions in `core/src/codex.rs:3763-3770`.

So the prefix-preserving move is:

* do **not** mutate prior history
* do **not** disable/remove tools in the prompt
* do **not** alter the main instructions stack
* do **not** start a fresh model session

Instead, append one extra synthetic user message to the already-built prompt input and reuse the same `client_session`.

That preserves the entire preexisting token prefix and only adds a short suffix.

### 2) It avoids the lossy-compaction trap

Today inline compaction mainly carries forward:

* selected user messages via `collect_user_messages()` / `build_compacted_history()` in `core/src/compact.rs:227-333`
* a compact summary generated during the compaction task itself in `core/src/compact.rs:177-198`

That is exactly where “reasoning in progress”, “file relevance conclusions”, and “what was ruled out already” can get flattened too aggressively.

A dedicated pre-compaction note round gives you a model-authored handoff artifact specifically optimized for:

* validated vs ruled-out hypotheses
* relevant vs irrelevant files
* partial edits / intended edits
* outstanding unknowns
* next best step

That artifact is then preserved uncompressed across the history rewrite.

---

## The lowest-conflict insertion point

Keep the main injection where you already identified it:

`core/src/codex.rs:3548-3550`

Replace it conceptually with:

```rust
if token_limit_reached && needs_follow_up {
    maybe_capture_pre_compact_work_notes(
        Arc::clone(&sess),
        Arc::clone(&turn_context),
        Arc::clone(&turn_diff_tracker),
        &mut client_session,
        tool_selection,
        cancellation_token.child_token(),
    )
    .await;

    run_auto_compact(&sess, &turn_context).await;
    continue;
}
```

This is the right place because at that point you already know:

* the session is near the conservative compaction threshold
* more work is still needed
* the live `client_session` still exists
* the current turn’s `tool_selection` is already computed
* the full session history is available

If you move this into `run_auto_compact()` in `core/src/codex.rs:3600-3605`, you lose direct access to `needs_follow_up`, `tool_selection`, and the live `client_session`, which would force a larger refactor.

---

## Proposed implementation

## A new helper in `core/src/codex.rs`

Add a new helper, intentionally **localized** rather than refactoring the whole sampling stack:

```rust
async fn maybe_capture_pre_compact_work_notes(
    sess: Arc<Session>,
    turn_context: Arc<TurnContext>,
    turn_diff_tracker: SharedTurnDiffTracker,
    client_session: &mut ModelClientSession,
    tool_selection: SamplingRequestToolSelection<'_>,
    cancellation_token: CancellationToken,
)
```

### Behavior

1. clone promptable history with `sess.clone_history().await.for_prompt()`
2. append one synthetic user message:

   * “generate session work notes now”
   * “do not call tools”
   * “do not continue solving”
   * “do not make edits”
   * “respond as a single `<session_work_notes>...</session_work_notes>` block”
3. run **one** sampling request using the same `client_session`
4. do **not** record the synthetic request message into persistent history
5. do allow the assistant’s resulting note message to be recorded normally
6. if anything goes wrong, log/warn and fall back to immediate compaction

### Important constraint

Do **not** use the existing compaction path for this round.
The whole point is to preserve the active-turn prompt prefix and the active client session.

---

## Keep the prompt prefix unchanged

The critical design requirement is: **tools must remain present in the prompt**.

The normal prompt is built in `core/src/codex.rs:3763-3770` from:

* `input`
* `tools`
* `parallel_tool_calls`
* `base_instructions`
* `personality`
* `output_schema`

For the pre-compaction note round, the recommended rule is:

* keep **everything** the same
* append only one final user message to `input`

That means no:

* tools removed
* tools reordered
* separate compaction prompt substituted
* separate model session
* fresh synthetic history rewrite before the note round

---

## Tool suppression without prompt drift

You correctly called out that “disable tools” at the prompt layer is bad because it changes the request prefix.

So the right place to suppress tools is **execution**, not **prompt construction**.

### Minimal-change implementation

Extend `ToolCallRuntime` in `core/src/tools/parallel.rs:24-137` with a small execution mode:

```rust
enum ToolCallExecutionMode {
    Normal,
    RejectAll { reason: &'static str },
}
```

Then in `handle_tool_call()`:

* if mode is `Normal`, do today’s behavior
* if mode is `RejectAll`, return a synthetic tool output immediately, e.g.

  * `"tool use disabled during pre-compaction note round"`

This preserves:

* the tool list in the prompt
* the prefix cache opportunity
* the rest of the streaming/event pipeline

But it prevents:

* accidental shell calls
* accidental file edits
* accidental MCP fetches

### Why this is better than prompt-level disabling

Because the model still sees the exact same tool inventory it saw in the immediately preceding round, so the prompt prefix remains stable. Only the appended user message is new.

---

## Output format for the work note

Use a strict tagged block so it is easy to detect and preserve later.

Example synthetic request text:

```text
Before context compaction, write a handoff note for the next session.

Do not call tools.
Do not make file changes.
Do not continue solving the task.
Do not ask the user questions.

Respond with exactly one assistant message that starts with
<session_work_notes>
and ends with
</session_work_notes>

Inside the block, include these sections:
- task_state
- validated_hypotheses
- ruled_out_hypotheses
- open_hypotheses
- files_read_relevant
- files_read_irrelevant
- edits_made
- edits_in_progress
- tool_observations
- constraints_and_preferences
- next_best_step

Be concrete. Include file paths when useful. If a section is empty, write "none".
```

This gives you:

* a machine-detectable marker
* a stable handoff structure
* no need for protocol changes
* no need for output schema changes in the common case

---

## Preservation strategy across compaction

This is the second half of the design and it matters as much as the note round itself.

### Current inline compaction behavior

Inline compaction currently:

* records a compaction prompt into a temporary history
* drains the model to completion
* computes `summary_text`
* rebuilds history from:

  * initial context
  * selected user messages
  * summary text
  * ghost snapshots

That happens in `core/src/compact.rs:70-205`, especially `177-198` and `267-333`.

### Current remote compaction behavior

Remote compaction:

* clones current history
* optionally trims some items
* calls `compact_conversation_history(&prompt)`
* replaces the history with returned `new_history`
* appends ghost snapshots

That happens in `core/src/compact_remote.rs:35-99`.

### What should change

Introduce a compaction-specific preserved artifact:

* the **latest session work note**
* copied **verbatim**
* injected into the new compacted history
* never token-truncated
* never merged into the compact summary text

---

## Concrete preservation rules

Add helpers in `core/src/compact.rs`:

* `is_session_work_notes_message(text: &str) -> bool`
* `extract_latest_session_work_notes(items: &[ResponseItem]) -> Option<String>`
* `push_session_work_notes(history: &mut Vec<ResponseItem>, note: &str)`

### Detection rule

Detect either:

* the assistant note just produced before compaction, or
* the previously preserved synthetic note from an earlier compaction

The simplest detection rule is “message text starts with `<session_work_notes>`”.

### Carry-forward rule

Carry forward **only the latest** work note.

That avoids unbounded accumulation across multiple compactions. The latest note should already summarize the entire current session state, including prior handoff context.

---

## Inline compaction changes

In `core/src/compact.rs`:

### Before rebuilding history

After `history_snapshot.raw_items()` is obtained at `177-178`, extract:

```rust
let preserved_work_notes = extract_latest_session_work_notes(history_items);
```

### When building compacted history

Extend `build_compacted_history(...)` to accept:

```rust
preserved_work_notes: Option<&str>
```

Then build the new history in this order:

1. initial context
2. selected user messages
3. compact summary message
4. preserved work notes message
5. ghost snapshots

I would put the work note **after** the normal compact summary so it is the freshest and most salient artifact in the new session.

### Why not fold it into `summary_text`

Because the requirement is to preserve it **unchanged/uncompressed**.
If you merge it into `summary_text`, future compaction logic can rewrite or truncate it.

---

## Prevent duplication/truncation on later compactions

This is easy to miss.

Today `collect_user_messages()` in `core/src/compact.rs:227-241` collects normal user messages and excludes only summary messages via `is_summary_message()`.

If you preserve the work note as a synthetic user message in compacted history, it will later get swept up as a normal user message unless you filter it out.

So update `collect_user_messages()` to also skip:

* session work note messages

That way:

* work notes are not reselected into the “recent user messages” bucket
* work notes are preserved only via the dedicated preserved-artifact path
* no duplication
* no truncation

---

## Remote compaction changes

In `core/src/compact_remote.rs`:

### Before calling remote compaction

Extract the latest preserved note from the cloned history:

```rust
let preserved_work_notes =
    extract_latest_session_work_notes(history.raw_items());
```

Do this **before** calling `compact_conversation_history(...)`.

### After remote compaction returns

Reinject the preserved note into `new_history` verbatim.

Then append ghost snapshots as today.

### Why remote compaction also needs this

Because the remote compactor may compress or omit exactly the detailed “files read / hypotheses ruled out / edits in progress” artifact that you want to keep lossless.

---

## Edge cases

## 1) Output schema is active

The normal prompt includes `output_schema: turn_context.final_output_json_schema.clone()` in `core/src/codex.rs:3769`.

If that is non-`None`, a generic free-form work-note round is awkward.

For a low-conflict drop-in, I recommend:

* if `final_output_json_schema.is_some()`, **skip the pre-compaction note round**
* compact immediately as today

Why:

* keeping the schema would make note output unreliable
* removing the schema would change more than the suffix of the request
* a schema-aware adapter is possible later, but it is not a minimal patch

This is the one place where I would explicitly choose safety over completeness.

## 2) The note round itself hits context-window overflow

Because `auto_compact_limit` is conservative, the extra round should usually fit.

But if the pre-compaction note round returns `ContextWindowExceeded`, do **not** fail the turn.
Just log/warn and fall through to immediate compaction.

## 3) The model still tries to call tools

That should not break anything.

With runtime-level rejection:

* no tools actually run
* no files are modified
* the round may still complete with a note
* even if it does not, compaction still proceeds

## 4) Multiple compactions in one long thread

Preserve only the **latest** note, not all prior notes.

That keeps the replacement history bounded and avoids repeated note stacking.

---

## Suggested file-by-file patch shape

### `core/src/codex.rs`

Minimal changes:

* add `maybe_capture_pre_compact_work_notes(...)`
* insert one call in the `token_limit_reached && needs_follow_up` branch
* avoid refactoring `run_sampling_request()` unless absolutely necessary

### `core/src/tools/parallel.rs`

Small additive change:

* add `ToolCallExecutionMode`
* add synthetic rejection response path
* default remains `Normal`

### `core/src/compact.rs`

Add compaction-note helpers:

* marker constants
* detect/extract latest note
* filter note messages out of `collect_user_messages()`
* extend `build_compacted_history(...)` to inject preserved note

### `core/src/compact_remote.rs`

Add:

* extract latest preserved note before remote compaction
* re-inject it after remote compaction result is returned

### `core/templates/compact/`

Add one new template file, e.g.:

* `work_notes_prompt.md`

This keeps long prompt text out of Rust and matches the existing compaction prompt pattern.

---

## Why this is rebase-friendly

This matters for your maintenance model.

The design intentionally avoids:

* changing protocol enums
* changing `ResponseItem` shape
* changing history storage semantics globally
* changing the normal turn loop architecture
* extracting shared sampling abstractions across large blocks
* touching the compaction prompt contract more than necessary

Instead, it is a narrow patch:

1. one new call site in `run_turn()`
2. one new helper path for the pre-compaction note round
3. one additive runtime flag for tool rejection
4. one additive preservation path in compactors

That is much less likely to collide with future upstream churn than a broad refactor of `run_sampling_request()` / `try_run_sampling_request()`.

In other words: **localized duplication is preferable here to clever generalization**, because your stated maintenance model is “one patch carried on top of upstream”.

---

## Recommended tests

I would add focused tests around the new invariants, not broad end-to-end rewrites.

### `compact.rs`

* `collect_user_messages_skips_session_work_notes`
* `extract_latest_session_work_notes_returns_latest_only`
* `build_compacted_history_preserves_work_notes_verbatim`
* `build_compacted_history_does_not_truncate_work_notes`

### `compact_remote.rs`

* `remote_compaction_reinjects_preserved_work_notes`

### `codex.rs`

A targeted unit/integration test for:

* `token_limit_reached && needs_follow_up` triggers note round before compaction
* note request is **not** recorded into persistent history
* resulting assistant work note **is** recorded
* compaction replacement history contains the preserved note

### `tools/parallel.rs`

* `reject_all_mode_returns_synthetic_tool_output_without_dispatch`

---

## Bottom line

The recommended patch is:

* **do not** change compaction itself into a smarter summarizer first
* **first** add a dedicated **pre-compaction handoff note round**
* keep the note round prompt-compatible with the current session by changing only the **suffix**
* suppress tools at execution time, not prompt time
* preserve the latest note as a **lossless carried-forward message** in both inline and remote compaction paths

That directly addresses the two failure modes you described:

* repeated hypothesis churn after compaction
* repeated irrelevant file inspection after compaction

while staying close to the current architecture and minimizing future rebase pain.
