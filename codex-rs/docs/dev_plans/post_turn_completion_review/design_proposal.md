# Design Proposal

## Implementation Status

The workflow is present in this project: `Op::ReviewCompletedTurn`, `PostTurnCompletionReviewTask`, `core/post_turn_completion_review_prompt.md`, read-only review delegate configuration, post-turn output parsing, and continuation handoff are wired. Later hardening addressed two observed gaps: review delegates now receive the full compact user/final-assistant interaction history for the session instead of only a single reconstructed pair, and the prompt treats `fix_actions_advised` as a concrete follow-up signal for both bugs and incomplete user-request fulfillment.

## Target Base

This proposal targets this project's current customized code shape with the custom `model_overlay` feature and the custom `review_model_provider` delegate override already present.

## Design Summary

Add a sibling workflow named `post_turn_completion_review` that can be invoked manually through `/review-completed-turn` or automatically after a regular `TurnComplete` when `[features].auto_post_turn_completion_review = true` is enabled. The workflow spawns a read-only review delegate using the existing review model/provider selection, evaluates the last completed turn using a compact history of every regular round's user messages and final assistant message, and, when it advises concrete follow-up actions, injects an advisory developer message into the main session and continues without a new user turn.

The hardening layer also updates generic `/review` and post-turn review delegate setup so each review task can use review-scoped host `AGENTS.*.md` instructions and optional prompt files from `~/.codex/config.toml`, while retaining the built-in prompts and project-level `AGENTS.md` docs as fallbacks.

The design intentionally keeps the delegate independent from the main agent's hidden reasoning and tool transcript. This makes the review useful for the specific blind points of context-efficient coding turns: missed files, partial reads, duplicated logic, final deliverable overclaims, and silent narrowing of the requested scope across a multi-round session.

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

## Review Configuration Keys

Add top-level `~/.codex/config.toml` keys for review host-instruction filenames and prompt-file overrides:

```toml
# Host-level AGENTS filename candidates, resolved under CODEX_HOME.
host_agents_filename = "AGENTS.md"
review_agents_filename = "AGENTS.review.md"
post_turn_completion_review_agents_filename = "AGENTS.post-turn-review.md"

# Optional on-disk prompt overrides. Unset means use the built-in prompt files.
review_prompt_file = "~/.codex/prompts/review.md"
post_turn_completion_review_prompt_file = "~/.codex/prompts/post-turn-review.md"
```

Host filename keys are filenames, not paths. Resolve them under `Config::codex_home`, reject absolute paths or path separators, and fall back to each default filename when the configured value is unset or empty after trimming. Missing candidate files are not errors; they fall through to the next candidate in the hierarchy. Unreadable non-missing files should produce a clear config or task error.

Prompt-file keys are on-disk paths. Read the configured file at runtime, trim surrounding whitespace, reject empty prompt files, and use the contents as the delegate `base_instructions`. If unset, generic `/review` falls back to `core/review_prompt.md` through `REVIEW_PROMPT`, and post-turn completion review falls back to `core/post_turn_completion_review_prompt.md` through `POST_TURN_COMPLETION_REVIEW_PROMPT`.

The first implementation uses the host filename keys only while constructing review delegate configs. It does not change normal main-session host instruction loading.

Implementation shape:

```rust
pub struct ConfigToml {
    pub host_agents_filename: Option<String>,
    pub review_agents_filename: Option<String>,
    pub post_turn_completion_review_agents_filename: Option<String>,
    pub review_prompt_file: Option<AbsolutePathBuf>,
    pub post_turn_completion_review_prompt_file: Option<AbsolutePathBuf>,
}

pub struct Config {
    pub host_agents_filename: String,
    pub review_agents_filename: String,
    pub post_turn_completion_review_agents_filename: String,
    pub review_prompt: Option<String>,
    pub post_turn_completion_review_prompt: Option<String>,
}
```

Keep these keys top-level for the first implementation. If profile-scoped prompt or host filename overrides are needed later, add them by mirroring the existing profile merge rules for other config fields rather than special-casing review tasks.

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

Add a small session-state record for the latest completed regular turn plus the compact session interaction history that led to it:

```rust
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CompletedTurnReviewRound {
    pub(crate) user_messages: Vec<String>,
    pub(crate) final_agent_message: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CompletedTurnForReview {
    pub(crate) turn_id: String,
    pub(crate) cwd: PathBuf,
    pub(crate) interaction_history: Vec<CompletedTurnReviewRound>,
    pub(crate) user_messages: Vec<String>,
    pub(crate) final_agent_message: String,
}
```

Store it in `SessionState`:

```rust
pub(crate) last_completed_regular_turn_for_review: Option<CompletedTurnForReview>,
```

Capture it when a `RegularTask` finishes and before the automatic review trigger runs. The capture path should preserve only:

- text from real user-turn boundary messages for each completed round;
- the final assistant message for each completed round;
- the latest completed turn's `turn_id` and `cwd`.

Do not include:

- `AgentReasoning`, `Reasoning`, or encrypted reasoning items;
- shell/function/tool calls;
- tool outputs;
- approval events;
- intermediate assistant deltas or non-final assistant messages within a round;
- review-mode synthetic user/assistant messages.

Build `interaction_history` from the session history by pairing each real user-turn boundary with the last non-empty assistant message before the next real user turn. This pairing fixes the multi-round failure mode where the reviewer could receive the first user message and the latest assistant message as a single false pair. `Session::spawn_task(...)` still summarizes the current input into `RunningTask` for regular tasks before moving it; `Session::on_task_finished(...)` uses that explicit input as a fallback when history cannot reconstruct the current turn precisely.

If resume support for manual `/review-completed-turn` is required, use the same reconstruction helper over the resumed history. The helper should ignore Codex-generated developer messages and review synthetic messages.

## Delegate Prompt And Input

Keep the dedicated built-in prompt file:

```text
core/post_turn_completion_review_prompt.md
```

Keep the corresponding include in `core/src/client_common.rs` as the fallback prompt:

```rust
pub const POST_TURN_COMPLETION_REVIEW_PROMPT: &str = include_str!("../post_turn_completion_review_prompt.md");
```

At task setup, resolve the effective prompt from config before calling `configure_review_delegate_config(...)`:

1. If `Config::post_turn_completion_review_prompt` was loaded from `post_turn_completion_review_prompt_file`, use that string.
2. Otherwise use `POST_TURN_COMPLETION_REVIEW_PROMPT`.

Apply the same pattern to generic `/review` with `review_prompt_file` and `REVIEW_PROMPT` from `core/review_prompt.md`.

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
  <session_interaction_history note="The highest-indexed round is the completed turn being reviewed.">
    <round index="1">
      <user_messages>
        <message index="1"><![CDATA[...]]></message>
      </user_messages>
      <final_agent_message><![CDATA[...]]></final_agent_message>
    </round>
  </session_interaction_history>
</completed_turn_review_context>
```

Do not pass the normal session history as `InitialHistory`; use `InitialHistory::New`. The delegate can inspect the repository read-only through tools, but the only conversation context it receives should be the compact per-round user/final-assistant history. Do not pass hidden reasoning, tool calls, or tool outputs.


## Review Effectiveness Hardening

The post-turn reviewer must be optimized for complementarity, not for repeating the main session. The base prompt should explicitly state that the main session may already have used context-efficient keyword search and narrow range reads, and that the review delegate should use a different coverage-driven inspection strategy.

Minimum prompt requirements:

1. Treat the completed turn as an end-state artifact to verify, not as a request to perform a generic review.
2. Build a request-fulfillment checklist from every user message in the supplied interaction history, including explicit requirements, constraints, verification asks, and promised follow-ups.
3. Start by deriving a coverage checklist from the request-fulfillment checklist, the final assistant message, changed or untracked files when available, repository manifests, neighboring modules, tests, schemas, protocol definitions, generated bindings, and registration points.
4. Prefer whole-file reads for small and medium changed files. For large files, inspect the whole relevant symbol or module context plus imports, exports, registration tables, nearby tests, and paired helper functions. Do not rely only on `rg` hits followed by narrow `sed` ranges.
5. Use multiple orthogonal searches for duplicate or related logic: new symbol names, semantic concepts, config keys, protocol variants, UI labels, test names, file families, and neighboring directory structure.
6. For each suspected issue, cite concrete repository evidence in the `evaluation` text: file path, missing paired surface, conflicting existing helper, unsupported final-answer claim, unfulfilled user requirement, or test gap.
7. Treat `fix_actions_advised` as the concrete follow-up signal despite its historical bug-fix name. Set it to true for actionable bugs, incomplete requested scope, missed deliverables, or required verification follow-up. Use false for speculative concerns, stylistic preferences, missing evidence, or findings that do not require the main session to continue.

A useful `evaluation` format is Markdown inside the JSON string with short sections such as `Inspection coverage`, `Findings`, and `Fix actions advised`. The schema should stay unchanged so existing clients only need the `evaluation` text and the boolean signal.

## Delegate Configuration

Extract the shared `/review` delegate setup into a helper so both workflows stay aligned:

```rust
pub(crate) enum ReviewDelegateInstructionProfile {
    Review,
    PostTurnCompletionReview,
}

pub(crate) struct ReviewDelegateConfigParams<'a> {
    pub(crate) base_instructions: &'a str,
    pub(crate) sandbox_policy: SandboxPolicy,
    pub(crate) disable_collab: bool,
    pub(crate) instruction_profile: ReviewDelegateInstructionProfile,
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
- set `base_instructions` to the caller's resolved prompt;
- set `approval_policy = Constrained::allow_any(AskForApproval::Never)`;
- select `review_model` when set, otherwise `parent_model_slug`;
- apply `review_model_provider` through `apply_delegate_model_provider(...)`;
- disable `Feature::RemoteModels` when `review_model_provider` is set;
- set the caller-specified sandbox policy;
- apply the caller-specified instruction profile so review delegates can avoid host main-session-only guidance while keeping project docs.

For `/review`, call the helper with the resolved `/review` prompt, the parent sandbox policy, and `ReviewDelegateInstructionProfile::Review`. For `post_turn_completion_review`, call it with the resolved post-turn prompt, `SandboxPolicy::ReadOnly`, and `ReviewDelegateInstructionProfile::PostTurnCompletionReview`:

```rust
let prompt = ctx
    .config
    .post_turn_completion_review_prompt
    .as_deref()
    .unwrap_or(crate::POST_TURN_COMPLETION_REVIEW_PROMPT);

let sub_agent_config = configure_review_delegate_config(
    ctx.config.as_ref(),
    ctx.model_info.slug.as_str(),
    ReviewDelegateConfigParams {
        base_instructions: prompt,
        sandbox_policy: SandboxPolicy::ReadOnly,
        disable_collab: true,
        instruction_profile: ReviewDelegateInstructionProfile::PostTurnCompletionReview,
    },
)?;
```

Use `run_codex_thread_one_shot(...)` from `core/src/codex_delegate.rs` exactly as `/review` does. Keep `InitialHistory::New`.


## Review-Scoped Host Instructions

`configure_review_delegate_config(...)` currently clones the parent `Config`, so the delegate inherits the parent `Config::user_instructions` that was loaded for the main session. `Codex::spawn(...)` then calls `project_doc::get_user_instructions(...)`, which combines host-level instructions from `Config::user_instructions`, project-level docs discovered by `core/src/project_doc.rs`, skills, and the hierarchical `AGENTS.md` note. Project-level instructions should remain available to review delegates, but host-level main-session tool-use and editing guidance should not control review methodology.

Add a source-separated host instruction loader that can be used after cloning the parent config:

```rust
pub(crate) enum ReviewDelegateInstructionProfile {
    Review,
    PostTurnCompletionReview,
}

pub(crate) fn load_review_host_instructions(
    config: &Config,
    profile: ReviewDelegateInstructionProfile,
) -> std::io::Result<Option<String>>;
```

The helper resolves candidates under `config.codex_home`:

|Profile|Candidate order|
|---|---|
|`Review`|`review_agents_filename`, then `host_agents_filename`|
|`PostTurnCompletionReview`|`post_turn_completion_review_agents_filename`, then `review_agents_filename`, then `host_agents_filename`|

With default filenames, the effective hierarchy is:

1. Generic `/review`: prefer `~/.codex/AGENTS.review.md` if it exists and is non-empty; otherwise fall back to `~/.codex/AGENTS.md`.
2. Post-turn completion review: prefer `~/.codex/AGENTS.post-turn-review.md` if it exists and is non-empty; then `~/.codex/AGENTS.review.md`; otherwise fall back to `~/.codex/AGENTS.md`.

When configuring a delegate, replace `sub_agent_config.user_instructions` with the helper result for the selected profile before calling `run_codex_thread_one_shot(...)`. This intentionally avoids carrying main-session host instructions into review delegates. Do not mutate project doc discovery: `project_doc::get_user_instructions(...)` should still append repository `AGENTS.md` docs, skills, and the hierarchical agents note after the review-specific host instructions.

Do not add host section-marker parsing in the first implementation. Separate `AGENTS.*.md` files plus filename override keys are enough for the desired hierarchy and avoid parsing already-concatenated instruction text. If a future project-level audience feature is needed, add it separately from host instruction selection.

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
7. Run the read-only delegate with the resolved post-turn prompt, the structured completed-turn context, and the post-turn review instruction profile.
8. The delegate follows the coverage-driven review protocol from the prompt rather than replaying the main session search strategy.
9. Parse the final delegate `last_agent_message` as `PostTurnCompletionReviewOutputEvent`, with a fallback that stores unparsable text as `evaluation` and sets `fix_actions_advised = false`.
10. Emit `ExitedReviewMode(ExitedReviewModeEvent { review_output: None, post_turn_completion_review_output: Some(output.clone()) })`.
11. If `fix_actions_advised` is false, finish normally and emit `TurnComplete` for the review task.
12. If `fix_actions_advised` is true, inject an advisory developer message into the main session, then continue the main session without a new user message.

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

A suitable first version of `core/post_turn_completion_review_prompt.md` as the built-in fallback prompt. If `post_turn_completion_review_prompt_file` is configured, the configured file replaces this prompt at runtime:

```markdown
# Post-turn completion review guidelines

You are an independent reviewer for a completed Codex coding turn. Review the repository end-state and the final assistant deliverable from that completed turn.

Your purpose is to catch issues that a context-efficient coding agent may miss: relevant files not found by keyword search, partial file reads that missed nearby contracts, duplicated existing functionality, incomplete paired updates, missing registration/schema/test changes, and final-answer claims that do not match repository state.

Do not merely repeat the main session strategy of keyword search followed by narrow range reads. The main session may already have used that path. Build an independent coverage checklist from the user request, final assistant message, changed or untracked files when available, repository manifests, neighboring modules, tests, schemas, protocol definitions, generated bindings, and registration points.

Prefer whole-file inspection for small and medium relevant files. For large files, inspect the whole relevant symbol or module context plus imports, exports, registration tables, nearby tests, and paired helper functions. Use multiple orthogonal searches for duplicate or related logic: new symbol names, semantic concepts, config keys, protocol variants, UI labels, test names, file families, and neighboring directory structure.

You may inspect files and run read-only commands. Do not modify files. Do not request write approval. Do not use web search. If inherited instructions conflict with this review methodology, this prompt wins for the post-turn review task.

Only use the completed-turn context supplied by the user message: the user messages and the final assistant message. Do not assume access to hidden reasoning, tool calls, or intermediate transcript events.

In the `evaluation` string, include concrete evidence for any finding: file path, missing paired surface, conflicting existing helper, unsupported final-answer claim, or test gap. A compact structure such as `Inspection coverage`, `Findings`, and `Fix actions advised` is preferred.

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
3. Keep `core/review_prompt.md` and `core/post_turn_completion_review_prompt.md` as built-in fallbacks and add config-backed prompt-file loading for `review_prompt_file` and `post_turn_completion_review_prompt_file`.
4. Add `CompletedTurnForReview` session state and capture it for completed regular turns.
5. Extract shared review delegate config setup from `core/src/tasks/review.rs` into a helper and update `/review` to use the `Review` instruction profile.
6. Add review host filename config keys and the delegate host-instruction loader with the `/review` and post-turn `AGENTS.*.md` hierarchies.
7. Add `core/src/tasks/post_turn_completion_review.rs` and wire it into `core/src/tasks/mod.rs`.
8. Add `handlers::review_completed_turn(...)` in `core/src/codex.rs` and route `Op::ReviewCompletedTurn` to it.
9. Add automatic trigger after regular `TurnComplete` when the feature is enabled.
10. Add developer-message injection and post-completion continuation helper.
11. Add TUI slash command, dispatch, task-running handling, and post-turn review rendering.
12. Run `just write-config-schema` after adding `ConfigToml` keys.
13. Add tests.

## Test Plan

### Core tests

- `ReviewCompletedTurn` on a fresh session emits a friendly `BadRequest` error and does not spawn a task.
- A completed regular turn stores `CompletedTurnForReview` with only user text and final assistant text.
- The stored review context excludes reasoning, shell calls, tool outputs, approval events, and intermediate assistant events.
- Manual `ReviewCompletedTurn` uses `review_model` when set.
- Manual `ReviewCompletedTurn` applies `review_model_provider` and disables `Feature::RemoteModels` in the delegate config.
- The delegate config always has `WebSearchMode::Disabled`, disabled web-search features, `AskForApproval::Never`, and `SandboxPolicy::ReadOnly`.
- The post-turn review prompt contains explicit coverage-driven review guidance and says not to replay the main session keyword/range-read strategy.
- `review_prompt_file` overrides `REVIEW_PROMPT`; `post_turn_completion_review_prompt_file` overrides `POST_TURN_COMPLETION_REVIEW_PROMPT`; both reject empty files and report unreadable files clearly.
- Generic `/review` loads `~/.codex/AGENTS.review.md` when present and non-empty, otherwise `~/.codex/AGENTS.md`.
- Post-turn review loads `~/.codex/AGENTS.post-turn-review.md`, then `~/.codex/AGENTS.review.md`, then `~/.codex/AGENTS.md`.
- `host_agents_filename`, `review_agents_filename`, and `post_turn_completion_review_agents_filename` override the corresponding hierarchy slots and reject path-like values.
- The post-turn review instruction profile keeps project-level `AGENTS.md` instructions available while excluding host main-session-only guidance when review-scoped host instructions are configured.
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
- `core/src/config/mod.rs` and `core/config.schema.json`: new host filename and prompt-file config keys.
- `core/src/client_common.rs`: built-in review prompt constants remain fallbacks while task setup resolves configured prompt-file contents.
- `core/src/tasks/review.rs`: shared delegate config extraction must preserve `/review` model, sandbox, output, and provider behavior while adding the `Review` instruction profile.
- `core/src/project_doc.rs` and `core/src/instructions/user_instructions.rs`: review-scoped host instruction loading and source separation from project docs.
- `core/src/codex_delegate.rs`: should remain the delegate runner; avoid embedding post-turn-specific behavior here.
- `core/src/codex.rs`: operation routing, completed-turn capture, automatic trigger, and continuation handoff.
- `core/src/tasks/mod.rs`: `TaskKind` additions and task lifecycle interactions.
- `tui/src/slash_command.rs` and `tui/src/chatwidget.rs`: slash command, task-running state, and review-mode rendering.
- `core/templates/review/*`: no change required unless the implementation chooses to add post-turn-specific exit templates.

## Bottom Line

Keep `post_turn_completion_review` as a narrow sibling to `/review`: use the same review model/provider and `codex_delegate` machinery, force read-only and web-disabled execution, feed the delegate only the completed turn's user/final messages, and use a boolean advisory output to decide whether to continue the main session through a developer message instead of creating a new user turn. The follow-up hardening is to make review genuinely complementary by strengthening the post-turn prompt, allowing both review prompts to be overridden from configured files, and separating review-safe host instructions through the `AGENTS.review.md` / `AGENTS.post-turn-review.md` hierarchy while preserving project-level `AGENTS.md` context.
