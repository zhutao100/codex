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

Capable full state replacements require an explicit `activeTurns` array: omission is rejected without replacing the current runtime. New producers also send `workActivity.runningTurnKeys`, the complete set of canonical `threadId:turnId` identities owned by executing tasks. The producer and daemon intersect visible turns with this set on updates and reconnect; they never synthesize visible turns from work. A queued successor can prevent sleep without keeping its predecessor visible. Earlier producers without the field retain whole-runtime idle pruning.

Daemon capability `workActivity` describes the wire format, not every producer. Check each runtime's `workActivitySupported`. For a capable producer, `workActivity` is a complete `{producerGeneration, revision, items}` object. `items` contains only scheduler-owned tasks and accepted queued review/continuation stages. An empty scheduler set also clears leftover projected turns; a delayed start, inactive UI or missing completion must not hold the system awake. Absent or malformed is not empty. Legacy producers provide `activeTurns` only. Always use turns (not work keys) for history.

Core guards span task finalization and automatic submission dispatch. A queued guard is reserved before the predecessor's terminal event and retained through acceptance of the next task, or dropped on rejection/cancellation. This covers delegate preparation without requiring a delegate turn ID. The guard reports work; it does not create a macOS assertion.

The TUI forwards live events at thread ingress, independent of selection; replaying a thread does not republish its history. App-server publishes daemon starts from core `TurnStarted`, not the public v2 `turn/start` acknowledgement: input submitted during an existing turn may be steered into it rather than starting a second task. Delegate starts use the child thread identity; matching context deactivation closes the child even if the parent consumes the child's terminal event and later completes under its own ID.

An explicit `turn.key` or `turnKey` is authoritative. Without a key, a completion or context update naming a thread resolves within that thread only. Only a genuinely unthreaded legacy event may use a bare `turnId`, and only when exactly one active turn matches. A late completion for child `A:0` cannot remove `B:0`, including from a producer's reconnect snapshot.
When a runtime notification changes the snapshot, `codexd` emits a
`runtimeUpsert` before the corresponding `runtimeNotification`.

## Producer flow (runtime side)

Runtimes connect to the same socket and send:

- `codexd/runtime/register` (claim a `runtimeId` for that connection)
- `codexd/runtime/updateMetadata`
- `codexd/runtime/updateState` (atomically claim the runtime and replace its metadata, active turns and optional work summary)
- `codexd/runtime/event` (forward a hub notification)
- `codexd/runtime/unregister` (optional; disconnect also unregisters claimed runtimes)

For notification-only producers, a rejected state or event causes a reverse `codexd/runtime/requestState` notification on that same socket: `{runtimeId, producerGeneration, minimumRevision}`. The daemon retains the last usable projection, even if an invalid live event claims idle, and records identifiable revisions before the first accepted state. It coalesces outstanding requests. The producer drains queued detail, reads current scheduler ownership and sends a complete `updateState` at or above the observed revision without waiting for more activity or reconnecting. Stale frames cannot satisfy the request. Older producers that ignore this method cannot guarantee quiet repair.

Do not forward `turn/started` if the scheduler projection excludes that task, including when a sibling is still working. Valid terminal and item details still reach history; during repair they do not establish new power state.

Runtime, generation, thread and turn IDs must be nonempty and unpadded. A malformed generation cannot establish a revision floor; a negotiated full state with an unidentified visible turn cannot be cached. A repair request for an unidentified producer uses a null generation and minimum revision zero; a valid generation can establish authority on the same socket. Work revisions must fit a signed 64-bit integer and work IDs must be nonempty.

An older revision cannot change current work or live turns. Its explicitly keyed item or progress detail may still describe an existing archive: `runtimeArchiveDetail` carries only that notification and runtime ID. Consumers apply it to a matching completed run, never to create or modify live work. Old consumers ignore this additional event type.

TUI and app-server use one `CodexdProducerClient` projection per producing runtime. `updateState` is its first publication on every socket; there is no preliminary empty registration. On ordinary updates the notification and corresponding work revision are applied together, with `runtimeUpsert` preceding forwarded detail. A newer connection supersedes older writes and cleanup for the same runtime ID.

Publish lifecycle notifications in order (`try_publish_hub_notification` on synchronous paths). The bounded outbox retains current state independently of pending detail. Overflow and reconnect discard superseded deltas in favor of a complete replacement; offline progress/history detail is not a durable replay service. Socket EOF triggers reconnect even when a running task produces no output.
