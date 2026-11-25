# codexd

`codexd` is a local daemon (Unix domain socket + JSON-lines protocol) that:

- accepts runtime updates (`codexd/runtime/*`)
- serves a consistent snapshot of current state (`codexd/snapshot`)
- broadcasts a sequenced event stream to subscribers (`codexd/event`)

It is used by tools like `CodexMenuBar` (`CodexMenuBar/README.md`) to render authoritative active turn state.

## Socket path

By default, `codexd` binds to:

- `$CODEX_HOME/runtime/codexd/codexd.sock` (if `CODEX_HOME` is set)
- otherwise, `~/.codex/runtime/codexd/codexd.sock`

You can override the socket path with `--socket-path`.

Note: Unix domain socket paths have a platform-specific length limit (macOS `SUN_LEN`). For tests, prefer short paths like `/tmp/codexd.sock`.

The socket is created with user-only permissions (`0700` on the parent directory and `0600` on the socket), so only the same OS user can connect by default.

## Running

Foreground:

```shell
codex app-server codexd run
```

From this repo:

```shell
cd codex-rs
cargo run -p codex-cli -- app-server codexd run
```

Override the socket path:

```shell
codex app-server codexd run --socket-path /tmp/codexd.sock
```

Launch agent management:

```shell
codex app-server codexd install-launch-agent
codex app-server codexd status
codex app-server codexd uninstall-launch-agent
```

## Protocol (JSON-lines)

Transport is an AF_UNIX stream socket. Each message is a single UTF-8 JSON object, delimited by `\n`.

- Requests: `{"id":1,"method":"...","params":{...}}`
  - `id` is optional for fire-and-forget methods.
- Responses: `{"id":1,"result":...}` or `{"id":1,"error":{"code":-32000,"message":"..."}}`
- Notifications: `{"method":"...","params":...}`

`codexd` does not require a `"jsonrpc"` version field.

## Consumer flow (snapshot + subscribe)

Typical client flow:

1. Connect to the socket.
2. Optionally request daemon protocol information:
   - `{"id":1,"method":"codexd/hello","params":{}}`
3. Request a snapshot:
   - `{"id":2,"method":"codexd/snapshot","params":{}}`
4. Subscribe using the snapshot sequence:
   - `{"id":3,"method":"codexd/subscribe","params":{"afterSeq":<snapshot.seq>}}`

`codexd/hello` returns:

- `protocolVersion`
- `capabilities` (`eventReplay`, `runtimeState`, `activeTurnContext`, `workActivity`)
- current `seq`

The `codexd/event` stream is sequenced:

- each emitted event increments a global `seq` counter
- notifications include `params.seq` so clients can resume from a known point

On subscribe, `codexd` replays a bounded in-memory buffer (currently the most recent 1024 events) with `seq > afterSeq` and then continues live delivery. The buffer is not persisted across restarts.

`afterSeq` must not be ahead of the current sequence or `codexd/subscribe` returns an error.

If a client disconnects, it should reconnect, re-fetch a snapshot, and resubscribe using the last seen `seq` (or the snapshot `seq`).

## `codexd/event` payloads

Each `codexd/event` notification has:

```json
{"method":"codexd/event","params":{"seq":123,"event":{...}}}
```

Event `type` values:

- `runtimeUpsert`
  - `{"type":"runtimeUpsert","runtime":{...}}`
- `runtimeRemoved`
  - `{"type":"runtimeRemoved","runtimeId":"rt-1"}`
- `runtimeNotification`
  - `{"type":"runtimeNotification","runtimeId":"rt-1","notification":{"method":"turn/started","params":{...}}}`

`runtimeUpsert.runtime` is a `RuntimeSnapshot`:

- `runtimeId` (string)
- `pid` (number | null)
- `sessionSource` (string | null)
- `cwd` (string | null)
- `displayName` (string | null)
- `activeTurns` (array)
  - `turnKey` (stable composite key, normally `<threadId>:<turnId>`)
  - `threadId`
  - `turnId`
  - optional summary fields: `status`, `startedAt`, `model`, `latestLabel`
  - optional active-context fields: `scope`, `taskKind`, `sessionSource`,
    `subAgentSource`, `parentThreadId`, `parentTurnId`, `modelProvider`,
    `thinkingLevel`, `cwd`, `approval`, `sandbox`, `modelContextWindow`,
    `contextRemainingPercent`, `tokenUsage`, `tokenUsageBaseline`, `threadName`

The `notification` in `runtimeNotification` is a generic hub notification forwarded from runtimes. `codexd` interprets `turn/started`, `turn/completed`, `turn/contextUpdated`, `turn/stateUpdated`, and `thread/tokenUsage/updated` to maintain `activeTurns` in snapshots; all notifications are still forwarded as-is.

### Work Activity

Daemon capability `workActivity` describes the wire format, not every producer. Check each runtime's `workActivitySupported`. For a capable producer, `workActivity` is a complete `{producerGeneration, revision, items}` object. `items` contains only scheduler-owned tasks and accepted queued review/continuation stages. An empty scheduler set also clears leftover projected turns; a delayed start, inactive UI or missing completion must not hold the system awake. Absent or malformed is not empty. Legacy producers provide `activeTurns` only. Always use turns (not work keys) for history.

Core guards span task finalization and automatic submission dispatch. A queued guard is reserved before the predecessor's terminal event and retained through acceptance of the next task, or dropped on rejection/cancellation. This covers delegate preparation without requiring a delegate turn ID. The guard reports work; it does not create a macOS assertion.

The TUI forwards live events at thread ingress, independent of selection; replaying a thread does not republish its history. App-server publishes daemon starts from core `TurnStarted`, not the public v2 `turn/start` acknowledgement: input submitted during an existing turn may be steered into it rather than starting a second task. Delegate starts use the child thread identity; matching context deactivation closes the child even if the parent consumes the child's terminal event and later completes under its own ID.

Active turns are keyed by `turnKey`. Producers should send `turn.key` or
`turnKey`; otherwise `codexd` computes `<threadId>:<turnId>`. Legacy
`turn/completed` notifications without a key fall back to this composite key
when `threadId` is present, then to a bare `turnId` search for older producers.
When a runtime notification changes the snapshot, `codexd` emits a
`runtimeUpsert` before the corresponding `runtimeNotification`.

## Producer flow (runtime side)

Runtimes connect to the same socket and send:

- `codexd/runtime/register` (claim a `runtimeId` for that connection)
- `codexd/runtime/updateMetadata`
- `codexd/runtime/updateState` (atomically claim the runtime and replace its metadata, active turns and optional work summary)
- `codexd/runtime/event` (forward a hub notification)
- `codexd/runtime/unregister` (optional; disconnect also unregisters claimed runtimes)

TUI and app-server use one `CodexdProducerClient` projection per producing runtime. `updateState` is its first publication on every socket; there is no preliminary empty registration. On ordinary updates the notification and corresponding work revision are applied together, with `runtimeUpsert` preceding forwarded detail. A newer connection supersedes older writes and cleanup for the same runtime ID.

Publish lifecycle notifications in order (`try_publish_hub_notification` on synchronous paths). The bounded outbox retains current state independently of pending detail. Overflow and reconnect discard superseded deltas in favor of a complete replacement; offline progress/history detail is not a durable replay service. Socket EOF triggers reconnect even when a running task produces no output.
