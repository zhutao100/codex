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
- `capabilities` (`eventReplay`, `runtimeState`, `activeTurnContext`)
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
    `contextRemainingPercent`, `tokenUsage`, `threadName`

The `notification` in `runtimeNotification` is a generic hub notification forwarded from runtimes. `codexd` interprets `turn/started`, `turn/completed`, `turn/contextUpdated`, `turn/stateUpdated`, and `thread/tokenUsage/updated` to maintain `activeTurns` in snapshots; all notifications are still forwarded as-is.

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
- `codexd/runtime/updateState` (replace the runtime's active-turn summary after reconnect)
- `codexd/runtime/event` (forward a hub notification)
- `codexd/runtime/unregister` (optional; disconnect also unregisters claimed runtimes)

In Rust, prefer using `codex_codexd::producer::CodexdProducerClient` instead of writing to the socket directly.
Publish lifecycle notifications in observed order for a runtime. In synchronous
UI event paths, enqueue notifications with `try_publish_hub_notification`
rather than spawning one task per notification; if `turn/completed` reaches
`codexd` before its matching `turn/started`, active-turn state can remain stale.
