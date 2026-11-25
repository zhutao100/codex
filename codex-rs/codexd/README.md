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
- `instanceId` (unique to this daemon lifetime; older daemons omit it)
- `capabilities` (`eventReplay`, `runtimeState`, `activeTurnContext`, `workActivity`)
- current `seq`

The `codexd/event` stream is sequenced:

- each emitted event increments a global `seq` counter
- notifications include `params.seq` so clients can resume from a known point

On subscribe, `codexd` replays a bounded in-memory buffer (currently the most recent 1024 events) with `seq > afterSeq` and then continues live delivery. The buffer is not persisted across restarts.

`afterSeq` must not be ahead of the current sequence or `codexd/subscribe` returns an error.

On reconnect, request a fresh snapshot for current work. Only resume history from the last contiguous `seq` if `instanceId` matches. Merge replay through the snapshot sequence before reconciling turns. A different or absent instance ID or a gap beyond the replay buffer cannot establish the outcome of a missing turn.

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
- `runtimeArchiveDetail`
  - `{"type":"runtimeArchiveDetail","runtimeId":"rt-1","notification":{"method":"turn/completed","params":{"turnKey":"review:0","turn":{"id":"0","status":"failed"}}}}`
  - Archive-only item/progress detail, or a keyed terminal from producer repair. If the turn is still attached during an overlap, its terminal follows the projection that ends its ownership. This is not a runtime registration or a live start.

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

`codexd` interprets `turn/started`, `turn/completed`, `turn/contextUpdated`, `turn/stateUpdated`, and `thread/tokenUsage/updated` to maintain `activeTurns`. A corresponding `runtimeUpsert` precedes any forwarded `runtimeNotification`. Forwarding is conditional: starts excluded by scheduler ownership and starts during repair are suppressed. A stale ownership revision cannot reintroduce live work; explicitly keyed item/progress detail can still target an existing archive.

### Work Activity

Capable full state replacements require an explicit `activeTurns` array: omission is rejected without replacing the current runtime. New producers also send `workActivity.runningTurnKeys`, the complete set of canonical `threadId:turnId` identities owned by executing tasks. The producer and daemon intersect visible turns with this set on updates and reconnect; they never synthesize visible turns from work. A queued successor can prevent sleep without keeping its predecessor visible. Earlier producers without the field retain whole-runtime idle pruning.

Daemon capability `workActivity` describes the wire format, not every producer. Check each runtime's `workActivitySupported`. For a capable producer, `workActivity` is a complete `{producerGeneration, revision, items}` object. `items` contains only scheduler-owned tasks and accepted queued review/continuation stages. An empty scheduler set also clears leftover projected turns; a delayed start, inactive UI or missing completion must not hold the system awake. Absent or malformed is not empty. Legacy producers provide `activeTurns` only. Always use turns (not work keys) for history.

Core guards span task finalization and automatic submission dispatch. A queued guard is reserved before the predecessor's terminal event and retained through acceptance of the next task, or dropped on rejection/cancellation. This covers delegate preparation without requiring a delegate turn ID. The guard reports work; it does not create a macOS assertion.

The TUI forwards live events at thread ingress, independent of selection; replaying a thread does not republish its history. App-server publishes daemon starts from core `TurnStarted`, not the public v2 `turn/start` acknowledgement: input submitted during an existing turn may be steered into it rather than starting a second task. Delegate starts use the child thread identity; matching context deactivation closes the child even if the parent consumes the child's terminal event and later completes under its own ID.

Starts and completions read the same identity: nested `turn.key`, then top-level `turnKey`. An explicit key need not equal `threadId:turnId`. A completion or context update without an explicit key resolves within its named thread; a bare short ID can complete only a uniquely matching attached turn. Resolve it **before** removing that turn. The producer's journal and forwarded completion use the resolved `turnKey` and known `threadId`. A bare duplicate with no attached match, or an ambiguous short ID, supplies no archive outcome. A legacy terminal that conflicts with versioned history still publishes the updated visible-turn projection, never a downgrade to the archive.
When a runtime notification changes the snapshot, `codexd` emits a
`runtimeUpsert` before the corresponding `runtimeNotification`.

Each keyed terminal outcome published by the producer carries a new revision, including a correction after the visible turn is gone. The ownership set can stay empty across these revisions: `completed` then `failed` updates the same archive without creating work.

## Producer flow (runtime side)

Runtimes connect to the same socket and send:

- `codexd/runtime/register` (claim a `runtimeId` for that connection)
- `codexd/runtime/updateMetadata`
- `codexd/runtime/updateState` (atomically claim the runtime and replace its metadata, active turns and optional work summary)
- `codexd/runtime/event` (forward a hub notification)
- `codexd/runtime/unregister` (optional; disconnect also unregisters claimed runtimes)

For notification-only producers, a rejected state or event causes a reverse `codexd/runtime/requestState` notification on that same socket: `{runtimeId, producerGeneration, minimumRevision}`. The daemon keeps accepted turn details and records identifiable revisions before the first accepted state. While a replacement is outstanding, snapshots and upserts for a known runtime advertise `workActivitySupported: true` but omit `workActivity`. The first rejection publishes this transition; it is neither confirmed work nor idle. The producer drains queued detail, reads current scheduler ownership and sends a complete `updateState` at or above the observed revision without waiting for more activity or reconnecting. Stale frames cannot satisfy the request. Older producers that ignore this method cannot guarantee quiet repair.

Do not forward `turn/started` if the scheduler projection excludes that task, including when a sibling is still working. Valid terminal and item details still reach history; during repair they do not establish new power state.

A first state declaring `workActivitySupported: true` without `workActivity` already requires negotiated repair, even with no generation and a zero floor. For a challenger the obligation belongs to its socket and runtime ID, not the incumbent. Its subsequent legacy-shaped state cannot displace a healthy legacy owner; closing the challenger cannot revoke the incumbent. If the incumbent leaves first, the challenger still owes coherent negotiated state. An outstanding negotiated repair for a runtime already claimed also survives the owner's EOF.

Two genuinely legacy sockets have no `producerGeneration` to distinguish their histories. If the old claim has unfinished turns, the daemon sends `runtime/historyLost` before the new claim's `runtimeUpsert`. This is not a removal or terminal outcome; the successor's complete projection decides ownership.

Runtime, generation, thread and turn IDs must be nonempty and unpadded. A malformed generation cannot establish a revision floor; a negotiated full state with an unidentified visible turn cannot be cached. A repair request for an unidentified producer uses a null generation and minimum revision zero; a valid generation can establish authority on the same socket. Work revisions must fit a signed 64-bit integer and work IDs must be nonempty.

An older revision cannot change current work or live turns. While the runtime is published, `runtimeArchiveDetail` can still carry explicitly keyed item or progress detail for an existing completed run. A detached terminal may instead complete missing or detached history, never a still-attached active turn. Consumers that do not recognize an archive-only method should ignore it, not interpret it as a live notification.

TUI and app-server start with `updateState`. A producer may also `register` first: a genuine legacy stream can then report visible turns, but registration does not establish a negotiated work revision. A stale first full state or a first versioned delta requests a coherent full state. After accepted idle 10, `register` followed by busy 9 requests minimum 10; headerless starts cannot bypass that request. On ordinary updates, `runtimeUpsert` precedes notification detail.

Socket accept order is not publication order. A socket that has not yet claimed the runtime can take over with a coherent full state at or above its generation's admission floor, even if a later-connected peer published metadata first. A displaced claimant cannot publish or unregister again on that claim; its EOF does not remove the new owner. Receiving `requestState` does not transfer a provisional claim. An invalid first state keeps the runtime unpublished and the original socket can correct it without reconnecting. Legacy-to-legacy handoff requires a full list of visible turns, possibly empty. Keyed repair detail waits if the turn is still attached, even when its key differs from `threadId:turnId`.

Publish lifecycle notifications in order (`try_publish_hub_notification` on synchronous paths). When overflow or reconnect discards buffered detail, the producer sends a full replacement with a `historyRecovery` epoch and up to 50 distinct keyed terminal outcomes. The first accepted state with a newer epoch sequences `runtime/historyLost`, the ownership projection, then retained outcomes (attached terminals wait for release). Consumers detach unfinished history on the loss marker; a missing terminal is not proof of success. The daemon remembers the highest applied epoch per runtime and producer generation, including across EOF, unregister and other generations. Repeating an old epoch at a new work revision refreshes only the projection; it does not re-stamp or replay old outcomes. This ordering is not durable across daemon restart. Socket EOF triggers reconnect even for quiet work.

Owner EOF and explicit unregister revoke ownership immediately, not revision order. For each runtime ID, retain the maximum identifiable revision of **each** producer generation until daemon restart. This applies to challengers before their first accepted state: malformed H:30 followed by coherent H:29 does not displace healthy G. Corrected H:30 can take over, but does not erase G:10 or admit G:9 from another socket. The directed request uses the challenger's generation and minimum when its header identifies them. A healthy incumbent may still report its own final idle or EOF while a competing socket waits for a higher revision of the same generation. A genuine legacy full state does not reset these floors or bypass a pending negotiated repair.

Order keyed terminal outcomes at publication, whether delivered as ordinary `runtimeNotification`, recovered history, or `runtimeArchiveDetail`. A normal terminal without its own work header uses the current authoritative work revision when available. Remember the last 50 published outcome stamps per runtime across full states, unregister, EOF and repair. Thus ordinary `failed@13` cannot become `completed@12` on reconnect; `interrupted@15` can update it. A legacy completion for a reused key still removes its attached visible turn and publishes that projection, but cannot overwrite the earlier versioned outcome. During repair an absent turn's outcome can be sequenced immediately, while an attached turn's outcome waits for the projection or removal that relinquishes it. Neither path can restore work. The RPC reply precedes a directed `requestState`.

Draining the producer queue is not a successful socket write. The producer keeps up to 50 distinct, keyed terminal outcomes not yet written, including frames in the writer's hands. A complete event or recovery-frame write retires only the outcomes it covered. A partial write, an unsent tail, or a second failed replacement therefore recovers current work and outstanding outcomes without replaying the successfully written prefix. This is not an acknowledgment of consumption by the daemon.

To verify a real producer overflow with a consumer, build `cargo-local build -p codex-codexd --example observe_producer_overflow` and pass that example executable to the menu bar VM verifier's `--overflow-producer`.
