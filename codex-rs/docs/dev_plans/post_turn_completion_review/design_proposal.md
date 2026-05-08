# Design Proposal

## Target Base

This proposal targets this project's current customized branch shape with the custom `model_overlay` feature and the custom `review_model_provider` delegate override already present.

## Design Summary

Add a sibling workflow named `post_turn_completion_review` that can be invoked manually through `/review-completed-turn` or automatically after a regular `TurnComplete` when `[features].auto_post_turn_completion_review = true` is enabled. The workflow spawns a read-only review delegate using the existing review model/provider selection, evaluates the last completed turn using only the user messages and final agent message, and, when it advises fixes, injects an advisory developer message into the main session and continues without a new user turn.

The design intentionally keeps the delegate independent from the main agent's hidden reasoning and tool transcript. This makes the review useful for the specific blind points of context-efficient coding turns: missed files, partial reads, duplicated logic, and final deliverable overclaims.

## User-Facing Shape

### Manual command

Add a slash command:

```text
/review-completed-turn
```

The command reviews the most recent completed regular Codex turn. It should be unavailable while another task is running, matching `/review` and other task-starting commands.

Fresh-session misuse should return a direct error:

```text
No completed Codex turn is available to review yet. Run a normal prompt first, wait for Codex to finish, then use /review-completed-turn.
```

If the last completed regular turn has no final assistant message, return:

```text
The last completed turn has no final assistant message to review. Run another prompt or retry after a completed response.
```

### Automatic feature flag

Register a new under-development feature in `core/src/features.rs`:

```rust
pub enum Feature {
    // ...
    AutoPostTurnCompletionReview,
}

FeatureSpec {
    id: Feature::AutoPostTurnCompletionReview,
    key: "auto_post_turn_completion_review",
    stage: Stage::UnderDevelopment,
    default_enabled: false,
}
```

User config:

```toml
[features]
auto_post_turn_completion_review = true
```

The automatic mode should trigger only after a successful regular main-session `TurnComplete` with a non-empty final assistant message. It should not trigger for review tasks, compact tasks, user-shell tasks, sub-agent sessions, auto-rename tasks, or the post-turn review delegate itself.

## Protocol Additions

### Operation

Add a dedicated operation rather than overloading `Op::Review`, because the core must validate completed-turn state and use different sandbox/output/continuation semantics:

```rust
pub enum Op {
    // ...
    ReviewCompletedTurn,
}
```

The TUI command dispatches `Op::ReviewCompletedTurn`. App-server clients can expose the same operation later without needing to understand TUI-specific state.

### Output payload

Add a minimal output type:

```rust
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, JsonSchema, TS)]
pub struct PostTurnCompletionReviewOutputEvent {
    pub evaluation: String,
    pub fix_actions_advised: bool,
}
```

The delegate prompt should require exactly this JSON shape:

```json
{
  "evaluation": "free-form Markdown text with the review result",
  "fix_actions_advised": true
}
```

Keep `fix_actions_advised` strictly boolean. Do not infer it from prose.

### Review-mode lifecycle reuse

Reuse `EnteredReviewMode(ReviewRequest)` with `ReviewTarget::Custom` and a user-facing hint such as `completed turn`:

```rust
ReviewRequest {
    target: ReviewTarget::Custom {
        instructions: "Review the last completed Codex turn.".to_string(),
    },
    user_facing_hint: Some("completed turn".to_string()),
}
```

For exit, prefer a small extension over a parallel event family:

```rust
pub struct ExitedReviewModeEvent {
    pub review_output: Option<ReviewOutputEvent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub post_turn_completion_review_output: Option<PostTurnCompletionReviewOutputEvent>,
}
```

This keeps existing `/review` clients compatible because `review_output` remains unchanged. `tui/src/chatwidget.rs::on_exited_review_mode(...)` can render either `review_output` or `post_turn_completion_review_output` and keep the same start/finish banners.

### Continuation source

Extend `TurnContinuationSource`:

```rust
pub enum TurnContinuationSource {
    Paused,
    Interrupted,
    PostTurnCompletionReview,
}
```

This makes the event stream explicit when the main model resumes because an independent post-turn review advised further action. It also avoids abusing `Interrupted` or `Paused` and prevents cleanup logic from removing non-existent interrupt markers.

## Completed-Turn Context Capture

Add a small session-state record for the latest completed regular turn:

```rust
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CompletedTurnForReview {
    pub(crate) turn_id: String,
    pub(crate) cwd: PathBuf,
    pub(crate) user_messages: Vec<String>,
    pub(crate) final_agent_message: String,
}
```

Store it in `SessionState`:

```rust
pub(crate) last_completed_regular_turn_for_review: Option<CompletedTurnForReview>,
```

Capture it when a `RegularTask` finishes and before the automatic review trigger runs. The capture path should preserve only:

- text from the user input items for the completed turn;
- the final assistant message returned by `run_turn(...)`.

Do not include:

- `AgentReasoning`, `Reasoning`, or encrypted reasoning items;
- shell/function/tool calls;
- tool outputs;
- approval events;
- intermediate assistant deltas or messages;
- review-mode synthetic user/assistant messages.

Prefer explicit capture over scanning the full history. `Session::spawn_task(...)` currently receives `input: Vec<UserInput>` and moves it into the task; clone or summarize that input into `RunningTask` for regular tasks before moving it. `Session::on_task_finished(...)` already receives `last_agent_message` and `task_kind`, so it can update `last_completed_regular_turn_for_review` when `task_kind == TaskKind::Regular` and the final message is non-empty.

If resume support for manual `/review-completed-turn` is required, add a fallback reconstruction helper that scans history for the most recent user-turn boundary followed by a final assistant message. This fallback should ignore Codex-generated developer messages and review synthetic messages; it should remain a fallback because history reconstruction is less precise than explicit per-turn capture.

## Delegate Prompt And Input

Add a dedicated prompt file:

```text
core/post_turn_completion_review_prompt.md
```

Add a corresponding include in `core/src/client_common.rs`:

```rust
pub const POST_TURN_COMPLETION_REVIEW_PROMPT: &str = include_str!("../post_turn_completion_review_prompt.md");
```

The prompt should instruct the delegate to:

- review the end-state of the completed turn and final user-facing deliverable;
- look for missed relevant files, incomplete paired updates, duplicate logic, broken registrations, missing tests, and final-answer overclaims;
- inspect repository files only through read-only tools;
- avoid style nits and speculative concerns;
- output exactly `PostTurnCompletionReviewOutputEvent` JSON and no markdown fence.

Pass the completed-turn context as the delegate's only user input:

```xml
<completed_turn_review_context>
  <turn_id>...</turn_id>
  <cwd>...</cwd>
  <user_messages>
    <message index="1"><![CDATA[...]]></message>
  </user_messages>
  <final_agent_message><![CDATA[...]]></final_agent_message>
</completed_turn_review_context>
```

Do not pass the normal session history as `InitialHistory`; use `InitialHistory::New`. The delegate can inspect the repository read-only through tools, but the only conversation context it receives should be the user message(s) and final agent message from the completed turn.

## Delegate Configuration

Extract the shared `/review` delegate setup into a helper so both workflows stay aligned:

```rust
pub(crate) struct ReviewDelegateConfigParams<'a> {
    pub(crate) base_instructions: &'a str,
    pub(crate) sandbox_policy: SandboxPolicy,
    pub(crate) disable_collab: bool,
}

pub(crate) fn configure_review_delegate_config(
    parent_config: &Config,
    parent_model_slug: &str,
    params: ReviewDelegateConfigParams<'_>,
) -> Result<Config, CodexErr>;
```

The helper should apply the common policy:

- clone the parent `Config`;
- set `web_search_mode = Some(WebSearchMode::Disabled)`;
- disable `Feature::WebSearchRequest` and `Feature::WebSearchCached`;
- disable `Feature::Collab` unless a caller explicitly permits it;
- set `base_instructions` to the caller's prompt;
- set `approval_policy = Constrained::allow_any(AskForApproval::Never)`;
- select `review_model` when set, otherwise `parent_model_slug`;
- apply `review_model_provider` through `apply_delegate_model_provider(...)`;
- disable `Feature::RemoteModels` when `review_model_provider` is set;
- set the caller-specified sandbox policy.

For `/review`, call the helper with the existing prompt and the parent sandbox policy to preserve behavior. For `post_turn_completion_review`, call it with `POST_TURN_COMPLETION_REVIEW_PROMPT` and `SandboxPolicy::ReadOnly`:

```rust
let sub_agent_config = configure_review_delegate_config(
    ctx.config.as_ref(),
    ctx.model_info.slug.as_str(),
    ReviewDelegateConfigParams {
        base_instructions: crate::POST_TURN_COMPLETION_REVIEW_PROMPT,
        sandbox_policy: SandboxPolicy::ReadOnly,
        disable_collab: true,
    },
)?;
```

Use `run_codex_thread_one_shot(...)` from `core/src/codex_delegate.rs` exactly as `/review` does. Keep `InitialHistory::New`.

## Core Workflow

Add a task file:

```text
core/src/tasks/post_turn_completion_review.rs
```

Add `TaskKind::PostTurnCompletionReview` so task completion can be excluded from automatic review triggering.

High-level flow:

1. `Op::ReviewCompletedTurn` enters `handlers::review_completed_turn(...)`.
2. The handler checks that no task is active.
3. The handler loads `last_completed_regular_turn_for_review` or reconstructs a fallback from history.
4. If no valid context exists, emit a friendly `ErrorEvent` and return.
5. Build a `TurnContext` using `new_default_turn_with_sub_id(...)`, then override the review delegate config inside the task as described above.
6. Emit `EnteredReviewMode(ReviewRequest { target: ReviewTarget::Custom { ... }, user_facing_hint: Some("completed turn") })`.
7. Run the read-only delegate with `POST_TURN_COMPLETION_REVIEW_PROMPT` and the structured completed-turn context.
8. Parse the final delegate `last_agent_message` as `PostTurnCompletionReviewOutputEvent`, with a fallback that stores unparsable text as `evaluation` and sets `fix_actions_advised = false`.
9. Emit `ExitedReviewMode(ExitedReviewModeEvent { review_output: None, post_turn_completion_review_output: Some(output.clone()) })`.
10. If `fix_actions_advised` is false, finish normally and emit `TurnComplete` for the review task.
11. If `fix_actions_advised` is true, inject an advisory developer message into the main session, then continue the main session without a new user message.

## Automatic Trigger

Extend `Session::on_task_finished(...)` after it emits the regular `TurnComplete` event and after it has stored `last_completed_regular_turn_for_review`:

```rust
if task_kind == TaskKind::Regular
    && turn_context.features.enabled(Feature::AutoPostTurnCompletionReview)
    && should_auto_review_completed_turn(turn_context.as_ref(), last_agent_message.as_deref())
{
    self.spawn_post_turn_completion_review(turn_context).await;
}
```

`should_auto_review_completed_turn(...)` should return false when:

- `last_agent_message` is absent or whitespace;
- `turn_context.session_source` is `SessionSource::SubAgent(_)`;
- the task was not a regular main-session task;
- the active completed turn was itself a continuation from `PostTurnCompletionReview` and a guard decides not to review repeated no-op loops;
- the feature is disabled.

The first implementation can allow repeated review-after-fix cycles because the workflow naturally stops when `fix_actions_advised = false`. Add a simple guard against reviewing the same `turn_id` twice concurrently.

## Developer Message Handoff

When `fix_actions_advised` is true, wrap the evaluation in a developer message, not a user message:

```xml
<post_turn_completion_review>
  <context>An independent post-turn review inspected the completed turn. The review may be correct or incorrect. Treat it as advisory input, verify the claims against the repository, and only act on findings that are actually applicable.</context>
  <fix_actions_advised>true</fix_actions_advised>
  <evaluation>
  ...review text...
  </evaluation>
</post_turn_completion_review>
```

Record it with `DeveloperInstructions::new(message).into()` and `Session::record_conversation_items(...)` so the next model request sees a developer-role item. Do not emit it as a visible user message.

Then continue the main session without recording a new user turn. Prefer a helper that reuses the existing continuation path while making the source explicit:

```rust
sess.continue_after_post_turn_completion_review(
    turn_context,
    PendingContinuation {
        source: TurnContinuationSource::PostTurnCompletionReview,
        continued_from_turn_id: Some(reviewed_turn_id),
    },
).await;
```

This helper can call the same `ContinueTask` / `continue_turn(...)` machinery, but `prepare_history_for_continuation(...)` must not remove an interrupt marker for `PostTurnCompletionReview`. The existing `remove_interrupted_abort` boolean already handles this if it remains `checkpoint.source == TurnContinuationSource::Interrupted`.

This preserves the important behavioral distinction: a new user message normally starts a new turn boundary, but the advisory developer message lets the main model continue the completed turn with the same prior conversation prefix and without changing the user's intent.

## TUI Changes

### Slash command registry

Add a command near `/review` in `tui/src/slash_command.rs`:

```rust
#[strum(serialize = "review-completed-turn")]
ReviewCompletedTurn,
```

Description:

```rust
SlashCommand::ReviewCompletedTurn => "review the last completed turn for missed follow-ups",
```

It should not support inline args and should not be available during a running task.

### Dispatch

In `tui/src/chatwidget.rs::dispatch_command(...)`:

```rust
SlashCommand::ReviewCompletedTurn => {
    self.submit_op(Op::ReviewCompletedTurn);
}
```

`submit_op(...)` should set the task-running state for both `Op::Review` and `Op::ReviewCompletedTurn` so the UI blocks competing task starts while the post-turn reviewer runs.

### Review-mode rendering

Update `on_exited_review_mode(...)` to render `post_turn_completion_review_output` when present. A minimal rendering is the `evaluation` markdown plus a final status line:

```text
Fix actions advised: yes
```

or:

```text
Fix actions advised: no
```

Keep the existing `>> Code review started: ... <<` and `<< Code review finished >>` banners unless a more specific banner is desired later. Using the existing lifecycle avoids adding a second modal state.

## Prompt File Draft

A suitable first version of `core/post_turn_completion_review_prompt.md`:

```markdown
# Post-turn completion review guidelines

You are an independent reviewer for a completed Codex coding turn. Review the repository end-state and the final assistant deliverable from that completed turn.

Your purpose is to catch issues that a context-efficient coding agent may miss: relevant files not found by keyword search, partial file reads that missed nearby contracts, duplicated existing functionality, incomplete paired updates, missing registration/schema/test changes, and final-answer claims that do not match repository state.

You may inspect files and run read-only commands. Do not modify files. Do not request write approval. Do not use web search.

Only use the completed-turn context supplied by the user message: the user messages and the final assistant message. Do not assume access to hidden reasoning, tool calls, or intermediate transcript events.

Return `fix_actions_advised = true` only when the main session should continue and consider concrete follow-up actions. Use `false` for clean reviews, speculative concerns, low-confidence style nits, or issues that are not actionable from the current repository state.

Output exactly this JSON object and no markdown fence:

{
  "evaluation": "free-form Markdown review result",
  "fix_actions_advised": false
}
```

## Parsing And Fallbacks

Mirror the resilient parser in `core/src/tasks/review.rs::parse_review_output_event(...)`:

- first parse the whole final message as `PostTurnCompletionReviewOutputEvent`;
- then try to extract the first JSON object substring;
- if parsing still fails, use `evaluation = text` and `fix_actions_advised = false`;
- if `evaluation` is empty, use a fallback such as `Post-turn reviewer produced no evaluation.`.

Do not set `fix_actions_advised = true` from an unparsable response. False negatives are safer than untrusted autonomous continuation.

## Failure Handling

|Failure|Behavior|
|---|---|
|Fresh session or no completed regular turn|Emit `ErrorEvent` with the friendly guidance above; do not enter review mode.|
|Active task is running|Emit `Cannot review a completed turn while another task is running.`|
|Last completed regular turn has no final assistant message|Emit `The last completed turn has no final assistant message to review.`|
|Delegate spawn fails|Emit `ErrorEvent` with prefix `Post-turn completion review failed`.|
|Delegate interrupted|Emit `ExitedReviewMode` with no post-turn output and a short interrupted message; do not inject a developer message.|
|Delegate output unparsable|Render the raw text as evaluation, set `fix_actions_advised = false`, and do not continue automatically.|
|Developer-message continuation spawn fails|Emit an error and leave the advisory review visible in the transcript; do not retry automatically.|

## Implementation Sequence

1. Add protocol types: `Op::ReviewCompletedTurn`, `PostTurnCompletionReviewOutputEvent`, optional field on `ExitedReviewModeEvent`, and `TurnContinuationSource::PostTurnCompletionReview`.
2. Add `Feature::AutoPostTurnCompletionReview` and its `FeatureSpec` with key `auto_post_turn_completion_review`, `Stage::UnderDevelopment`, and `default_enabled = false`.
3. Add `core/post_turn_completion_review_prompt.md` and `POST_TURN_COMPLETION_REVIEW_PROMPT` include.
4. Add `CompletedTurnForReview` session state and capture it for completed regular turns.
5. Extract shared review delegate config setup from `core/src/tasks/review.rs` into a helper and update `/review` to use it without behavior changes.
6. Add `core/src/tasks/post_turn_completion_review.rs` and wire it into `core/src/tasks/mod.rs`.
7. Add `handlers::review_completed_turn(...)` in `core/src/codex.rs` and route `Op::ReviewCompletedTurn` to it.
8. Add automatic trigger after regular `TurnComplete` when the feature is enabled.
9. Add developer-message injection and post-completion continuation helper.
10. Add TUI slash command, dispatch, task-running handling, and post-turn review rendering.
11. Add tests.

## Test Plan

### Core tests

- `ReviewCompletedTurn` on a fresh session emits a friendly `BadRequest` error and does not spawn a task.
- A completed regular turn stores `CompletedTurnForReview` with only user text and final assistant text.
- The stored review context excludes reasoning, shell calls, tool outputs, approval events, and intermediate assistant events.
- Manual `ReviewCompletedTurn` uses `review_model` when set.
- Manual `ReviewCompletedTurn` applies `review_model_provider` and disables `Feature::RemoteModels` in the delegate config.
- The delegate config always has `WebSearchMode::Disabled`, disabled web-search features, `AskForApproval::Never`, and `SandboxPolicy::ReadOnly`.
- `fix_actions_advised = false` emits review output but does not record a developer message and does not continue the main session.
- `fix_actions_advised = true` records exactly one developer message and starts a continuation with `TurnContinuationSource::PostTurnCompletionReview`.
- An unparsable delegate response is rendered as evaluation with `fix_actions_advised = false`.
- Automatic review triggers after regular main-session `TurnComplete` only when the feature is enabled.
- Automatic review does not trigger for review tasks, compact tasks, user-shell tasks, or sub-agent sessions.

### TUI tests

- `/review-completed-turn` appears near `/review` with the expected description.
- The command is disabled while a task is running.
- Dispatch sends `Op::ReviewCompletedTurn`.
- `EnteredReviewMode` with `user_facing_hint = completed turn` shows the existing review start banner.
- `ExitedReviewMode` with `post_turn_completion_review_output` renders the evaluation and the binary fix-advice status.

### Rollout and continuation tests

- A positive review handoff records a developer-role response item, not a user-role response item.
- The follow-up main model request does not introduce a new user turn boundary.
- `TurnContinued` is emitted with source `post_turn_completion_review`.
- Continue cleanup does not remove normal completed-turn transcript items.
- Existing pause/interruption continuation behavior remains unchanged.

## Rebase-Sensitive Surfaces

- `protocol/src/protocol.rs`: operation enum, review-mode payloads, continuation source, TypeScript bindings.
- `core/src/features.rs`: feature enum and `FEATURES` registry ordering.
- `core/src/tasks/review.rs`: shared delegate config extraction must preserve existing `/review` behavior.
- `core/src/codex_delegate.rs`: should remain the delegate runner; avoid embedding post-turn-specific behavior here.
- `core/src/codex.rs`: operation routing, completed-turn capture, automatic trigger, and continuation handoff.
- `core/src/tasks/mod.rs`: `TaskKind` additions and task lifecycle interactions.
- `tui/src/slash_command.rs` and `tui/src/chatwidget.rs`: slash command, task-running state, and review-mode rendering.
- `core/templates/review/*`: no change required unless the implementation chooses to add post-turn-specific exit templates.

## Bottom Line

Implement `post_turn_completion_review` as a narrow sibling to `/review`: use the same review model/provider and `codex_delegate` machinery, force read-only and web-disabled execution, feed the delegate only the completed turn's user/final messages, and use a boolean advisory output to decide whether to continue the main session through a developer message instead of creating a new user turn.
