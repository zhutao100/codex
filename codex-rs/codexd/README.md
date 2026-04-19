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
2. Request a snapshot:
   - `{"id":1,"method":"codexd/snapshot","params":{}}`
3. Subscribe using the snapshot sequence:
   - `{"id":2,"method":"codexd/subscribe","params":{"afterSeq":<snapshot.seq>}}`

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
  - `[{ "threadId": "...", "turnId": "..." }]`

The `notification` in `runtimeNotification` is a generic hub notification forwarded from runtimes. `codexd` only interprets `turn/started` and `turn/completed` to maintain `activeTurns` in snapshots; all other notifications are forwarded as-is.

## Producer flow (runtime side)

Runtimes connect to the same socket and send:

- `codexd/runtime/register` (claim a `runtimeId` for that connection)
- `codexd/runtime/updateMetadata`
- `codexd/runtime/event` (forward a hub notification)
- `codexd/runtime/unregister` (optional; disconnect also unregisters claimed runtimes)

In Rust, prefer using `codex_codexd::producer::CodexdProducerClient` instead of writing to the socket directly.
