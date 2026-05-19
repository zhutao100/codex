# Steer And Queue Input Upstream Backport

This plan compares this branch with the upstream branch for the workflows around:

- Enter submission while an agent turn is running (`steer input`).
- Tab submission while an agent turn is running (`queued input`).
- Pending steer commit, interruption, pause, continuation, review, compact, and queue draining.

## Documents

- [`upstream_audit.md`](upstream_audit.md): code-path comparison and behavior-difference classification.
- [`problem_statement.md`](problem_statement.md): correctness and UX problems to solve in this branch.
- [`design_proposal.md`](design_proposal.md): minimal backport design that preserves this branch's `/pause` and `/continue` semantics.
- [`implementation_plan.md`](implementation_plan.md): phased implementation and regression-test plan.

## Scope

The goal is not to port the upstream TUI/app-server architecture wholesale. The goal is to backport the behavioral fixes that make steer input and queued input deterministic:

1. Enter while a regular turn is running becomes pending steer input, not an immediately rendered visible user turn.
2. Pending steer input is rendered only after core commits a user-message item.
3. Steer input is rejected for non-steerable active tasks such as review and compact, then retried as next-turn input.
4. Pending steers survive pause, interrupt, thread switch, and error paths.
5. Queued input remains next-turn input and drains only when there is no active or pending user turn.

## Non-goals

- Removing `/pause` and `/continue` from this branch.
- Replacing `Op::UserTurn` with the upstream app-server-first command path.
- Reworking the complete task/session architecture.
- Guaranteeing prefix-cache reuse after an accepted steer changes model-visible context.
