# Post-Turn Completion Review Design Notes

## Current status

These notes describe the customized `codex-rs_v0.98` project as inspected on 2026-10-29 and a proposed follow-up for repeated review continuations. The accompanying archive contains documentation changes only; it does not implement the proposal or change runtime prompts.

|Area|Status in the inspected source|
|---|---|
|Manual `/review-completed-turn` and feature-gated automatic review|Implemented|
|Fresh one-shot delegate, read-only inspection, review model/provider selection|Implemented|
|Coverage-driven prompt, runtime prompt overrides, review-scoped host instructions|Implemented|
|Developer-advice handoff, no-new-user-input continuation, review pause/continue|Implemented|
|Compact history across separate real user requests|Implemented|
|Retention of earlier finals within a same-user review loop|Defective: each continuation overwrites the reconstructed final for that user segment|
|Explicit requirement for a complete replacement final answer|Missing|
|Bounded episode snapshot, exact request/candidate ownership, durable replay|Proposed|
|Separate invalid-review status and explicit payload budget|Proposed|
|Finite automatic review-attempt allowance|Separate proposed hardening; not implemented|

## Read these documents

|Document|Purpose|
|---|---|
|[problem_statement.md](problem_statement.md)|Original review motivation, observed same-request failure, and updated requirements|
|[design_proposal.md](design_proposal.md)|Baseline architecture, configuration, protocol, and integration points; updated to refer to the bounded follow-up|
|[review_effectiveness_hardening.md](review_effectiveness_hardening.md)|Implemented independent-inspection and instruction-profile work, with the new answer-completeness boundary|
|[multi_round_context_and_final_answer.md](multi_round_context_and_final_answer.md)|Source-grounded root causes, comparative options, recommended state/payload/answer contracts, lifecycle and persistence rules, and regression plan|

The last document governs the proposed follow-up where older assumptions conflict. Its types, new rollout record, prompt wording, and attempt allowance are design proposals, not existing runtime behavior.

## Recommended change

Freeze genuine user context for an episode. Retain the first main final as a historical reference and replace a separate latest-candidate slot after each follow-up. Send at most those two episode answer texts to a fresh reviewer, omitting all prior reviewer verdicts. Require the latest main answer to be a complete replacement for the active user request, and have the reviewer check it as a user deliverable independently of the historical reference.

The reference is not accepted or authoritative. It restores context for the reported failure but cannot recover arbitrary omitted intermediate corrections. The replacement-answer contract must carry those results into the current candidate. The harness can enforce ownership, text identity, and retention bounds; semantic completeness still requires model behavior and independent assessment.

## Source entry points

|Concern|Source|
|---|---|
|Current user-boundary reconstruction|[core/src/session/turn.rs](../../../core/src/session/turn.rs)|
|Current completed-turn cache and history replacement|[core/src/session/mod.rs](../../../core/src/session/mod.rs), [state/session.rs](../../../core/src/state/session.rs)|
|Review input, output parser, and advisory prompt|[core/src/tasks/post_turn_completion_review.rs](../../../core/src/tasks/post_turn_completion_review.rs)|
|Automatic review and continuation lifecycle|[core/src/tasks/mod.rs](../../../core/src/tasks/mod.rs), [continue_task.rs](../../../core/src/tasks/continue_task.rs)|
|Fresh delegate harness|[core/src/codex_delegate.rs](../../../core/src/codex_delegate.rs)|
|Persistence policy|[core/src/rollout/policy.rs](../../../core/src/rollout/policy.rs), [recorder.rs](../../../core/src/rollout/recorder.rs)|
|TUI answer/copy behavior|[streaming.rs](../../../tui/src/chatwidget/streaming.rs), [turn_runtime.rs](../../../tui/src/chatwidget/turn_runtime.rs), [replay.rs](../../../tui/src/chatwidget/replay.rs)|
|Existing integration coverage|[core/tests/suite/review.rs](../../../core/tests/suite/review.rs)|

## Applying the documentation archive

The archive preserves `docs/dev_plans/post_turn_completion_review/` paths. Extract it at the repository root to add this index and the new follow-up design and replace the three updated existing documents:

```bash
tar -xzf /path/to/post_turn_completion_review_docs.tar.gz -C /path/to/codex-rs
```

No source code, runtime prompt, generated binding, or config file is included. Relative source links are intended to resolve after the files are placed in the repository.

## Inspection and validation scope

The conclusions come from the supplied customized source archive, not an assumption about current upstream Codex. Relevant design notes, implementation paths, persistence policy, TUI handlers, and tests were inspected. Existing tests were read to identify coverage gaps; no Rust build, test suite, or model behavior test was run for this documentation-only update.

Documentation checks cover relative links against the source tree, Markdown structure, changed-file scope, and archive contents. The new validation matrix describes future implementation tests and explicitly separates deterministic harness checks from semantic model evaluations.
