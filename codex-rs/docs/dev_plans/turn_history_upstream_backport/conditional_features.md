# Conditional and Deferred Upstream Features

## 1. Compaction compatibility hash

### Value

The upstream branch can compact when the model's compaction compatibility changes even if the model slug is unchanged or the context-window downshift predicate does not fire.

The rule is deliberately conservative:

```text
compact only when previous.comp_hash and current.comp_hash are both present and differ
```

A missing hash does not imply incompatibility.

### Minimal dependency scope

Port only these pieces:

1. Add `comp_hash: Option<String>` with a serde default to `ModelInfo` in `protocol/src/openai_models.rs`.
2. Thread it through model metadata decoding and branch-local model overlays:
   - `core/src/models_manager/manager.rs`
   - `core/src/models_manager/model_info.rs`
   - `core/src/models_manager/overlay.rs`
   - affected constructors/tests.
3. Add the optional field to `TurnContextItem` and `PreviousTurnSettings` with backward-compatible defaults.
4. Populate it in `TurnContext::to_turn_context_item`.
5. Restore it through the new committed replay checkpoints.
6. Add `comp_hash_changed` to `maybe_run_previous_model_inline_compact` before the existing context-window downshift test.
7. Compact with the previous model context when the hash differs.

### Gate

Do not land the trigger merely with an always-`None` field. First verify that the model metadata source used by this branch provides stable values. Otherwise the added schema and plumbing have no behavior and create maintenance cost.

## 2. Body-after-prefix compaction budget

### Upstream behavior

`AutoCompactTokenLimitScope::BodyAfterPrefix` tracks an estimated prefill at the start of a compaction window. Auto-compaction can then budget growth after that prefix while still enforcing the model's absolute context window.

This is relevant to prefix-cache economics: a stable large prefix can remain cached while the active body is allowed a separate growth budget.

### Minimal prerequisites are still broad

A correct port requires all of the following as one coherent unit:

- configuration enum and parsing;
- session state for compaction-window prefill;
- history helper for tokens after the last model-generated item/body boundary;
- absolute full-context-window enforcement;
- pre-turn and post-sampling token-status calculations;
- compaction-window reset/advance semantics;
- resume reconstruction of window number/ID or an explicitly simpler branch-local substitute;
- tests for total-scope and body-scope behavior across compaction, resume, and model switch.

Porting only the enum or only the token subtraction would risk exceeding the real context window. Defer this feature from the minimal correctness series.

## 3. Persisted lifecycle IDs and reverse replay

### Upstream value

Turn IDs on lifecycle/context records let upstream reverse replay:

- associate incomplete, completed, and aborted records;
- skip exactly N newest real user-turn segments;
- select the newest surviving replacement checkpoint;
- stop reading older rollout data once required metadata is known;
- support future lazy rollout loading.

### Why it is not the minimal dependency

This branch currently does not persist `TurnStarted` or `TurnComplete`, has no turn ID on `TurnContextItem`, and has custom continuation semantics. A complete port would touch:

- protocol event schemas;
- rollout persistence policy;
- every turn/task producer;
- abort and completion emission;
- old-rollout compatibility;
- pause/continue cleanup;
- reconstruction and rollback tests;
- clients that deserialize lifecycle events.

The branch-local epoch/checkpoint design fixes the known correctness bugs with existing records. Lifecycle IDs remain a reasonable future migration if exact rollback through compacted historical turns or lazy replay becomes a requirement.

## 4. Auto-compaction window IDs

Window numbers/IDs help upstream coordinate body-after-prefix accounting, explicit new-window requests, hooks, and reconstruction. Without those consumers, adding IDs alone has no value. Defer with the body-after-prefix feature.

## 5. History versioning

`history_version` is useful when another component holds a cursor into history and must detect replacement, rollback, or image rewrite. This branch's prompt construction clones current raw history each time and does not need a version token for the proposed fix.

Port it only alongside a concrete cursor/guardian consumer.

## 6. Image-detail token estimation

Upstream decodes dimensions and distinguishes richer image-detail modes. This branch already prevents base64 transport bytes from dominating estimates by substituting a fixed resized-image cost.

A separate accuracy improvement could port dimension-aware estimates, but it is independent of turn/history correctness and may require protocol fields absent here.

## 7. Upstream protocol and ecosystem changes

Do not pull these as prerequisites for the mandatory backport:

- typed inter-agent communication;
- tool-search and image-generation response variants;
- realtime state;
- hook/plugin session-start lifecycle;
- remote compaction v2;
- shared output-truncation crate migration;
- item-ID assignment;
- prompt guardian/history cursors.

Each should be evaluated against a branch-local feature requirement rather than inherited through textual cherry-picking.
