# Problem Statement

## Status

Initial workflow implemented in this project; follow-up review-effectiveness hardening proposed.

## Scenario

A normal Codex coding turn is intentionally optimized for context-window efficiency: the agent infers the task, uses keyword search to find likely relevant files, reads targeted ranges, and applies small diff-style edits. This usually produces fast and useful changes, but it can miss project-level context that was not reachable from the initial keywords or the ranges that were read.

The proposed workflow adds an independent read-only review after a turn has completed. It reviews the completed turn's end-state and final deliverables, not the in-flight reasoning or tool transcript, and it can feed actionable concerns back into the same main session without requiring a new user message.

## Test-Run Findings

Test runs showed that post-turn review delegates often inspected the repository the same way as the main coding session: infer intent, run keyword searches, read narrow ranges, and judge from those slices. This weakens the workflow because the reviewer can repeat the same discovery path and miss the same adjacent files, paired updates, existing helpers, and repository-level consistency checks.

Two likely causes are now in scope:

- `core/post_turn_completion_review_prompt.md` states the high-level review purpose but does not require a different inspection methodology.
- Review delegates inherit the same host-level and project-level `AGENTS.md` instruction stream through the normal instruction-loading path. Project-level instructions are still valuable, but host-level instructions can contain main-session editing and efficient-search guidance that is counterproductive for an independent review.
- The review prompts are currently built in through `include_str!`, so changing `core/review_prompt.md` or `core/post_turn_completion_review_prompt.md` requires a rebuild rather than a host-level config change.

## Risks To Address

|Risk|Typical cause|Desired review behavior|
|---|---|---|
|Missed relevant files|The main agent used keyword search and did not discover adjacent implementations, registration sites, tests, generated schemas, or platform-specific variants.|Search and inspect the current repository state read-only, then call out missed updates when they are concrete and actionable.|
|Partial context from range reads|The main agent read only selected line ranges and missed invariants, nearby helpers, module-level contracts, or paired methods.|Re-read whole relevant files or broader symbol/module context when needed, then assess whether the final change is internally consistent.|
|Reinvented duplicate logic|The main agent did not search broadly enough for existing helpers or patterns before adding new code.|Identify duplicate or conflicting logic and advise reusing the existing project component when the evidence is strong.|
|Final answer overclaims|The final agent message may claim tests, guarantees, or implementation scope that the repository state does not support.|Compare the final user-facing deliverable against repository state and report mismatches.|
|Unsafe follow-up assumptions|A successful-looking turn may leave a subtle required follow-up that the main session did not know to perform.|Produce an advisory result with an explicit binary `fix_actions_advised` signal so the main session can decide whether to continue.|
|Reviewer repeats the main search path|The review prompt and inherited host instructions encourage context-efficient keyword/range inspection, which is the same strategy that caused the missed context.|Require the reviewer to build an independent coverage plan, inspect changed files and paired surfaces broadly, and search for duplicate or related implementations using multiple orthogonal signals.|
|Main-session host instructions leak into review|Host-level `AGENTS.md` may mix machine resource notes with editing and efficiency instructions intended for normal work sessions.|Introduce review-scoped host instruction loading so `/review` prefers `AGENTS.review.md` and post-turn review prefers `AGENTS.post-turn-review.md`, with explicit fallback to shared host instructions.|
|Review prompt changes require rebuilds|The built-in review prompts are compiled into the binary.|Add `review_prompt_file` and `post_turn_completion_review_prompt_file` config keys so users can supply prompt files from disk at runtime.|

## Existing Mechanics In This Project

### `/review` delegate workflow

The existing `/review` flow already has most of the process isolation and UI lifecycle needed for an independent reviewer:

- `protocol/src/protocol.rs` defines `Op::Review`, `ReviewRequest`, `ReviewTarget`, `EnteredReviewMode`, `ExitedReviewMode`, and `ReviewOutputEvent`.
- `tui/src/slash_command.rs` exposes `SlashCommand::Review`; `tui/src/chatwidget.rs` dispatches it through `open_review_popup(...)` or inline `/review ...` custom instructions.
- `core/src/review_prompts.rs` resolves `ReviewTarget` into a concrete review prompt and user-facing hint.
- `core/src/codex.rs::handlers::review(...)` builds a default `TurnContext`, resolves the request, and calls `spawn_review_thread(...)`.
- `core/src/codex.rs::spawn_review_thread(...)` selects `review_model`, applies `review_model_provider` when configured, disables web search features, sets `WebSearchMode::Disabled`, builds review-local tools config, and starts `ReviewTask`.
- `core/src/tasks/review.rs` clones the parent config, sets `base_instructions = REVIEW_PROMPT`, inherits the parent `Config::user_instructions`, disables web search and collab, sets `approval_policy = Never`, applies `review_model` / `review_model_provider`, and uses `core/src/codex_delegate.rs::run_codex_thread_one_shot(...)`.
- `core/src/codex_delegate.rs` is already a reusable delegate harness: it spawns a sub-Codex session, forwards non-approval events, routes approvals through the parent session, and shuts the delegate down after `TurnComplete`, `TurnAborted`, or `TurnPaused`.
- `tui/src/chatwidget.rs::on_entered_review_mode(...)` and `tui/src/chatwidget.rs::on_exited_review_mode(...)` already provide the visible review-mode lifecycle banners and review output rendering.

These pieces should be reused where their semantics match. The new workflow should not duplicate the delegate runner or review-mode UI plumbing unless the existing protocol shape cannot represent the new result.

### Current limitations for post-turn completion review

The existing `/review` task reviews uncommitted changes, a base-branch diff, a commit, or custom review instructions. It does not know how to identify the last completed Codex turn, extract only that turn's user messages and final assistant message, or continue the already completed main turn based on an independent advisory review.

The existing `ReviewOutputEvent` is code-review-shaped: it has structured `findings`, `overall_correctness`, `overall_explanation`, and `overall_confidence_score`. The new workflow needs a looser end-state evaluation plus a machine-readable binary signal, `fix_actions_advised`, because its primary downstream consumer is the main session continuation path rather than an inline code-review UI.

The existing `ContinueTask` path is designed for paused or interrupted turns. It can continue without adding a new user turn, but its checkpoint source is currently limited to `Paused` and `Interrupted`. A post-completion review handoff needs a new continuation source or a sibling continuation helper so the main session can continue from an advisory developer message without treating it as a fresh user prompt.

## Requirements

|Requirement|Assessment|
|---|---|
|Manual invocation through `/review-completed-turn`|Add a TUI slash command and a core operation, rather than overloading `/review`, because this workflow validates completed-turn state and has different safety semantics.|
|Automatic invocation through `auto_post_turn_completion_review`|Add `Feature::AutoPostTurnCompletionReview` with key `auto_post_turn_completion_review`, `Stage::UnderDevelopment`, and `default_enabled = false`.|
|Respect `review_model` and `review_model_provider`|Reuse the same selection and provider override path as `/review`; if a provider override is configured, disable `Feature::RemoteModels` for the delegate.|
|Always disable web search|Force `WebSearchMode::Disabled` and disable `Feature::WebSearchRequest` / `Feature::WebSearchCached` in the delegate config.|
|Always run read-only|Force the delegate `sandbox_policy` to `SandboxPolicy::ReadOnly` regardless of the main session policy, and keep approval policy at `AskForApproval::Never` to prevent escalation into writes.|
|Use a dedicated prompt file|Add a prompt file separate from `core/review_prompt.md`, for example `core/post_turn_completion_review_prompt.md`.|
|Pass only user messages and final agent messages as context|Capture or reconstruct a compact completed-turn context and pass no reasoning, tool calls, tool outputs, approval events, or intermediate assistant messages.|
|Produce a loose evaluation plus a binary fix signal|Add a small output type such as `PostTurnCompletionReviewOutputEvent { evaluation: String, fix_actions_advised: bool }`.|
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

## Conclusion

The implementation should remain a small sibling workflow beside `/review`: reuse the review model/provider override, `codex_delegate`, review-mode UI lifecycle, and `ReviewRequest`/`ReviewTarget::Custom` where practical, but keep hardening the prompt and instruction-loading path so reviewers perform independent, coverage-driven review rather than replaying the main session strategy. The host-level split should be explicit: generic `/review` uses review-scoped host instructions when available, post-turn review can specialize further, and both prompts can be overridden from configured files without rebuilding.
