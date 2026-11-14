# Core submissions and events

Read when changing a core client, task lifecycle or event mapping. This is the internal queue protocol used by `CodexThread`; the [MCP server](codex_mcp_interface.md) and [app-server](../app-server/README.md) adapt it to separate public interfaces.

## Vocabulary and ownership

|Term|Meaning|
|---|---|
|Thread/session|Conversation state, configuration, history and runtime services owned by one core instance|
|Submission|A client operation with an ID, sent to the submission queue|
|Task|An executing workflow, such as regular work, review or compaction|
|User turn|Work rooted in accepted user input; it may include many inference/tool cycles|
|Model response|One Responses API inference stream; completion does not by itself finish a user turn|
|Event|Core output correlated with a submission; multiple events can have the same ID|

Initialization uses `Codex::spawn` and emits `SessionConfigured`. There is no `Op::ConfigureSession`. Configuration updates use `OverrideTurnContext` or the context supplied with `UserTurn`; a context-only override does not itself enqueue user input. See [session construction](../core/src/session/mod.rs) and [submission dispatch](../core/src/session/handlers.rs).

Regular work is serialized within a session; child/delegate threads have separate sessions. Pending user input can be accepted while work is running. Do not infer task replacement or consumption from the mere arrival of an operation.

## Operations and lifecycle

|Operation|Contract|
|---|---|
|`UserTurn`|Supplies user items and turn context, including model, cwd and execution policies|
|`UserInput`|Legacy input using persistent session defaults|
|`SteerInput`|Adds input to an expected active turn without changing its settings; see [steering and queue](features/steering-and-queue.md)|
|`OverrideTurnContext`|Updates persistent defaults without creating a user message|
|`Pause` / `Continue`|Stops and continues eligible work without a synthetic user turn; see [pause/continue](features/pause-continue.md)|
|`Interrupt`|Cancels current work and emits `TurnAborted`|
|`ExecApproval` / `PatchApproval` / `UserInputAnswer`|Resolves a pending interaction; waiting for an answer is not successful turn completion|
|`ReviewCompletedTurn`|Reserves an eligible completed answer for [completion review](features/post_turn_completion_review/README.md)|

`Submission` contains `id` and `op`; `Event` contains `id` and `msg`. Follow [protocol types](../protocol/src/protocol.rs) for field spelling, optionality, variants and correlation. Keep adapters forward-compatible with the variants they can receive instead of maintaining a second handwritten schema.

- `TurnStarted` begins the visible lifecycle. Assistant/tool deltas and completed items can arrive before the task ends.
- `TurnComplete` carries `last_agent_message`; it has no response-ID bookmark. Its v1 serialized tag is `task_complete`, with `turn_complete` accepted on input; `TurnStarted` likewise writes `task_started` and accepts `turn_started`.
- `TurnPaused` and `TurnAborted` are distinct terminal outcomes. An `Error` is an operation error; use lifecycle events and error context rather than treating every error as successful or final completion.
- `TokenCount` and runtime-context events update presentation/accounting; they do not establish which user amendments were consumed or which answer is reviewable.

## Model context and persistence

[User input types](../protocol/src/user_input.rs) include text, images, explicit skills and mentions. Preserve structured content through client adapters. Prompt construction and transport are separate from this submission/event API.

The local rollout records selected events, response items and host metadata under an explicit [persistence policy](../core/src/rollout/policy.rs). The UI event stream is not a replay log: pause/continue UI events are not persisted, and host recovery checkpoints are not model instructions. Use [history replay](features/history-replay.md) for replacement-history, rollback and resume rules.

`previous_response_id` belongs to [WebSocket transport](features/responses-websocket.md). Resume/fork uses local thread history and checkpoints, not a response ID returned by `TurnComplete`. Preserve that separation when adding adapters or changing cancellation.
