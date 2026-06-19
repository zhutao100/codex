# Steer And Queue Upstream Backport

This proposal compares the steer-input and queue-input workflows in this branch with the upstream branch and defines a branch-local backport plan.

## Executive conclusion

The most visible upstream steer fixes are already present in this branch:

- Enter during an active regular turn creates a pending steer instead of rendering a committed user message immediately.
- A pending steer is rendered only when core emits the committed user-message event.
- Review, compact, post-turn review, and standalone user-shell turns reject steering through structured `ActiveTurnNotSteerable` errors.
- Rejected steers drain before ordinary queued messages.
- Queue auto-send is blocked while a submitted turn is waiting for `TurnStarted`.
- `/pause`, `/continue`, interrupt, and error paths retain or recover uncommitted pending steers.

This backport implements the high-value upstream work that applies to this branch:

1. Preserve whether queued input is plain text, a slash command, or a shell command, and defer command execution until dequeue.
2. Preserve typed user input through core pending-input storage and emit the complete committed user-message lifecycle when a task finishes with undrained steer input.
3. Separate `turn start` from `turn steer`, carry an expected active-turn id, and avoid applying new-turn settings before a steer is accepted.
4. Add an exact request-input prefix regression test for the normal no-compaction steer path.
5. Backport selected lifecycle hardening: plan-stream and user-shell queue guards, merged rejected steers, and safer abort cleanup.

## Answer to the prefix-cache observation

An accepted steer is intentionally non-preemptive. It does not discard response items already produced by the active model request. At the next model sampling boundary, the expected input shape is:

```text
previous request input
+ committed reasoning/assistant/tool-call/tool-output items
+ accepted steer user message
```

Therefore, when the model, tools, instructions, environment, and history have not been rewritten, the earlier request's `input` array should remain an exact prefix of the follow-up request. The backport adds this exact-prefix regression in `core/tests/suite/pending_input.rs`.

A prefix change is expected after compaction, truncation, rollback, or an intentional model-visible context update. Outside those cases, loss of the previous request input as a prefix is a bug rather than intended steer behavior.

Visible TUI history is a separate concern. This branch already avoids rendering an active-turn steer as committed history before core acknowledges it.

## Documents

- [`upstream_audit.md`](upstream_audit.md): end-to-end comparison and behavior classification.
- [`problem_statement.md`](problem_statement.md): addressed correctness gaps and required invariants.
- [`design_proposal.md`](design_proposal.md): branch-local design that preserves `/pause`, `/continue`, and the editable queue.
- [`implementation_plan.md`](implementation_plan.md): prioritized implementation and regression-test sequence.

## Backport disposition

|Priority|Backport|Disposition|
|---|---|---|
|P0|Exact no-preemption/request-prefix test|Done.|
|P0|Typed pending user input and task-finish commit lifecycle|Done. This closes a duplicate-resubmission race and preserves text elements.|
|P0|Turn-id-checked dedicated steer operation|Done without migrating the TUI to the upstream app-server architecture.|
|P0|Deferred queued slash/shell actions|Done while retaining queue ids, editing, ordering, and per-message model/reasoning overrides for plain queued input.|
|P1|Merge rejected steers into one next-turn submission|Done.|
|P1|Queue while a plan item is streaming or only user-shell commands are active|Done.|
|P1|Abort cleanup ordering and empty-active-turn preservation|Done.|
|P1|Queue auto-send suppression during lifecycle transitions|Done for the branch-local transition windows.|
|P2|Client-generated user-message ids for exact pending-steer correlation|Done for direct-core TUI steers, committed user-message events, and structured steer rejection recovery.|
|P2|Interrupt-and-immediately-resubmit pending steers|Optional adaptation for explicit interrupt only; do not apply to `/pause`.|
|Out of scope|Thread-switch input snapshots, mailbox wakeups, remote-image support, and wholesale app-server migration|Upstream features with broader architectural dependencies.|

## Branch behavior to preserve

- `/pause` keeps ordinary queued messages queued and restores uncommitted steers without creating a normal user turn.
- `/continue` resumes the checkpoint without adding a user message and keeps queue draining gated until the continuation starts or fails.
- The queue remains editable and reorderable, with per-message model and reasoning-effort overrides.
- The steer feature toggle continues to control whether Enter steers or queues; upstream has graduated steering to always-on behavior, but that is a product-design difference rather than a required correctness backport.

## Scope

The proposal covers:

- composer submission and deferred-command handling;
- pending, rejected, and ordinary queued inputs;
- TUI-to-core turn identity and error recovery;
- core pending-input ordering, commit, completion, pause, and abort paths;
- cache-relevant request construction tests.

It does not propose replacing this branch's direct core operation channel with the upstream app-server-first TUI.
