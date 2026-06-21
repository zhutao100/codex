# Upstream Audit and Classification

## 1. Comparison basis

The audit compares corresponding live-turn, history, compaction, rollout, and replay paths in this branch and the upstream branch. The upstream branch has undergone broader protocol and architecture work, so a textual diff is not a safe backport strategy. The useful unit of comparison is behavioral invariant.

## 2. Summary matrix

|Behavior|This branch|Upstream branch|Classification|Backport decision|
|---|---|---|---|---|
|Record real user item before committing previous settings|Present|Present|Already fixed/aligned|No action.|
|Keep previous settings when compaction clears reference context|Present live and replay|Present in replay|Backported replay fix|Done.|
|Invalid-image scan stops at a real user boundary|Present|Present|Already fixed/aligned|No action.|
|Fresh explicit input precedes queued steering|Present|Present|Aligned behavior|No action.|
|Prompt-copy call/output normalization and modality stripping|Present|Present, expanded for new item types|Aligned core behavior|No action.|
|Exact replacement history for new compactions|Present|Present|Aligned behavior|No action.|
|Bare `TurnContext` does not become previous settings|Present in replay|Present|Backported semantics without lifecycle subsystem|Done.|
|Rollback counts metadata and user turns coherently|Present through branch-local checkpoints|Present through turn segments|Backported invariant|Done.|
|Legacy compaction avoids current-context historical injection|Present|Present|Backported fix|Done.|
|Persisted `TurnStarted`/`TurnComplete` lifecycle boundaries|Filtered out|Persisted|New upstream infrastructure|Defer.|
|Reverse segmented replay and early stop at replacement base|Absent|Present|New upstream architecture|Defer; emulate only required semantics.|
|Compaction compatibility hash (`comp_hash`)|Absent|Present|New feature/correctness guard|Conditional.|
|Context-window downshift under same model|Model-change path only|Expanded|New feature|Conditional/defer.|
|History version and compaction windows|Absent|Present|New infrastructure|Defer.|
|Body-after-prefix compaction budgeting|Absent|Present|New optimization|Defer.|
|Realtime, inter-agent, plugin, hook, and expanded response-item replay|Branch-specific older model|Present|Upstream feature growth|Do not import for this patch.|
|`/pause`, `/continue`, work-note carryover, `GhostSnapshot`|Present|Not equivalent|Deliberate branch divergence|Preserve and add compatibility tests.|

## 3. Confirmed upstream fixes worth backporting

### 3.1 Replay segments associate metadata with real user work

The upstream `core/src/session/rollout_reconstruction.rs` scans newest-to-oldest and groups records into `ActiveReplaySegment`s bounded by persisted turn lifecycle events. A segment can contain `TurnContext`, compaction metadata, abort state, and response items, but it counts against rollback only when there is evidence of a real instruction/user turn.

This fixes the key category error in this branch: `TurnContext` records are metadata candidates, not user-turn counters.

A direct cherry-pick is inappropriate because upstream replay relies on:

- persisted `TurnStarted` and `TurnComplete` events;
- turn IDs carried by lifecycle and `TurnContext` records;
- reference states distinguishing never-set, explicitly-cleared, and latest;
- history windows and replacement bases;
- newer response and inter-agent variants.

The selected backport reproduces the invariant using existing forward records and real user `ResponseItem` boundaries.

### 3.2 Rollback skips non-user tasks

Upstream tests cover completed turns, incomplete turns, and standalone non-user tasks. Rollback decrements its pending count only for segments that contain real user work. This prevents compaction or other task metadata from consuming a rollback slot.

This branch should obtain the same result with one metadata checkpoint per real user boundary after the current replay epoch.

### 3.3 Bare context does not hydrate resume metadata

Upstream replay can attach a `TurnContext` to a segment while still refusing to use it as `previous_turn_settings` unless the segment is a surviving user turn. This prevents an early-cancelled or task-only context record from becoming the apparent latest model.

The minimal equivalent is a `pending_context` candidate committed only when replay observes `is_user_turn_boundary`.

### 3.4 Legacy compaction is deterministic with respect to resume-time context

For a legacy `CompactedItem` without `replacement_history`, upstream rebuilds compacted history with `Vec::new()` as the initial context. It then clears the reconstructed reference baseline so canonical context is appended at the current end on the next turn.

This avoids inserting current cwd, policy, instructions, or model context at an old historical position. The change is local and should be backported.

### 3.5 Previous settings and reference baseline are independent

Upstream replay separately derives:

- previous settings from the newest surviving user turn;
- reference context from the newest surviving user baseline or an explicit compaction clear.

This matches the live semantics already present in this branch. The selected replay design restores that separation.

## 4. New upstream features not required by the confirmed fix

### Persisted lifecycle events and reverse/lazy replay

This is the upstream foundation for precise turn segmentation and efficient resume from the newest replacement checkpoint. It is valuable long term but not minimal. Porting it safely would require auditing every branch-local task that emits or suppresses lifecycle events, especially `/pause`, `/continue`, compaction, post-turn review, delegates, and undo.

### `comp_hash`

Upstream model metadata includes a compaction-compatibility hash. `PreviousTurnSettings` and `TurnContextItem` carry it, and the turn path can compact when the model slug is unchanged but the compaction contract changed. This is useful only if the model catalog used by this branch supplies a stable hash.

### Context-window downshift handling

Upstream previous-model compaction also considers a smaller context window even when the model slug remains unchanged. This branch's current helper is primarily model-change-oriented. The feature is separable but should be considered together with `comp_hash` and model metadata provenance.

### History versions and windows

Upstream tracks history rewrites and compaction windows, including IDs/numbers used by newer storage and replay paths. They support observability, lazy history, and newer compaction semantics rather than the immediate replay correctness fix.

### Body-after-prefix budget

Upstream can reason about tokens after a reusable prefix when deciding compaction. Porting it would touch token accounting, model metadata, request construction, and tests. It is an optimization, not a prerequisite.

### Expanded protocol and tools

Upstream history handles newer response variants, inter-agent communication, image generation, tool search, encrypted outputs, realtime state, plugins, hooks, and related telemetry. These are feature additions, not defects in this branch's existing item universe.

## 5. Deliberate branch divergence to preserve

|Branch-local behavior|Backport constraint|
|---|---|
|`/pause` and `/continue` without a synthetic user item|Replay changes must continue to infer and hydrate `PendingContinuation` without importing upstream lifecycle assumptions.|
|Auto-compact session work notes|Work-note messages remain contextual/non-boundary items and survive compaction as currently designed.|
|`GhostSnapshot` and `/undo`|Snapshots remain durable raw items, omitted from model prompts, and preserved across compaction.|
|Post-turn completion/review flows|Non-user task metadata must not commit previous-turn settings; synthetic review output can carry current metadata in rollback checkpoints.|
|Existing rollout schema|Old rollout files must remain readable; no mandatory migration.|

## 6. Bugs already fixed in this branch

The earlier version of this plan incorrectly treated the following as outstanding. Source inspection and tests show they are already fixed:

1. **Previous settings committed too early:** false. `record_context_updates_and_set_reference_context_item` does not set them; `record_user_prompt_and_emit_turn_item` commits after recording the user response item.
2. **Compaction clears live previous settings:** false. `replace_compacted_history` changes history/reference state but leaves `previous_turn_settings` intact.
3. **Invalid-image recovery stops at any user-role message:** false. `replace_last_turn_images` already uses `is_user_turn_boundary`.

No backport patch should disturb these behaviors.

## 7. Confirmed defects fixed by this backport

- independent `context_stack` and history rollback counters;
- bare `TurnContext` hydration;
- loss of previous settings after a compaction with no adjacent post-compaction context;
- post-compaction context incorrectly becoming previous settings without a user boundary;
- resume-time initial context injected at a legacy compaction point;
- pending continuation model selected from the reference baseline instead of committed previous settings;
- standalone compaction summary eligible for false unfinished-turn inference.

## 8. Upstream regressions and audit risks

No upstream-only regression was confirmed strongly enough to propose a corrective backport.

One upstream-only case should receive an explicit test before its replay design is adopted wholesale: multiple non-contextual user/steering messages can occur within one persisted lifecycle segment, while forward history rollback counts individual instruction boundaries. The audited reverse replay currently records only a boolean `counts_as_user_turn` per segment. Whether this is a defect depends on the intended rollback unit, so it is an upstream review item rather than a confirmed bug in this plan.

Shared limitations, not upstream regressions:

- pre-turn compaction does not fully account for incoming context/user items before deciding whether compaction is sufficient;
- local compaction's “preserve cache” comment is inaccurate when it removes oldest input;
- legacy compaction necessarily produces a temporary non-canonical prompt shape until current context is appended again.

## 9. Chore changes not needed here

Do not pull these into the minimal patch merely to resemble upstream:

- rollout policy crate relocation;
- shared truncation crate/module moves;
- response enum exhaustiveness changes for variants absent here;
- telemetry, analytics, plugin, and hook plumbing;
- window/realtime/inter-agent fields in `TurnContextItem`;
- widespread visibility/module-layout refactors;
- upstream test-support reorganizations.

They add merge risk without improving the selected invariants.
