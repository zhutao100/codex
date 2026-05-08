# `codexd` Protocol Notes for Delegate Visibility

## Status

Proposed.

## Target Base

This proposal targets this project's customized `v0.98` branch shape.

## Current shape

`codexd` currently treats each producer process as one runtime. The TUI producer registers a runtime id like `pid:<pid>` with session source `cli`, cwd, and display name. The daemon stores active turns inside that runtime.

The current active turn snapshot contains only:

- `threadId`.
- `turnId`.
- optional `status`.
- optional `startedAt`.
- optional `model`.
- optional `latestLabel`.

The daemon updates active turns only from forwarded generic notifications:

- `turn/started`: insert active turn.
- `turn/completed`: remove active turn.

All other notifications are forwarded to subscribers but are not folded into the daemon's snapshot state.

## Current failure mode for delegates

The TUI `MenuBarBridge` infers `turn/started` notifications from the TUI event stream. Its `current_model` and `current_model_provider` are updated only from visible `SessionConfigured` events. Because delegate `SessionConfigured` is filtered by `codex_delegate.rs`, delegate turns inherit stale parent model/provider metadata.

The bridge also has a bare-turn-id uniqueness guard: if a turn id is already associated with one thread, a second turn with the same id and a different thread is ignored. This is unsafe for delegate sessions because parent and delegate sessions can each generate low integer turn ids independently.

The daemon has the same keying problem because `RuntimeState.active_turns` is keyed by bare `turnId`.

## Upstream app-server alignment

The upstream project has no `codexd/` module, but app-server v2 provides the closest public protocol vocabulary. `codexd` should follow those shapes for overlapping concepts so downstream applications can bridge both APIs with minimal translation:

| Concept | Upstream app-server v2 shape | `codexd` recommendation |
| --- | --- | --- |
| Thread/session lifecycle | `thread/started`, `thread/status/changed`, `thread/name/updated` | Use the same names when emitting thread-level lifecycle from `codexd`, or expose a direct mapping in `codexd/README.md`. |
| Turn lifecycle | `turn/started`, `turn/completed`, each scoped by `thread_id` | Preserve these names and require `threadId` in producer events whenever available. |
| Token/context-window usage | `thread/tokenUsage/updated` with `total`, `last`, `modelContextWindow` | Prefer this event for token usage and context window changes. Do not bury token usage only in a generic context update. |
| Source classification | `SessionSource::SubAgent(CoreSubAgentSource)` | Add optional `sessionSource` and `subAgentSource` fields rather than only `taskKind`. |
| External identity | `thread_id` plus `turn_id` | Use `turnKey = "${threadId}:${turnId}"` as the daemon key and keep bare `turnId` only as a legacy display field. |

A `turn/contextUpdated` or `runtime/contextUpdated` notification is still useful for this project's additional runtime fields: model display name, provider id, approval policy, sandbox policy, instruction summary, parent thread id, parent turn id, and task kind. It should complement, not replace, app-server-compatible token and lifecycle events.

## Protocol extension

Keep the generic notification stream, but define a richer active-turn contract for `turn/started` and optional update notifications.

`turn/started.params.turn` should allow these optional fields:

- `key`: stable active turn key. If omitted, consumers should use `threadId + ":" + turn.id`.
- `status`.
- `scope`: `primary` or `delegate`.
- `taskKind`: for example `user`, `review`, or `post_turn_completion_review`.
- `sessionSource`: for example `cli` or `sub_agent:review`.
- `parentThreadId`.
- `parentTurnId`.
- `model`.
- `modelProvider`.
- `thinkingLevel`.
- `cwd`.
- `approval`.
- `sandbox`.
- `modelContextWindow`.
- `latestLabel`.

Add a generic update notification for fields that are not known at turn start or change during the turn:

- Method: `turn/contextUpdated` or `turn/stateUpdated`.
- Required params: `threadId`, `turnId`, and preferably `turnKey`.
- Optional fields: `modelContextWindow`, `tokenUsage`, `contextRemainingPercent`, `latestLabel`, `threadName`, and status fields.

Keep `thread/tokenUsage/updated` for backwards compatibility, but also fold token usage into the active turn when `threadId` and `turnId` resolve to a known active turn.

## Daemon state changes

Change daemon active-turn storage from bare `turnId` to explicit `turnKey`:

- Parse `turn.key` when present.
- Otherwise compute `turnKey = threadId + ":" + turn.id`.
- Insert, update, and remove by `turnKey`.
- When handling legacy `turn/completed` without a key, compute the same composite key from `threadId` and `turn.id` when both are present.
- If a legacy completion lacks `threadId`, fall back to the previous bare-turn-id search only as a compatibility path.

After interpreting `turn/started`, `turn/completed`, or `turn/stateUpdated`, broadcast a `runtimeUpsert` or another snapshot-changing event in addition to the generic `runtimeNotification`. Otherwise snapshot clients that rely on state updates rather than raw event replay can stay stale until the next explicit state update.

## TUI producer bridge changes

Change `MenuBarBridge` to track active turns by composite key:

- Replace `turn_key_by_turn_id: HashMap<String,String>` with a lookup that can store multiple thread ids per turn id, or remove it and use `threadId:turnId` everywhere.
- `ensure_turn_started(thread_id, turn_id)` should never suppress a second turn solely because another thread already has the same turn id.
- `complete_turn` should prefer a composite key. If the completion event lacks a thread id, complete the most recent matching key as a fallback.
- When an active delegate context is available, include delegate context fields in the produced `turn/started` notification.
- When a delegate token/context update is available, publish a context update notification with the delegate `turnKey`.

## Producer source of truth

There are two possible producer sources:

1. TUI bridge source: the bridge consumes parent-visible events and publishes `codexd` notifications. This is the current shape and is sufficient if delegate runtime context events are forwarded to the TUI before delegate content events.
2. Core/app-server source: every `Codex` session publishes lifecycle and context directly to `codexd`. This is more authoritative and helps non-TUI app-server sessions, but it is a larger architectural shift.

Recommended first step: keep the TUI bridge producer, but feed it explicit active runtime context events and emit app-server-compatible names for overlapping lifecycle/token events. Then consider moving the same context event generation lower into core/app-server so all frontends share one producer path.

## Compatibility

- Keep protocol version 1 clients functional by preserving `threadId`, `turnId`, `status`, `model`, and `latestLabel`.
- Advertise an additional capability such as `activeTurnContext` when `scope`, `taskKind`, parent linkage, and context-window fields are populated.
- Treat unknown fields as ignored by older consumers.
- Do not include raw provider config, API keys, base URLs with embedded credentials, or full prompt/instruction bodies in `codexd` snapshots.
