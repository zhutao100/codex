# Problem Statement

## Status

The initial workflow and review-effectiveness hardening are implemented in this project. Same-request review-loop context and latest-answer completeness remain unresolved in the inspected archive. The proposed follow-up is documented in [multi_round_context_and_final_answer.md](multi_round_context_and_final_answer.md); no runtime changes are included in this documentation update.

## Scenario

A normal Codex coding turn is intentionally optimized for context-window efficiency: the agent infers the task, uses keyword search to find likely relevant files, reads targeted ranges, and applies small diff-style edits. This usually produces fast and useful changes, but it can miss project-level context that was not reachable from the initial keywords or the ranges that were read.

The proposed workflow adds an independent read-only review after a turn has completed. It reviews the completed turn's end-state and final deliverables against genuine user context and explicitly selected final-answer snapshots, without the in-flight reasoning or tool transcript. It can feed actionable concerns back into the same main session without requiring a new user message.

## Test-Run Findings

Earlier test runs showed that delegates repeated the main session's keyword-search and narrow-range inspection. The implemented hardening adds coverage-driven review, review-scoped host instructions, and configurable prompts; [review_effectiveness_hardening.md](review_effectiveness_hardening.md) records that work.

The remaining multi-round failure involves one real user request followed by several automated review continuations:

```text
U1 -> A0 -> developer(R1) -> A1 -> developer(R2) -> A2
```

The current reconstruction starts a new round only at a real user boundary. Each subsequent assistant message overwrites that round's previous final. Thus the next reviewer sees `U1 + A1` or `U1 + A2`, even when `A0` contains the comprehensive original answer. The existing support for `U1 -> A0 -> U2 -> A1` does not fix this same-user review loop.

There is a second, independent communication defect: the advisory handoff asks the main model to verify and address findings but does not require a complete replacement final answer. A delta-only follow-up becomes the newest main answer in the TUI and the latest copyable response.

Source-confirmed causes:

- [completed_turn_interaction_history_from_history](../../../core/src/session/turn.rs) groups by real user inputs and replaces the current assistant answer; developer review advice is not a new boundary.
- [capture_completed_regular_turn_for_review](../../../core/src/session/mod.rs) rebuilds that projection after each continuation and replaces the cached completed turn.
- [render_advisory_developer_message](../../../core/src/tasks/post_turn_completion_review.rs) has no self-contained replacement-answer contract.
- The [review prompt](../../../core/post_turn_completion_review_prompt.md) does not explicitly require the latest answer to stand alone, independently of earlier finals.
- [on_task_complete](../../../tui/src/chatwidget/turn_runtime.rs) keeps the latest nonempty final as the copyable main answer; it does not merge earlier responses.
- The [multi-round integration test](../../../core/tests/suite/review.rs) introduces a new user message between finals and never checks a second reviewer after a same-request continuation.

The [delegate runner](../../../core/src/codex_delegate.rs) is not losing a supplied transcript: the post-turn task intentionally supplies a rendered snapshot with `InitialHistory::New`. Preserve that fresh context. Sending the whole parent transcript would address the symptom by defeating the review's independence.

The desired fix must address both missing review context and incomplete final communication. See the [options and recommended design](multi_round_context_and_final_answer.md) for the bounded initial-reference/latest-candidate approach, the replacement-answer contract, persistence, and validation cases.

## Risks To Address

|Risk|Typical cause|Desired review behavior|
|---|---|---|
|Missed relevant files|The main agent used keyword search and did not discover adjacent implementations, registration sites, tests, generated schemas, or platform-specific variants.|Search and inspect the current repository state read-only, then call out missed updates when they are concrete and actionable.|
|Partial context from range reads|The main agent read only selected line ranges and missed invariants, nearby helpers, module-level contracts, or paired methods.|Re-read whole relevant files or broader symbol/module context when needed, then assess whether the final change is internally consistent.|
|Reinvented duplicate logic|The main agent did not search broadly enough for existing helpers or patterns before adding new code.|Identify duplicate or conflicting logic and advise reusing the existing project component when the evidence is strong.|
|Final answer overclaims|The final agent message may claim tests, guarantees, or implementation scope that the repository state does not support.|Compare the final user-facing deliverable against repository state and report mismatches.|
|Incomplete requested scope|A turn can silently narrow the request, omit deliverables, or lose an applicable constraint.|Build the checklist from genuine user context, applying later amendments and cancellations; assess current artifacts and the exact latest candidate.|
|Lost initial answer in a review loop|No new real user boundary separates main follow-ups, so reconstruction overwrites the earlier answer.|Retain an explicit initial answer reference and replace a separate latest-candidate slot.|
|Delta-only final answer|The latest advisory becomes the focus of the next main final.|Require a self-contained replacement final even when all findings are rejected; review communication completeness separately.|
|Accumulated review context|Passing every review/follow-up biases later reviewers and increases prompt size.|Keep at most two answer texts per episode, excluding all earlier reviewer verdicts; enforce a separate payload budget.|
|Stale or missing completion state|Compaction, resume, rollback, or new user input changes the usable history.|Persist narrow episode metadata, track consumed input and candidate identity, and invalidate stale review tickets.|
|Unsafe follow-up assumptions|A successful-looking turn may leave a subtle required follow-up that the main session did not know to perform.|Produce an advisory result with an explicit binary `fix_actions_advised` signal so the main session can decide whether to continue.|
|Reviewer repeats the main search path|The review prompt and inherited host instructions encourage context-efficient keyword/range inspection, which is the same strategy that caused the missed context.|Require the reviewer to build an independent coverage plan, inspect changed files and paired surfaces broadly, and search for duplicate or related implementations using multiple orthogonal signals.|
|Main-session host instructions leak into review|Host-level `AGENTS.md` may mix machine resource notes with editing and efficiency instructions intended for normal work sessions.|Introduce review-scoped host instruction loading so `/review` prefers `AGENTS.review.md` and post-turn review prefers `AGENTS.post-turn-review.md`, with explicit fallback to shared host instructions.|
|Review prompt changes require rebuilds|The built-in review prompts are compiled into the binary.|Add `review_prompt_file` and `post_turn_completion_review_prompt_file` config keys so users can supply prompt files from disk at runtime.|

## Existing Mechanics In This Project

### `/review` delegate workflow

The existing `/review` flow already has most of the process isolation and UI lifecycle needed for an independent reviewer:

- `protocol/src/protocol.rs` defines `Op::Review`, `ReviewRequest`, `ReviewTarget`, `EnteredReviewMode`, `ExitedReviewMode`, and `ReviewOutputEvent`.
- `tui/src/slash_command.rs` exposes `SlashCommand::Review`; `tui/src/chatwidget/slash_dispatch.rs` dispatches it through `open_review_popup(...)` or inline `/review ...` custom instructions.
- `core/src/review_prompts.rs` resolves `ReviewTarget` into a concrete review prompt and user-facing hint.
- `core/src/session/handlers.rs::review(...)` builds a default `TurnContext`, resolves the request, and calls `spawn_review_thread(...)`.
- `core/src/session/review.rs::spawn_review_thread(...)` selects `review_model`, applies `review_model_provider` when configured, disables web search features, sets `WebSearchMode::Disabled`, builds review-local tools config, and starts `ReviewTask`.
- `core/src/tasks/review.rs` clones the parent config, sets base instructions from `Config::review_prompt()` with `REVIEW_PROMPT` as the fallback, reloads host instructions through the review-specific profile, disables web search and collab, sets `approval_policy = Never`, applies `review_model` / `review_model_provider`, and uses `core/src/codex_delegate.rs::run_codex_thread_one_shot(...)`.
- `core/src/codex_delegate.rs` is already a reusable delegate harness: it spawns a sub-Codex session, forwards non-approval events, routes approvals through the parent session, and shuts the delegate down after `TurnComplete`, `TurnAborted`, or `TurnPaused`.
- `tui/src/chatwidget/replay.rs::on_entered_review_mode(...)` and `tui/src/chatwidget/replay.rs::on_exited_review_mode(...)` already provide the visible review-mode lifecycle banners and review output rendering.

These pieces should be reused where their semantics match. The new workflow should not duplicate the delegate runner or review-mode UI plumbing unless the existing protocol shape cannot represent the new result.

### What is implemented and what remains

`Op::ReviewCompletedTurn`, `PostTurnCompletionReviewTask`, the two-field review output, and `TurnContinuationSource::PostTurnCompletionReview` are already implemented. Manual and automatic invocation, read-only review, developer-advice handoff, and pause-aware review continuation are present.

The remaining limitations are the collapsed same-request history, the missing replacement-answer contract, and the absence of an explicit durable episode snapshot. The existing `ReviewOutputEvent` remains specific to ordinary code review; this follow-up keeps `PostTurnCompletionReviewOutputEvent { evaluation, fix_actions_advised }` and improves its use without replacing the delegate harness.

The requirements below include the existing baseline and the proposed follow-up. Current implementation status is summarized in [README.md](README.md).

## Requirements

|Requirement|Assessment|
|---|---|
|Manual invocation through `/review-completed-turn`|Add a TUI slash command and a core operation, rather than overloading `/review`, because this workflow validates completed-turn state and has different safety semantics.|
|Automatic invocation through `auto_post_turn_completion_review`|Add `Feature::AutoPostTurnCompletionReview` with key `auto_post_turn_completion_review`, `Stage::UnderDevelopment`, and `default_enabled = false`.|
|Respect `review_model` and `review_model_provider`|Reuse the same selection and provider override path as `/review`; if a provider override is configured, disable `Feature::RemoteModels` for the delegate.|
|Always disable web search|Force `WebSearchMode::Disabled` and disable `Feature::WebSearchRequest` / `Feature::WebSearchCached` in the delegate config.|
|Always run read-only|Force the delegate `sandbox_policy` to `SandboxPolicy::ReadOnly` regardless of the main session policy, and keep approval policy at `AskForApproval::Never` to prevent escalation into writes.|
|Use a dedicated prompt file|Add a prompt file separate from `core/review_prompt.md`, for example `core/post_turn_completion_review_prompt.md`.|
|Bound review context without losing user scope|Freeze genuine user context; retain an initial answer reference and exact latest candidate for the current episode. Do not pass prior reviewer verdicts, reasoning, tools, approvals, or intermediate assistant messages.|
|Deliver a complete latest answer|Every review-derived continuation must answer the full active user request at the requested level of detail, incorporating still-valid results and corrections.|
|Review answer completeness|The latest candidate must stand alone; the initial reference cannot compensate for omissions.|
|Preserve ownership across lifecycle changes|Persist episode/candidate/request identity, distinguish consumed from pending input, and restore or invalidate it on pause, resume, compaction, rollback, and fork.|
|Distinguish unavailable review from no advised follow-up|Invalid output, unavailable required context, and execution failure must not appear as a clean verdict.|
|Produce a loose evaluation plus a binary follow-up signal|Add a small output type such as `PostTurnCompletionReviewOutputEvent { evaluation: String, fix_actions_advised: bool }`; keep the historical field name but define it as true for concrete bug fixes or incomplete-scope follow-up.|
|Feed fix advice back as a developer message|When `fix_actions_advised` is true, wrap the evaluation in a developer message that labels it as independent advisory input and instructs the main model to verify before acting.|
|Continue the main session instead of starting a new user turn|Reuse the no-new-user-input continuation machinery where possible, but add a post-completion continuation source to avoid pretending this was an interrupt or pause.|
|Sanity-check misuse|Fresh sessions, active turns, sessions with no completed regular turn, or completed turns without a final assistant message should emit user-friendly errors and not spawn a delegate.|
|Harden review methodology|The prompt should explicitly forbid merely replaying the main session search style and should require coverage-driven inspection of changed files, adjacent implementations, paired registrations, tests, schemas, and duplicate logic.|
|Scope host instructions for review|Keep project-level `AGENTS.md` available, but reload host-level instructions for review delegates using the review-specific `AGENTS.*.md` hierarchy.|
|Generic `/review` host hierarchy|For `core/src/tasks/review.rs`, prefer `~/.codex/AGENTS.review.md` when present and non-empty; otherwise fall back to `~/.codex/AGENTS.md`.|
|Post-turn host hierarchy|For post-turn completion review, prefer `~/.codex/AGENTS.post-turn-review.md`, then `~/.codex/AGENTS.review.md`, then `~/.codex/AGENTS.md`.|
|Configurable host filenames|Add `host_agents_filename`, `review_agents_filename`, and `post_turn_completion_review_agents_filename` keys in `~/.codex/config.toml` to override the hierarchy filenames while keeping them resolved under `codex_home`.|
|Configurable review prompts|Add `review_prompt_file` and `post_turn_completion_review_prompt_file` keys in `~/.codex/config.toml` to override `core/review_prompt.md` and `core/post_turn_completion_review_prompt.md` from on-disk files at runtime.|

## Non-goals

- Do not make the post-turn reviewer a general PR review replacement.
- Do not expose hidden reasoning, tool call details, raw approvals, or intermediate sub-agent transcript to the review delegate.
- Do not allow the delegate to modify files, request write escalation, or use web search.
- Do not make automatic review enabled by default.
- Do not require server-side changes.
- Do not change the primary session's model provider or auth mode; only the review delegate should use the review-specific provider override.

## Design direction

Keep the existing sibling workflow beside `/review`, including its model/provider selection, fresh `codex_delegate` session, read-only inspection, and review-scoped instructions. Add bounded explicit completion context and a complete replacement-answer contract together. The [follow-up design](multi_round_context_and_final_answer.md) defines the recommended behavior and separates deterministic harness guarantees from the semantic completeness that the model and reviewer must assess.
