# Turn History Reference Context Backport

This plan proposes a focused backport from the upstream project into this project's current branch.

The goal is not to port the upstream session/task refactor wholesale. The useful remaining work is a smaller correctness and performance set around turn-context persistence, rollout replay, model-downshift compaction, and pending-input ordering.

## Documents

- `problem_statement.md` - current-branch inspection, remaining gaps, and backport decisions.
- `design_proposal.md` - proposed target behavior and patch shape.
- `implementation_plan.md` - phased work plan, tests, and verification commands.

## Executive Summary

Current branch inspection shows several previously identified upstream fixes have already landed:

|Area|Current branch status|Decision|
|---|---|---|
|Token accounting after last model item|Implemented in `core/src/context_manager/history.rs` via item-level estimates and post-model tail accounting.|No new backport work.|
|Image stripping for text-only models|Implemented in `core/src/context_manager/normalize.rs` and prompt-history preparation.|No new backport work.|
|Base64 image payload discounting|Implemented with fixed resized-image byte estimates in `core/src/context_manager/history.rs`.|Keep; dynamic original-detail image estimates are lower priority because this branch has no `ImageDetail` protocol field.|
|Rollback trimming contextual setup before the rolled-back turn|Partially implemented in `ContextManager::drop_last_n_user_turns` / `trim_pre_turn_context_updates`.|Keep, but extend with persisted baseline replay.|
|Compaction replacement history|Implemented for local and remote compaction through `CompactedItem.replacement_history`.|Keep; add reference-context semantics around replacement history.|
|Compaction failure propagation|Implemented by returning errors from auto-compaction paths and stopping the turn on failure.|No new backport work.|
|WebSocket reset after history rewrite|Implemented with `ModelClientSession::reset_websocket_session()` and compact-result reset plumbing.|No new backport work.|
|Tool-search normalization|Not applicable to this branch's current protocol variants.|Defer.|
|Inter-agent assistant turn boundaries|Upstream-specific multi-agent protocol behavior; current branch has different delegate semantics.|Defer unless this branch starts persisting assistant-role inter-agent instructions.|
|Persisted turn-context baseline and replay reconstruction|Missing as a coherent runtime invariant.|Backport.|
|Model-downshift pre-sampling compaction|Missing.|Backport after baseline work.|
|Pending-input lifecycle ordering|Partially missing; this branch drains pending input before the first sampling request.|Backport the narrow ordering fix.|

## Recommended Scope

Backport these pieces as one small feature series:

1. Persist one `TurnContextItem` per real user turn and keep it as the durable context-diff baseline.
2. Reconstruct history plus baseline metadata from rollout, including `ThreadRolledBack` and `CompactedItem.replacement_history`.
3. Use the previous turn's model settings to compact with the previous, larger-context model before sampling with a smaller current model.
4. Match upstream pending-input ordering so a fresh explicit user prompt samples before queued pending input.
5. Classify preserved work-notes messages as contextual session state, not user turn boundaries, so rollback and user-message compaction do not treat them as real user turns.
