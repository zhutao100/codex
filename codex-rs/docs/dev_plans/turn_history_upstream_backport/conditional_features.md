# Conditional and Deferred Upstream Features

## 1. Decision rule

The mandatory replay fix should not absorb upstream infrastructure merely because it appears nearby in the current upstream tree. A follow-up feature is eligible only when:

1. it solves a demonstrated problem in this branch;
2. its data source is available and stable here;
3. its minimal dependency set can be isolated;
4. it preserves branch-local pause/continue, work-note, and undo behavior;
5. it has focused tests independent of unrelated upstream protocol expansion.

## 2. Conditional: compaction compatibility hash

### Upstream behavior

The upstream branch carries `comp_hash: Option<String>` through:

- `ModelInfo` in `protocol/src/openai_models.rs`;
- `TurnContextItem` in `protocol/src/protocol.rs`;
- `TurnContext::to_turn_context_item` in `core/src/session/turn_context.rs`;
- `PreviousTurnSettings` in `core/src/session/mod.rs`;
- rollout reconstruction;
- the previous-model compaction decision in `core/src/session/turn.rs`.

Compaction is triggered for a hash change only when both previous and current hashes are present and differ. Missing metadata does not force compaction.

### Problem solved

A model slug can remain constant while its compaction prompt/contract changes. The hash lets the agent rebase history before continuing under an incompatible compaction regime.

### Minimal prerequisite scope

If the model metadata source used by this branch supplies a stable hash, port only:

1. `ModelInfo.comp_hash` with serde default.
2. `ModelInfoPatch` and `ModelInfoPatchToml` support in `core/src/models_manager/overlay.rs` when local overlays must set it.
3. `TurnContextItem.comp_hash` as an optional backward-compatible field.
4. `PreviousTurnSettings.comp_hash`.
5. propagation in `TurnContext::to_turn_context_item` and replay checkpoints.
6. a helper equivalent to:

   ```rust
   fn comp_hash_changed(previous: Option<&str>, current: Option<&str>) -> bool {
       matches!((previous, current), (Some(previous), Some(current)) if previous != current)
   }
   ```

7. focused live and resume tests.

Do not port history windows, realtime flags, or upstream replay segments as prerequisites.

### Gate

Do not land this feature when every model resolves to `None`. Optional fields alone create maintenance cost without behavior.

### Required tests

- same slug, hashes A/B: compaction runs;
- same slug, same hash: compaction does not run;
- either hash missing: compaction does not run;
- resume restores previous hash from a committed user checkpoint;
- bare `TurnContext` hash does not become previous settings;
- old rollout without the field remains readable.

## 3. Conditional: context-window downshift without model-slug change

### Upstream behavior

The upstream previous-model compaction path also reacts when the effective context window becomes smaller, even if the slug is unchanged.

### Minimal scope

This can be added after replay metadata is correct by extending `PreviousTurnSettings` with the minimum previous window/budget evidence required by the decision. Do not infer it from the current model catalog during replay; persist the historical value if correctness depends on it.

### Gate

Add only if this branch can switch model metadata/window configuration under a stable slug in production. Otherwise the existing model-change path is sufficient.

### Tests

- same slug, smaller current window, oversized history: compact before sampling;
- same slug, equal/larger window: no new compaction;
- resume preserves historical window evidence;
- missing historical window uses conservative existing behavior.

## 4. Deferred: persisted lifecycle IDs and reverse replay

### Value

Upstream lifecycle persistence enables:

- precise grouping of non-user tasks and user turns;
- newest-to-oldest rollback selection;
- early stopping at the newest surviving replacement base;
- future lazy rollout loading.

### Why it is not a minimal prerequisite

This branch filters `TurnStarted`, `TurnComplete`, `TurnPaused`, and `TurnContinued`. Adding them changes durable behavior and requires task-by-task semantics. A coherent port must cover:

- IDs on `TurnStarted`, `TurnComplete`, `TurnAborted`, and `TurnContext`;
- persistence policy;
- normal turns with queued steering;
- standalone and inline compaction;
- `/pause` and `/continue` boundaries;
- post-turn review/delegate tasks;
- old rollouts with absent IDs;
- fork/truncation utilities that count user turns;
- migration and duplicate-event behavior.

### Prerequisite test before adoption

Define the rollback unit for multiple real user/steering messages inside one lifecycle. Upstream's audited reverse metadata path counts a segment once, while forward history rollback counts instruction boundaries. Resolve and test that semantic before copying the architecture.

## 5. Deferred: history version

Upstream increments a history version on rewrites. This can invalidate derived caches or detect mutation across asynchronous work.

This branch currently clears WebSocket continuation explicitly around compaction and constructs prompt history under session state access. No confirmed stale-derived-history bug requires the version counter. Port only alongside a consumer that needs it.

Minimal future scope:

- counter in `ContextManager`;
- increment on replace, rollback, invalid-image mutation, and other rewrites;
- no increment for pure prompt projection;
- tests for every mutation path;
- explicit consumer semantics.

## 6. Deferred: body-after-prefix token budgeting

### Value

Upstream can compact based on the model-visible body after a reusable prefix rather than only total history size. This may reduce unnecessary compaction for large stable prefixes.

### Dependency surface

- token accounting split by prefix/body;
- model metadata/feature signaling;
- prompt construction agreement about the prefix boundary;
- compaction decisions and tests across model switches;
- interaction with settings updates, images, and request properties.

This is an optimization and should not be coupled to replay correctness.

## 7. Deferred: compaction windows and window IDs

Upstream persists window number/ID metadata and advances it after compaction. The feature supports request headers, observability, replay, and newer storage behavior.

A minimal port is not isolated because it touches:

- `CompactedItem` schema;
- session state;
- client metadata headers;
- resume/fork semantics;
- local and remote compaction;
- tests and telemetry.

Do not add placeholder fields without consumers.

## 8. Deferred: remote compaction v2 and expanded compaction phases

The upstream branch has newer remote compaction behavior, reasons/phases, and model capabilities. Port only from a separate product requirement. The replay fix already treats `replacement_history` as an opaque exact base and therefore remains compatible with future replacement producers.

## 9. Deferred: expanded response items and inter-agent communication

Upstream history and replay support item variants absent from this branch, including newer tool/search/image/inter-agent flows. Mechanical enum backports require protocol, API, event mapping, persistence, token estimation, normalization, and UI changes.

Do not add unreachable variants to satisfy upstream match arms. When a feature is intentionally ported, audit it end-to-end:

```text
API decode -> ResponseItem -> live history -> rollout -> replay -> prompt projection -> token estimate -> UI/event mapping
```

## 10. Not needed: upstream module and crate chores

The following are implementation-location changes, not behavioral prerequisites:

- rollout policy moved into a separate crate/module;
- truncation helpers moved/shared;
- test modules split or renamed;
- visibility and import cleanup;
- telemetry field additions unrelated to selected decisions.

Keep this branch's layout unless a selected feature requires a move.

## 11. Recommended sequencing

1. Land the mandatory replay/legacy/continuation patch.
2. Observe whether model metadata supplies a meaningful `comp_hash`.
3. Port `comp_hash` as a focused optional-field series if the gate is met.
4. Evaluate same-slug context-window downshift independently.
5. Consider lifecycle/reverse replay only as a dedicated architecture project with branch-local task semantics specified first.

This ordering avoids turning a narrow correctness fix into an upstream synchronization effort.
