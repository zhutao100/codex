# Menu Bar Status Hub Prerequisites

## Goal

Evolve `codexd` from an active-turn event fanout daemon into a status hub that can support a richer macOS menu bar status center without making the app connect directly to every Codex runtime.

## Current `codexd` Contract

- Local AF_UNIX JSON-lines daemon.
- Consumer methods:
  - `codexd/snapshot`;
  - `codexd/subscribe`.
- Runtime producer methods:
  - `codexd/runtime/register`;
  - `codexd/runtime/updateMetadata`;
  - `codexd/runtime/event`;
  - `codexd/runtime/unregister`.
- Event stream:
  - `codexd/event` with monotonic `seq`;
  - bounded in-memory replay of the latest 1024 events;
  - payloads: `runtimeUpsert`, `runtimeRemoved`, `runtimeNotification`.
- Runtime state:
  - `runtimeId`, `pid`, `sessionSource`, `cwd`, `displayName`;
  - active turns with `threadId` and `turnId`.
- Daemon-maintained state:
  - active turns are derived only from forwarded `turn/started` and `turn/completed`;
  - all other hub notifications are opaque.
- Current publisher behavior:
  - `app-server` publishes selected `ServerNotification` variants;
  - TUI publishes synthetic lifecycle, progress, token usage, and error notifications;
  - producers reconnect and register metadata, but do not publish a complete current-state snapshot on reconnect.

## Why Expansion Is Worthwhile

CodexMenuBar can improve the current popover with existing data, but these richer surfaces need daemon support:

- correct state after app reconnect, daemon restart, or missed events;
- recent completed-turn history after app restart;
- transcript, diff, command-output, and reasoning detail panes;
- stable runtime capabilities and protocol version checks;
- consumer-to-runtime controls such as interrupt, pause, continue, approvals, and thread reads;
- diagnostics for stale runtimes, producer queue loss, and replay gaps.

## Non-Goals

- Do not turn `codexd` into a full app-server replacement.
- Do not persist raw transcript content in the daemon by default.
- Do not add app-server v1 API surface.
- Do not make cross-user socket access easier.

## Protocol Additions

### 1. Capability And Health Discovery

Add a small discovery method:

```json
{"id":1,"method":"codexd/hello","params":{}}
```

Response shape:

```json
{
  "protocolVersion": 1,
  "capabilities": [
    "eventReplay",
    "runtimeState",
    "runtimeRequestRouting"
  ],
  "seq": 42
}
```

Use camelCase wire fields and additive capabilities. Older consumers can continue using `codexd/snapshot` and `codexd/subscribe`.

### 2. Refreshable Runtime State

Extend runtime registration or add a follow-up method so producers can publish current state after reconnect:

```json
{
  "method": "codexd/runtime/updateState",
  "params": {
    "runtimeId": "pid:123",
    "activeTurns": [
      {
        "threadId": "thr_1",
        "turnId": "turn_1",
        "status": "inProgress",
        "startedAt": 1760000000,
        "model": "gpt-5-codex",
        "latestLabel": "Running tests"
      }
    ]
  }
}
```

Keep the first patch summary-oriented:

- runtime metadata;
- active turns;
- current plan summary;
- latest progress label/category;
- token usage;
- latest error;
- command/file counts and compact summaries.

Do not include full command output, full diffs, or raw transcript text in this method.

### 3. Broaden Notification Relay

Publish more app-server v2 notifications to `codexd` when they are useful for status UI:

- `thread/name/updated`;
- `turn/diff/updated`;
- `item/agentMessage/delta`;
- `item/plan/delta`;
- `item/reasoning/summaryTextDelta`;
- `item/reasoning/summaryPartAdded`;
- `item/reasoning/textDelta`;
- `item/commandExecution/outputDelta`;
- `item/fileChange/outputDelta`;
- `item/mcpToolCall/progress`;
- `thread/compacted`;
- `account/updated`;
- `account/login/completed`;
- `configWarning`.

Gate high-volume deltas behind producer/runtime capabilities or a daemon setting if needed.

### 4. Consumer Read Methods

Add bounded read methods for status UIs:

```json
{"id":2,"method":"codexd/runtime/read","params":{"runtimeId":"pid:123"}}
{"id":3,"method":"codexd/turn/read","params":{"runtimeId":"pid:123","turnId":"turn_1"}}
```

Suggested response policy:

- return compact summaries by default;
- include optional detail sections behind explicit params;
- never require the menu bar app to reconstruct all details from raw event replay.

### 5. Runtime Request Routing

Add a bidirectional request path only after runtime capabilities exist.

Consumer-facing examples:

```json
{"id":4,"method":"codexd/turn/interrupt","params":{"runtimeId":"pid:123","turnId":"turn_1"}}
{"id":5,"method":"codexd/turn/pause","params":{"runtimeId":"pid:123","turnId":"turn_1"}}
{"id":6,"method":"codexd/turn/continue","params":{"runtimeId":"pid:123","turnId":"turn_1"}}
```

Daemon-to-producer routing can use internal request notifications:

```json
{
  "method": "codexd/runtime/request",
  "params": {
    "requestId": "req-1",
    "method": "turn/interrupt",
    "params": { "turnId": "turn_1" }
  }
}
```

Producer-to-daemon response:

```json
{
  "method": "codexd/runtime/response",
  "params": {
    "requestId": "req-1",
    "result": {}
  }
}
```

Route only to a runtime that explicitly registered the capability for the target method.

## State Model

Keep `codexd` state bounded and summary-first:

- runtime map keyed by `runtimeId`;
- active turn map keyed by `(runtimeId, turnId)`;
- compact recent completed turn ring per runtime;
- latest account/rate-limit summary;
- event replay ring with gap detection metadata.

Recommended state timestamps:

- integer Unix seconds;
- fields named `createdAt`, `updatedAt`, `startedAt`, `completedAt`, `lastEventAt`.

## Implementation Order

1. Add `codexd/hello` and protocol tests.
2. Add `runtime/updateState` with summary-only active turns.
3. Teach app-server and TUI producers to resend current state after producer reconnect.
4. Broaden app-server `should_publish_to_codexd` coverage for non-control notifications.
5. Add bounded daemon state summaries and `codexd/runtime/read`.
6. Add `codexd/turn/read` for compact turn details.
7. Add request routing for interrupt only.
8. Add pause/continue routing after core/app-server expose those operations.
9. Evaluate optional persisted replay after state summaries prove insufficient.

## Implemented Slice

- `codexd/hello` reports protocol version, capabilities, and current sequence.
- `codexd/runtime/updateState` lets producers replace the daemon's active-turn summary for a runtime.
- App-server notification relay now includes additional status UI events such as diffs, deltas, account updates, compaction, and config warnings.

## Validation

- `cargo test -p codex-codexd`.
- `cargo test -p codex-app-server-protocol` when app-server notification shapes change.
- `cargo test -p codex-app-server` when publish coverage or request routing changes.
- `cargo test -p codex-tui` when the TUI bridge changes.
- CodexMenuBar e2e smoke with a real daemon:
  - `./scripts/e2e_codexd.sh`;
  - reconnect while an active turn is running;
  - daemon restart followed by producer state refresh;
  - high-volume output delta stream with bounded memory.

## Backward Compatibility

- Keep existing `codexd/snapshot`, `codexd/subscribe`, and `codexd/event` shapes valid.
- Add methods and fields rather than changing required fields.
- Make consumers branch on `codexd/hello` capabilities before using new methods.
- If replay gaps are detected, tell consumers to fetch `codexd/snapshot` or `codexd/runtime/read`.
