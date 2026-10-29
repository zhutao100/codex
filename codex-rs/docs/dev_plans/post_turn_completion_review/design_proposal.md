# Design Proposal

## Implementation Status

The baseline workflow is present: `Op::ReviewCompletedTurn`, `PostTurnCompletionReviewTask`, the dedicated prompt, read-only delegate configuration, output parsing, and continuation handoff. Coverage-driven review, prompt overrides, and review-scoped host instruction selection are also implemented.

The current compact history preserves multiple real user rounds, but collapses multiple review-triggered continuations within one user request into its latest assistant message. The former claim that it supplies the full multi-round interaction history was too broad. The current handoff also does not require a self-contained replacement final answer.

The proposed correction is [Bounded Review Context and Complete Final Answers](multi_round_context_and_final_answer.md): freeze genuine user context, retain an initial answer reference and exact latest candidate, exclude past reviewer verdicts, and require each follow-up final to stand alone. That document is authoritative for this follow-up's context, output, lifecycle, persistence, and budget rules. This update changes documentation only.

Post-turn review is already pause-aware: `/pause` during the delegate records a paused review continuation and emits `TurnPaused` without `ExitedReviewMode`; `/continue` restarts the reviewer instead of sampling the parent as ordinary main work. The new episode metadata must preserve that behavior.

## Target Base

This proposal targets this project's current customized code shape with the custom `model_overlay` feature and the custom `review_model_provider` delegate override already present.

## Design Summary

The sibling workflow `post_turn_completion_review` can be invoked manually through `/review-completed-turn` or automatically after regular `TurnComplete` when `[features].auto_post_turn_completion_review = true`. It spawns a read-only delegate using the existing review model/provider selection and can inject advisory developer input to continue the same main request. The proposed context now consists of a fixed genuine-user request frame, an optional historical initial answer, and the exact latest candidate; it does not accumulate every automated review round.

The implemented hardening layer updates generic `/review` and post-turn review delegate setup so each review task can use review-scoped host `AGENTS.*.md` instructions and optional prompt files from `~/.codex/config.toml`, while retaining the built-in prompts and project-level `AGENTS.md` docs as fallbacks.

The design intentionally keeps the delegate independent from the main agent's hidden reasoning and tool transcript. This makes the review useful for the specific blind points of context-efficient coding turns: missed files, partial reads, duplicated logic, final deliverable overclaims, and silent narrowing of the requested scope across a multi-round session.

The sections below retain the original workflow design and identify follow-up changes where applicable. For current implementation locations and document status, see [README.md](README.md).

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

This keeps existing `/review` clients compatible because `review_output` remains unchanged. `tui/src/chatwidget/replay.rs::on_exited_review_mode(...)` can render either `review_output` or `post_turn_completion_review_output` and keep the same start/finish banners.

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

### Current implementation and remaining defect

The existing internal state is:

```rust
pub(crate) struct CompletedTurnReviewRound {
    pub(crate) user_messages: Vec<String>,
    pub(crate) final_agent_message: String,
}

pub(crate) struct CompletedTurnForReview {
    pub(crate) turn_id: String,
    pub(crate) cwd: PathBuf,
    pub(crate) interaction_history: Vec<CompletedTurnReviewRound>,
    pub(crate) user_messages: Vec<String>,
    pub(crate) final_agent_message: String,
}
```

`SessionState.last_completed_regular_turn_for_review` caches the latest completed regular turn. `Session::on_task_finished` captures the task's final before the automatic review trigger. However, `capture_completed_regular_turn_for_review` rebuilds its compact history using `completed_turn_interaction_history_from_history` in `core/src/session/turn.rs`.

That helper pairs each real user boundary with the last nonempty assistant message before the next real user boundary. This preserves `U1/A0/U2/A1`, but `U1/A0/developer(R1)/A1` becomes `U1/A1`. It must not be used as the authoritative live source of review-episode completions.

### Proposed bounded episode

Capture the completed main task's returned final answer explicitly. Freeze its consumed user input and genuine earlier user context as a request frame. Preserve the first final as `initial_answer_reference` and replace a separate `candidate_final_answer` after each review-derived continuation. Keep at most those two answer texts for the current episode and serialize identical text once.

The reference is historical and may be wrong or incomplete. The candidate is the exact user-facing final and must stand alone. Preserve prior genuine user-round context for follow-ups such as "use option two," but do not append prior reviewer evaluations or earlier automated answer revisions. Original input and consumed steering belong to the current task; they must not also be reconstructed as earlier completed rounds.

Keep reasoning, tool calls/results, approvals, intermediate assistant text, and review verdicts out of delegate context. Do not introduce fake user messages or let advice redefine user intent.

The [follow-up design](multi_round_context_and_final_answer.md) defines request ownership, ticket identity, budget overflow, and persistence. Existing `TurnComplete` and `TurnContinued` events are not persisted, so reliable restart support requires explicit metadata. Compaction must preserve the snapshot; rollback and new input must restore or invalidate it. Legacy reconstruction must report uncertainty rather than assert an exact episode it cannot recover.

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

Proposed: pass a bounded explicit envelope as the delegate's only user input. The renderer and model prompt must use the same versioned contract:

```xml
<completed_turn_review_context version="2">
  <episode_id>...</episode_id>
  <request_revision>...</request_revision>
  <candidate_completion_id>...</candidate_completion_id>
  <cwd><![CDATA[...]]></cwd>
  <request_context provenance="exact">...genuine user context...</request_context>
  <initial_answer_reference status="historical_unverified"><![CDATA[...]]></initial_answer_reference>
  <candidate_final_answer><![CDATA[...exact latest main final...]]></candidate_final_answer>
  <context_coverage request="complete" candidate="complete" reference="included" />
</completed_turn_review_context>
```

Keep `InitialHistory::New`. The reviewer can inspect current files read-only but receives no parent transcript, prior reviewer verdicts, hidden reasoning, or tool transcript. The initial reference is not evidence of present correctness and cannot compensate for an incomplete latest candidate. For deduplication, token limits, missing legacy context, and exact field semantics, use [the bounded-context specification](multi_round_context_and_final_answer.md).


## Review Effectiveness Hardening

The post-turn reviewer must be optimized for complementarity, not for repeating the main session. The base prompt should explicitly state that the main session may already have used context-efficient keyword search and narrow range reads, and that the review delegate should use a different coverage-driven inspection strategy.

Minimum prompt requirements:

1. Treat the completed turn as an end-state artifact to verify, not as a request to perform a generic review.
2. Build the request-fulfillment checklist from genuine user context, applying later scope changes and cancellations. Treat earlier assistant promises as claims to assess rather than new user requirements.
3. Start by deriving a coverage checklist from the request-fulfillment checklist, the final assistant message, changed or untracked files when available, repository manifests, neighboring modules, tests, schemas, protocol definitions, generated bindings, and registration points.
4. Prefer whole-file reads for small and medium changed files. For large files, inspect the whole relevant symbol or module context plus imports, exports, registration tables, nearby tests, and paired helper functions. Do not rely only on `rg` hits followed by narrow `sed` ranges.
5. Use multiple orthogonal searches for duplicate or related logic: new symbol names, semantic concepts, config keys, protocol variants, UI labels, test names, file families, and neighboring directory structure.
6. For each suspected issue, cite concrete repository evidence in the `evaluation` text: file path, missing paired surface, conflicting existing helper, unsupported final-answer claim, unfulfilled user requirement, or test gap.
7. Treat `fix_actions_advised` as the concrete follow-up signal despite its historical bug-fix name. Set it to true for actionable bugs, incomplete scope, missed deliverables, or required verification follow-up.
8. Proposed follow-up: assess whether the latest candidate alone communicates the current result. A correct repository with a delta-only answer can still require follow-up. An earlier answer must not make an incomplete candidate pass.
9. Do not treat invalid output or unavailable mandatory context as a clean review; the two-field result is valid only when actually parsed as a nonempty evaluation.

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
        sandbox_policy: SandboxPolicy::new_read_only_policy(),
        disable_collab: true,
        instruction_profile: ReviewDelegateInstructionProfile::PostTurnCompletionReview,
    },
)?;
```

Use `run_codex_thread_one_shot(...)` from `core/src/codex_delegate.rs` exactly as `/review` does. Keep `InitialHistory::New`.


## Review-Scoped Host Instructions

`configure_review_delegate_config(...)` clones the parent config and then replaces `user_instructions` using `Config::load_host_instructions_from_filenames` and the selected review profile. `Codex::spawn(...)` still loads project-level instructions through `project_doc::get_user_instructions(...)`. This host separation is already implemented and should remain unchanged by the bounded-context work.

The original design used the following illustrative helper; the current implementation uses `Config::load_host_instructions_from_filenames` instead:

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
3. The current handler loads the cached completed turn or reconstructs it from history. The proposed path loads an exact bounded episode snapshot or explicitly qualified legacy context; an automatic operation must match its scheduled ticket.
4. If no valid context exists, emit a friendly `ErrorEvent` and return.
5. Build a `TurnContext` using `new_default_turn_with_sub_id(...)`, then override the review delegate config inside the task as described above.
6. Emit `EnteredReviewMode(ReviewRequest { target: ReviewTarget::Custom { ... }, user_facing_hint: Some("completed turn") })`.
7. Run the read-only delegate with the resolved post-turn prompt, the structured completed-turn context, and the post-turn review instruction profile.
8. The delegate follows the coverage-driven review protocol from the prompt rather than replaying the main session search strategy.
9. Parse the final delegate output. The current code normalizes invalid text to a false result; the proposed path distinguishes valid results from invalid/unavailable review internally.
10. For a valid parsed output, emit `ExitedReviewMode` with `post_turn_completion_review_output: Some(output.clone())`. Proposed: invalid/unavailable review instead emits an exit without structured post-turn output and an appropriate diagnostic.
11. For a valid nonempty evaluation with `fix_actions_advised = false`, finish without another main answer; the exact candidate already delivered remains the latest main final.
12. For a valid positive result, verify ticket ownership and the applicable automatic allowance, then inject advisory developer input and the complete replacement-answer contract before continuing without a new user message. A manually requested review may perform its single handoff without resetting an exhausted automatic allowance.

## Automatic Trigger

The existing `Session::on_task_finished` in `core/src/tasks/mod.rs` captures a regular completion, emits `TurnComplete`, then calls `maybe_auto_post_turn_completion_review`. That helper checks the regular task kind, feature flag, non-subagent source, nonempty answer, idle state, and available completed-turn cache before submitting `Op::ReviewCompletedTurn`.

A review-derived `ContinueTask` is regular and can trigger another review. The inspected implementation has no explicit review-iteration allowance or no-progress guard. A false verdict stops the loop, but malformed output also currently produces false; neither behavior should be described as a guaranteed successful finalization.

Proposed changes:

- Keep the automatic trigger on successful main completions and preserve the incomplete-work-notes exclusion.
- Associate internal review submissions with the expected episode/request/candidate ticket; do not let a queued operation silently select a newer completion.
- Bound retained episode context independently from the number of executions.
- Optionally land the finite automatic-attempt policy specified in [the follow-up design](multi_round_context_and_final_answer.md). Its proposed default is three reviewer attempts, with an explicit unresolved status at exhaustion. This is not current behavior or a measured optimum.
- Preserve manual invocation and the complete-answer contract for its advisory follow-up even when automatic review is disabled.

## Developer Message Handoff

When `fix_actions_advised` is true, wrap the evaluation in a developer message, not a user message:

```xml
<post_turn_completion_review>
  <context>An independent post-turn review inspected the completed turn. The review may be correct or incorrect. Treat it as advisory input, verify the claims against the repository, and only act on findings that are actually applicable.</context>
  <final_answer_contract>After applicable work, write a complete replacement answer to the active user request. Preserve still-valid results, incorporate corrections, remove superseded claims, identify deliverables and actual verification, and state unresolved limitations. The answer must stand alone without prior final messages or review feedback, including when all findings are rejected. Respect the requested format and brevity.</final_answer_contract>
  <fix_actions_advised>true</fix_actions_advised>
  <evaluation>
  ...review text...
  </evaluation>
</post_turn_completion_review>
```

Record it with `DeveloperInstructions::new(message).into()` and `Session::record_conversation_items(...)` so the next model request sees a developer-role item. Do not emit it as a visible user message.

Then continue the main session without recording a new user turn. The existing flow below is schematic: `checkpoint` has source `PostTurnCompletionReview`, the reviewed turn ID, model, pause reason, and regular target. The proposed ticket must also accompany this scheduling path:

```rust
// The review task records its fully populated PendingContinuation.
sess.set_pending_post_turn_completion_review_continuation(Some(checkpoint))
    .await;

// On review-task completion, the helper takes that pending checkpoint.
sess.continue_after_post_turn_completion_review(turn_context).await;
```

This helper can call the same `ContinueTask` / `continue_turn(...)` machinery, but `prepare_history_for_continuation(...)` must not remove an interrupt marker for `PostTurnCompletionReview`. The existing `remove_interrupted_abort` boolean already handles this if it remains `checkpoint.source == TurnContinuationSource::Interrupted`.

This preserves the distinction between a new user request and an advisory continuation. The continuation must also retain explicit episode ownership across pause/compaction and return a complete user-facing replacement answer. Do not rely on a real user boundary to retain prior completion context. Durable handoff and stale-ticket rules are specified in [the follow-up design](multi_round_context_and_final_answer.md).

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

In `tui/src/chatwidget/slash_dispatch.rs::dispatch_command(...)`:

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

Proposed prompt revision for the bounded envelope. The runtime prompt file is not changed by this documentation update. Configured prompt overrides remain supported; the task framing must still explain exact-candidate and historical-reference semantics:

```markdown
# Post-turn completion review guidelines

You are an independent reviewer for completed main work. Review the current repository/artifact end state and candidate_final_answer against the applicable genuine user request.

Your purpose is to catch issues that a context-efficient coding agent may miss: relevant files not found by keyword search, partial file reads that missed nearby contracts, duplicated existing functionality, incomplete paired updates, missing registration/schema/test changes, and final-answer claims that do not match repository state.

Do not merely repeat the main session strategy of keyword search followed by narrow range reads. The main session may already have used that path. Build an independent coverage checklist from the user request, final assistant message, changed or untracked files when available, repository manifests, neighboring modules, tests, schemas, protocol definitions, generated bindings, and registration points.

Prefer whole-file inspection for small and medium relevant files. For large files, inspect the whole relevant symbol or module context plus imports, exports, registration tables, nearby tests, and paired helper functions. Use multiple orthogonal searches for duplicate or related logic: new symbol names, semantic concepts, config keys, protocol variants, UI labels, test names, file families, and neighboring directory structure.

You may inspect files and run read-only commands. Do not modify files. Do not request write approval. Do not use web search. If inherited instructions conflict with this review methodology, this prompt wins for the post-turn review task.

Only use the explicit review context and current read-only inspection. Treat initial_answer_reference and earlier user-round answers as historical, potentially incorrect or superseded context. Do not assume access to hidden reasoning, tool calls, intermediate messages, or past reviewer findings.

The candidate_final_answer is the exact latest answer delivered to the user. Check whether it stands alone at the requested level of detail, incorporates material corrections, preserves still-required explanations and deliverables, and accurately states verification and unresolved work. A historical answer cannot compensate for a delta-only candidate. Distinguish incomplete communication from missing implementation by inspecting available evidence.

In the `evaluation` string, include concrete evidence for any finding: file path, missing paired surface, conflicting existing helper, unsupported final-answer claim, or test gap. A compact structure such as `Inspection coverage`, `Findings`, and `Fix actions advised` is preferred.

Return `fix_actions_advised = true` for concrete actionable defects, incomplete requested scope, missing deliverables, or an incomplete latest answer, including when repository changes are otherwise correct. Use `false` when no actionable follow-up is advised; it is not a proof of semantic completeness.

Output exactly this JSON object and no markdown fence:

{
  "evaluation": "free-form Markdown review result",
  "fix_actions_advised": false
}
```

## Parsing And Fallbacks

The current parser first tries the complete final text as `PostTurnCompletionReviewOutputEvent`, then an extracted JSON substring. On failure it renders raw text with `fix_actions_advised = false`; empty output also becomes false. This is existing behavior, not a verified clean verdict.

Proposed: retain the public JSON object but represent parser/execution failure separately inside the task. A valid result requires successfully parsed fields and a nonempty evaluation. Invalid or missing output, unavailable mandatory context, and delegate failure must render a diagnostic or an exit without structured review output rather than a synthetic no-follow-up verdict. Do not infer a positive handoff from raw prose.

No additional semantic-certification field is required. The exact candidate and complete context must be identified by the host, while the model assesses whether the work and answer are actually complete.

## Failure Handling

The table combines the current baseline with proposed hardening. Ticket ownership, context budgets, invalid-result status, and a finite attempt allowance are proposed changes; the complete lifecycle specification is in the follow-up design.

|Failure|Behavior|
|---|---|
|Fresh session or no completed regular turn|Emit `ErrorEvent` with the friendly guidance above; do not enter review mode.|
|Active task is running|Emit `Cannot review a completed turn while another task is running.`|
|Last completed regular turn has no final assistant message|Emit `The last completed turn has no final assistant message to review.`|
|Delegate spawn fails|Emit `ErrorEvent` with prefix `Post-turn completion review failed`.|
|Delegate interrupted|Emit `ExitedReviewMode` without post-turn output; do not inject advice. A pause preserves the review continuation and does not emit a false review-finished event.|
|Delegate output unparsable or empty|Proposed: report invalid/unavailable review without a synthetic clean verdict; do not continue from raw prose.|
|Required context exceeds the review budget|Omit optional historical reference first; if mandatory scope/candidate still do not fit, report unavailable coverage.|
|Request/candidate changed while review ran|Discard stale side effects; do not inject advice into newer work.|
|Automatic allowance exhausted, if implemented|Keep current answer and unresolved findings visible; do not mark review clean.|
|Developer-message continuation cannot be queued or started|Current enqueue failure logs a warning. Proposed: report the failure visibly, retain the prepared handoff for defined recovery, and avoid an uncontrolled retry loop.|

## Original Workflow Implementation Sequence

These steps record the baseline already present. The bounded-context follow-up has its own implementation sequence in [multi_round_context_and_final_answer.md](multi_round_context_and_final_answer.md).

1. Add protocol types: `Op::ReviewCompletedTurn`, `PostTurnCompletionReviewOutputEvent`, optional field on `ExitedReviewModeEvent`, and `TurnContinuationSource::PostTurnCompletionReview`.
2. Add `Feature::AutoPostTurnCompletionReview` and its `FeatureSpec` with key `auto_post_turn_completion_review`, `Stage::UnderDevelopment`, and `default_enabled = false`.
3. Keep `core/review_prompt.md` and `core/post_turn_completion_review_prompt.md` as built-in fallbacks and add config-backed prompt-file loading for `review_prompt_file` and `post_turn_completion_review_prompt_file`.
4. Add `CompletedTurnForReview` session state and capture it for completed regular turns.
5. Extract shared review delegate config setup from `core/src/tasks/review.rs` into a helper and update `/review` to use the `Review` instruction profile.
6. Add review host filename config keys and the delegate host-instruction loader with the `/review` and post-turn `AGENTS.*.md` hierarchies.
7. Add `core/src/tasks/post_turn_completion_review.rs` and wire it into `core/src/tasks/mod.rs`.
8. Add `handlers::review_completed_turn(...)` in `core/src/session/handlers.rs` and route `Op::ReviewCompletedTurn` to it.
9. Add automatic trigger after regular `TurnComplete` when the feature is enabled.
10. Add developer-message injection and post-completion continuation helper.
11. Add TUI slash command, dispatch, task-running handling, and post-turn review rendering.
12. Regenerate the config schema through the repository-prescribed `just-local` wrapper after adding `ConfigToml` keys.
13. Add tests.

## Test Plan

### Core tests

- `ReviewCompletedTurn` on a fresh session emits a friendly `BadRequest` error and does not spawn a task.
- A completed regular turn captures authoritative terminal text; proposed episode state retains genuine user context, a historical initial answer, and the exact latest candidate.
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
- Proposed: an unparsable/empty delegate response is unavailable/invalid review, not a valid no-follow-up verdict.
- Automatic review triggers after regular main-session `TurnComplete` only when the feature is enabled.
- Automatic review does not trigger for review tasks, compact tasks, user-shell tasks, or sub-agent sessions.

### Proposed same-request multi-review regressions

- Exercise `U1 -> A0 -> R1(true) -> A1 -> R2` without any intervening user message. The second reviewer gets the frozen request, initial reference, and latest candidate, excluding `R1` and tool/reasoning history.
- Repeat ten synthetic continuations and retain at most two episode answer texts; deduplicate identical first/latest answers.
- Ensure every advisory handoff contains the replacement-answer contract, including all-findings-rejected and no-files-changed cases.
- Treat a delta-only latest answer as a communication defect even when the reference is comprehensive.
- Cover consumed steering, stale queued operations, pause/restart, compaction, rollback/fork, legacy provenance, invalid output, and mandatory-context overflow.
- Use behavioral evaluations for semantic completeness; mocked outputs prove harness behavior only.

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
- `core/src/session/{handlers,mod,turn,review}.rs`: operation routing, capture/reconstruction, snapshot ownership, and review setup.
- `core/src/rollout/*` and rollout type/schema consumers: narrow persisted episode metadata, replay, rollback, and fork handling.
- `core/src/tasks/mod.rs`: `TaskKind` additions and task lifecycle interactions.
- `tui/src/slash_command.rs` and `tui/src/chatwidget/{slash_dispatch,replay,streaming,turn_runtime}.rs`: command dispatch, task state, answer/copy behavior, and review rendering.
- `core/templates/review/*`: no change required unless the implementation chooses to add post-turn-specific exit templates.

## Design direction

Keep the existing isolated post-turn reviewer, review-specific instructions, and advisory continuation flow. Add an explicit bounded episode snapshot and require the main model to produce a complete current answer after every review handoff. The historical reference restores useful context without introducing a transcript of prior verdicts; the latest candidate alone must satisfy the user's communication needs. The detailed decision, alternatives, limits, and lifecycle requirements are in [multi_round_context_and_final_answer.md](multi_round_context_and_final_answer.md).
