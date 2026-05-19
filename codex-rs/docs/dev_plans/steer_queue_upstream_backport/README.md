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
4. Pending steers survive pause, interrupt, review/compact rejection, and recoverable error paths covered by this branch's in-place TUI lifecycle.
5. Queued input remains next-turn input and drains only when there is no active or pending user turn.

## Implementation Status

Implemented required phases:

- Core validates same-turn steering through `Session::steer_input(...)` and emits structured `ActiveTurnNotSteerable` errors for review, compact, post-turn review, and standalone user-shell turns.
- The TUI stores Enter steers as pending input while a turn is running, renders them only after live committed `UserMessage` events, and keeps Tab input as next-turn queued input.
- Rejected steers are retained separately and drained before normal queued messages.
- Queue auto-drain is gated while a submitted or continued turn is waiting for `TurnStarted`.
- Pending steers are recoverable across pause/interruption cleanup paths.

Optional phases not implemented here:

- Queue action semantics for queued slash/shell prompts.
- External app-server `turn/steer`.

## Non-goals

- Removing `/pause` and `/continue` from this branch.
- Replacing `Op::UserTurn` with the upstream app-server-first command path.
- Reworking the complete task/session architecture.
- Guaranteeing prefix-cache reuse after an accepted steer changes model-visible context.
