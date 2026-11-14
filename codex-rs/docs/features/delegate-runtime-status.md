# Delegate runtime status

Use this when changing review status rendering, delegate event forwarding, or `codexd` active-turn consumers. Inline delegates can use a different model, provider, policy and context window from their parent. Runtime snapshots describe that active work without replacing the parent conversation's configuration.

## Lifecycle contract

`RuntimeContextSnapshot` carries scope identity, task kind, session/parent linkage, effective CWD, model/provider id, policies, effort, service tier, context window, instruction-source summary and optional token usage. `SubAgentSource::Review` remains the low-level source classification; `post_turn_completion_review` is a task kind.

|Delegate event|Parent-visible behavior|
|---|---|
|`SessionConfigured`|Update the runtime snapshot; do not forward it as the parent's session configuration.|
|First status/content event|Activate the runtime context before forwarding content that depends on it.|
|`TokenCount`, `ThreadNameUpdated`, `TurnStarted`|Update delegate-scoped usage/name/window metadata. Preserve the parent's token state.|
|`TurnComplete`, `TurnAborted`, `TurnPaused`|Deactivate the active context before forwarding the normal terminal event.|
|Consumer cancellation or disconnect|Shut down and drain the owned delegate; do not leave a blocked forwarding task holding it alive.|

The TUI stores one active runtime snapshot. `/status`, the running model/effort label, status-line session/model fields and context indicators use it until a matching scope deactivates; then they use the parent state again. This is not a general nested-context stack. Raw delegate status events must not overwrite parent identity, rollout metadata or configuration.

Runtime-context events are transient and excluded from rollout persistence. Review results and durable task ownership use their existing records; a status snapshot is not recovery provenance. The status renderer still receives the shared account/rate-limit display context, so the runtime snapshot is not an independent provider billing record.

## `codexd` projection

The TUI's `MenuBarBridge` publishes the snapshots into the existing process-level runtime. The daemon advertises `activeTurnContext` and retains optional fields so older clients can continue reading the base active-turn shape.

|Notification|State change|
|---|---|
|`turn/started`|Insert by `turn.key`, otherwise `threadId:turn.id`; carry scope, task, parent linkage and effective runtime fields.|
|`turn/contextUpdated`|Update the matching active turn. The daemon also accepts `turn/stateUpdated`.|
|`thread/tokenUsage/updated`|Update thread usage and the addressed active turn; usage retains `total`, `last` and `modelContextWindow`.|
|`turn/completed`|Remove the addressed turn; explicit stale keys must not delete a different thread's reused turn id.|

Context activation may precede a normal turn-start event, so the bridge can establish the delegate turn first and reuse it when later content arrives. Deactivation completes that delegate's turns; a duplicate completion must not remove the parent's active turn. Preserve per-thread prior token usage as the baseline for turn usage displays.

Use composite keys in new producers. Legacy bare-turn-id fallback remains only for notifications without an explicit key. Snapshot-changing notifications also publish updated runtime snapshots, so consumers do not need to replay every raw event to observe current state. Expose provider ids and instruction-source summaries, not credentials, raw provider configuration or prompt bodies.

## Source and verification

- [Protocol types](../../protocol/src/protocol.rs), [delegate forwarding](../../core/src/codex_delegate.rs) and [rollout event policy](../../core/src/rollout/policy.rs).
- [TUI runtime lifecycle](../../tui/src/chatwidget/session_flow.rs), [status rendering](../../tui/src/status/card.rs) and [MenuBarBridge](../../tui/src/menubar_bridge.rs).
- [Daemon projection](../../codexd/src/daemon.rs) and [consumer protocol](../../codexd/README.md).
- Existing tests cover forwarding order, blocked-send cancellation, TUI restoration after deactivation, duplicate turn ids, pre-start activation, token updates, stale completion keys and snapshot broadcasts. Preserve model/provider/policy identity assertions as well as text snapshots when changing this flow.
