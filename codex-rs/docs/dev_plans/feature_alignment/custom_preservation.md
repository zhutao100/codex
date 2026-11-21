# Preserve customized behavior

Status: requirements for the [feature-alignment plan](README.md). These requirements describe behavior already implemented in this branch; the alignment work must preserve it. Follow the linked feature contracts for full semantics rather than introducing a second definition here.

## Keep the branch-specific controls

|Control|Preserve|Reference relationship|
|---|---|---|
|`auto_rename_thread`|Opt-in, core-owned naming after eligible completed regular turns; lightweight model/provider selection, fork exclusions, one attempt and session-index persistence.|Reference automatic titles have different TUI/app-server ownership, trigger and input; they are an analogy, not a replacement.|
|`auto_post_turn_completion_review`|Automatic review plus independently available manual completed-turn review, exact request/candidate snapshots, durable episode ownership and uncapped automatic fix rounds.|Ordinary code review and Guardian approval review do not implement this completion contract.|
|`recoverable_exec_output`|Bounded reader-observed bytes, committed-prefix receipts, source-session ownership, restart recovery and explicit missing/incomplete results.|Larger previews, new truncation flags or a different exec host do not reproduce retained evidence.|
|`exec_job_completion`|Registered ownership before launch, flushed call/result acknowledgment before waiting, terminal outcome before contextual delivery, explicit pause/continue behavior and one contextual admission.|A terminal session or a mailbox wakeup alone does not reproduce owned completion delivery.|

Keep their present defaults; none of these four controls is enabled by default. Registry cleanup must not drop their enum variants, schema exposure, configuration keys, tool wiring or tests. New architecture must adapt them at the new owner before replacing an old owner.

## Preserve the workflows outside the registry

|Workflow|Required invariant|Owning contract|
|---|---|---|
|Pause and continue|Continue resumes unfinished work without inventing a user message; stop waits for the required cancellation/finalization joins; old unmatched calls are repaired before replay.|[Pause and continue](../../features/pause-continue.md)|
|Steering and queue|Accepted, installed/acknowledged and model-consumed input remain distinct. A rejected or wrong-turn steer cannot mutate pending input.|[Steering and queue](../../features/steering-and-queue.md)|
|Exact work notes|Notes keep exact multipart bytes and contextual classification. The capture request can use normal tool schemas, but runtime execution is rejected. Notes are neither a new user request nor a completed answer.|[Session work notes](../../features/session-work-notes.md)|
|Completion review|Keep the exact consumed request, initial/latest candidate, request-only stable frame, advisory developer handoff, complete replacement answer and durable ownership across restart.|[Completion review](../../features/post_turn_completion_review/README.md)|
|Review budgets|Charge request-only context once and include unreported history tails. Integrate with a replacement usage ledger rather than retaining two independently advancing ledgers.|[Review accounting](../../features/post_turn_completion_review/budget_accounting.md)|
|Compaction|Preserve source/destination model distinctions, protected replacement, notes recovery, opaque compaction data and pending-input ordering.|[Remote compaction](../../features/remote_compaction_v2/README.md)|
|Transport and output replay|Retain full logical input for reconnect/fallback, opaque sampled-output ciphertext and tool/result order. Provider filtering affects outgoing projection, not durable stored history.|[WebSockets](../../features/responses-websocket.md) and [sampled-output replay](../../features/responses-output-replay.md)|
|Evidence and owned jobs|Do not advertise uncommitted bytes, expose another session's evidence to a fork, rerun a saved job, or let a stale completion wake an idle/superseded owner.|[Context harness](../../features/context-harness.md)|
|Restricted explorers|Fresh context, bounded acquisition, inherited model/provider and explicit dispatch/process restrictions remain consistent; generic multi-agent defaults must not broaden the role.|[Context harness](../../features/context-harness.md)|
|Delegate status and `codexd`|Review remains active work under its own identity after the main answer; duplicate/out-of-order delegate completion cannot finish the parent.|[Delegate runtime status](../../features/delegate-runtime-status.md)|
|Model and local skill customizations|Keep model overlays, fallback metadata, provider identity, lightweight naming model selection and intentional skill-root isolation when ownership moves.|[Model overlay](../../features/model-overlay.md), [model catalog](../../features/model-catalog.md) and [local plugins](../../features/local_plugins.md)|

The reference `app-server-daemon` is not a replacement for the custom `codexd` status-hub protocol. Storage or event-model migrations must enumerate that consumer explicitly.

## Focused improvement: manual rename wins over automatic naming

Current [`prepare_auto_rename`](../../../core/src/session/mod.rs) checks the saved title before generation starts. The async task later calls unconditional `set_thread_name` in [`tasks/mod.rs`](../../../core/src/tasks/mod.rs). A manual rename during that interval can be overwritten according to the inspected control flow. This is a source-identified race candidate, not a reproduced runtime failure.

The reference [`AutomaticThreadName::save`](https://github.com/zhutao100/codex/blob/33411a01d7dae5fe89728ecfa2ab8b1c09002329/codex-rs/tui/src/app_server_session.rs#L446) checks the expected saved name and cancellation, and its title workflow cancels generation on manual rename. Adapt that precedence at the current shared core persistence owner. Serialize the conditional automatic update with manual renames; a read immediately before an unconditional write leaves a race. Keep the current completed-turn trigger, opt-in, provider selection and core ownership.

Acceptance: block the generated result, save a manual name, then release generation; the manual name remains both in memory and after reopening the session. Also cover manual rename immediately before publication, cancellation, a forked session and the existing single-attempt rule. Preserve the existing lightweight-model naming fixtures.

## Backports that remain owned here

`remote_compaction_v2` was explicitly backported by [8d49c5e1eb29](https://github.com/zhutao100/codex/commit/8d49c5e1eb2921691fe207bca88fefdc9951bdc1); retirement of the reference flag does not remove streaming compaction. Its routing cleanup belongs in [cleanup](cleanup.md#compaction-and-transport-selection).

`output_token_replay` was introduced upstream by [5ef96ab2785f](https://github.com/zhutao100/codex/commit/5ef96ab2785f3ecddf279639eff7b1b42c906fa4) and explicitly backported by [b8b9e39e9453](https://github.com/zhutao100/codex/commit/b8b9e39e945326380c552a4550f5e6752bd07ad5). Its matching metadata is not permission to replace the custom request projection, review context, history normalization or transport reset logic. Compare supported request shapes and recovery scenarios in [stable alignment](stable_alignment.md#shared-metadata-with-different-implementations).

Use the [validation map](validation.md#preservation-checks) to select existing fixtures whenever a shared owner changes. A plan cannot ensure absence of regression before implementation; it can identify the contracts and evidence that must hold at each change boundary.
