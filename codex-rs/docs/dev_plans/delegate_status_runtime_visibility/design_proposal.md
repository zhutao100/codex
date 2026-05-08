# Design Proposal

## Status

Proposed.

## Summary

Add an explicit active runtime context layer between delegate sessions and status consumers. The parent TUI should keep the parent session as the primary conversation, but it should temporarily set the active status subject to the delegate while the delegate is running. `codexd` should receive the same delegate-scoped context so subscribers can represent the active delegate turn without guessing from parent-session state.

The recommended design is to add delegate-scoped protocol events or an equivalent internal event envelope that carries a compact `RuntimeContextSnapshot`. This snapshot should be independent from `Config` and safe to render in status surfaces.

## Non-goals

- Do not forward raw delegate `SessionConfigured` into the parent TUI as a normal session configuration event.
- Do not mutate the parent `ChatWidget.config` to temporarily look like the delegate config.
- Do not persist transient delegate status cards into the parent rollout unless a product decision explicitly wants that.
- Do not expose provider secrets or raw provider configuration through `codexd`.
- Do not change the post-turn review result schema `PostTurnCompletionReviewOutputEvent { evaluation, fix_actions_advised }`.

## Recommended proposal: active runtime context snapshots

Introduce a protocol-safe status snapshot used by the TUI and `codexd`:

- `scope_id`: stable id for this active runtime scope, for example `delegate:<parent_session_id>:<delegate_session_id>`.
- `scope`: `primary` or `delegate`.
- `task_kind`: `review`, `post_turn_completion_review`, or another task kind.
- `session_source`: the existing session source, including sub-agent source when applicable.
- `session_id`: the active session/thread id.
- `parent_session_id`: set for delegate scopes.
- `parent_turn_id`: set when the delegate is reviewing or continuing a specific parent turn.
- `rollout_path`: optional delegate rollout path when useful for debugging.
- `thread_name`: optional active thread name.
- `cwd`: effective working directory.
- `model`: effective model slug or display name chosen for the active scope.
- `model_provider_id`: effective provider id chosen for the active scope.
- `model_provider_display_name`: optional display label; do not include secrets.
- `approval_policy`: effective approval policy.
- `sandbox_policy`: effective sandbox policy summary or serializable policy.
- `reasoning_effort`: optional effective reasoning effort.
- `service_tier`: optional effective service tier.
- `model_context_window`: optional effective context window after model overlay and config overrides.
- `agents_summary`: optional already-renderable instruction-source summary for the active scope.
- `token_info`: optional active-scope token usage.

The exact Rust placement can be `protocol/src/protocol.rs` if app-server and downstream consumers need it, or an internal TUI/core type if the first implementation is TUI-only. Because `codexd` also needs the data, prefer protocol placement with optional fields and backwards-compatible serialization.

## New event flow

Add a lifecycle around delegate event forwarding:

1. Before the delegate's first content/progress event is forwarded, emit `RuntimeContextActivated` or `DelegateSessionStarted` to the parent event consumer.
2. When delegate `TokenCount` arrives, emit `RuntimeContextUpdated { scope_id, token_info }` instead of dropping the event or overwriting parent token state.
3. When delegate `ThreadNameUpdated` arrives, update the delegate context instead of dropping the event.
4. When delegate `TurnComplete`, `TurnAborted`, `TurnPaused`, or shutdown is observed, emit `RuntimeContextDeactivated { scope_id }` after any final status update.

`codex_delegate.rs::forward_events(...)` should still route approval requests to the parent session and should still suppress raw legacy deltas that would duplicate richer content events.

## TUI behavior

Add a status-subject abstraction to `ChatWidget`:

- Primary subject: current behavior, backed by parent `Config`, parent `SessionConfigured`, parent token state, and parent thread id.
- Delegate subject: active `RuntimeContextSnapshot`, active delegate token state, and active delegate thread id.

`ChatWidget::active_status_subject()` should return the top active delegate subject when a delegate is running, otherwise the primary subject.

Update these surfaces to use the active subject:

- `/status`: render from `StatusContextSnapshot` instead of directly from `Config`.
- Bottom status indicator: set active model and reasoning from the active subject, not from `current_model()` when the turn is a delegate.
- Status line model items: use active subject model.
- Status line context items: use active subject token info and context window.
- Session/status items: show delegate session id and parent linkage when the active subject is a delegate.

The parent session header should remain parent-oriented unless the product explicitly wants a small nested badge. Replacing the primary header model with the delegate model would imply a session switch and should be avoided.

## Status card refactor

Change `status/card.rs` so `new_status_output(...)` accepts a renderable status snapshot rather than a full `Config`. A compact shape is enough:

- Directory.
- Model.
- Model provider.
- Approval.
- Sandbox.
- Agents summary.
- Account display.
- Thread name.
- Session id.
- Parent/fork/delegate linkage.
- Collaboration mode or task kind.
- Token usage and context window.
- Rate limits when applicable.

For the primary subject, build this snapshot from the existing parent `Config` and `ChatWidget` state. For a delegate subject, build it from `RuntimeContextSnapshot`. This keeps the renderer pure and avoids temporarily mutating global config.

## `codexd` behavior

`codexd` should represent delegate turns as active turns with scope metadata. It should not require consumers to infer delegation from thread ids or event ordering.

Recommended active-turn fields:

- `turnKey`: stable composite key, preferably explicit; otherwise `threadId:turnId`.
- `threadId`.
- `turnId`.
- `scope`: `primary` or `delegate`.
- `taskKind`.
- `sessionSource`.
- `parentThreadId`.
- `parentTurnId`.
- `status`.
- `startedAt`.
- `model`.
- `modelProvider`.
- `thinkingLevel`.
- `cwd`.
- `approval`.
- `sandbox`.
- `modelContextWindow`.
- `contextRemainingPercent` or raw token usage.
- `latestLabel`.

All new fields can be optional for compatibility. The daemon should key active turns by `turnKey`, not by bare `turnId`.

## Alternatives considered

|Option|What changes|Pros|Cons|Recommendation|
|---|---|---|---|---|
|Forward raw delegate `SessionConfigured`|Stop filtering delegate `SessionConfigured` and let the parent TUI receive it|Smallest code change|Breaks primary session identity, header, rollout metadata, and history semantics|Reject|
|Enrich `TurnStartedEvent` only|Add model/provider/context fields to `TurnStartedEvent` and use them in TUI/codexd|Can fix bottom model display quickly|Does not solve `/status` sandbox, agents, session, parent linkage, token updates, or nested lifecycle|Useful as a short hotfix only|
|Active runtime context snapshots|Add explicit delegate-scoped status context lifecycle|Preserves parent session, fixes `/status`, bottom line, status line, and `codexd` consistently|Requires protocol/UI/codexd changes|Recommended|
|Register each delegate as a separate `codexd` runtime|Give each delegate its own runtime id|Clear separation for downstream apps|Still leaves TUI status unsolved; can overstate process/runtime boundaries; harder to correlate parent and delegate|Consider later if multiple concurrent delegates become common|

## Migration and compatibility

- Keep existing `SessionConfiguredEvent` semantics for primary sessions.
- Add optional fields or new event variants rather than changing required payloads.
- Keep old `codexd` `activeTurns` fields valid; consumers that ignore new fields should continue to work.
- Bump `codexd` protocol version or advertise a capability such as `activeTurnContext` when the optional fields are populated.
- Do not require model overlay changes. The delegate context should simply reflect the already-resolved model metadata.
