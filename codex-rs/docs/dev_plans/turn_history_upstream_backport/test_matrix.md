# Test Matrix

## 1. Test conventions

Every replay case should assert the complete `ReconstructedRollout` state where practical:

```text
history
reference_context_item
previous_turn_settings
pending_continuation
```

Use distinct model names and context values so an accidental selection is visible. Use real versus contextual user-message constructors explicitly.

## 2. Core metadata replay

|ID|Rollout shape|Expected result|
|---|---|---|
|R1|`TurnContext(A), user, assistant`|Reference A; previous model A; no continuation.|
|R2|`TurnContext(A)`|No reference; no previous; no continuation.|
|R3|context-update response items, `TurnContext(A)`, EOF|Update items remain according to current history rules; metadata remains uncommitted.|
|R4|`TurnContext(A), contextual user work-note`|No committed metadata; work note is not a boundary.|
|R5|`TurnContext(A), user1, assistant, user2 steering, assistant`|Both real boundaries are represented for branch-local rollback; metadata remains A.|
|R6|`TurnContext(A), user` with user event absent|Metadata commits from the response item.|
|R7|`TurnContext(A), UserMessage event` without user response item|No metadata commit. Event alone is not history evidence.|
|R8|multiple pending contexts before one real user|Newest pending context commits; earlier context records do not create checkpoints.|

## 3. Rollback

|ID|Rollout shape|Expected result|
|---|---|---|
|RB1|two complete turns A/B, rollback 1|History and metadata restore A.|
|RB2|complete A, bare context X, rollback 1|History removes A; metadata is empty, not A.|
|RB3|complete A, non-user compaction-task context X, complete B, rollback 1|Metadata restores A or the compaction epoch base according to surviving history; X never consumes a turn.|
|RB4|two complete turns, rollback 99|All real user history removed; metadata empty.|
|RB5|rollback 0 through direct history helper|No-op; public handler still rejects zero as today.|
|RB6|two cumulative rollback markers|Equivalent to applying both in order.|
|RB7|contextual user/developer updates immediately before rolled-back user|Updates attached to that turn are removed; metadata checkpoint count matches the real boundary.|
|RB8|preserved work notes near cut|Notes do not count as a rollback turn.|
|RB9|`GhostSnapshot` near cut|Snapshot behavior remains governed by existing history/undo semantics and never creates metadata.|

## 4. Compaction replay

|ID|Shape|Expected result|
|---|---|---|
|C1|complete A, standalone local `Compacted(replacement)`|Exact replacement; reference none; previous A.|
|C2|complete A, standalone remote `Compacted(replacement)`|Same metadata semantics as C1.|
|C3|complete A/current user, `Compacted(replacement), TurnContext(B)` adjacent|Exact replacement; reference B; previous remains the committed user model, not changed solely by B.|
|C4|`Compacted(replacement), EventMsg(...), TurnContext(B)` or `Compacted(replacement), context response items, TurnContext(B)`|No adjacency special case; B is pending until a later real user.|
|C5|pre-turn compaction, then `TurnContext(B), user, assistant`|Replacement base followed by committed B metadata.|
|C6|compaction clears an earlier pending context|Pending context cannot leak through the replacement.|
|C7|multiple compactions|Newest replacement is the active opaque base; previous settings carry through until a later user commits new settings.|
|C8|replacement includes work notes and snapshots|Exact raw replacement retained; prompt projection removes snapshots and retains current work-note behavior.|

## 5. Rollback around compaction

|ID|Shape|Expected result|
|---|---|---|
|CR1|compacted base, complete post-base turn B, rollback 1|Restore compaction epoch-base metadata.|
|CR2|compacted base, complete B/C, rollback 1|Restore B checkpoint.|
|CR3|compacted base, complete B, rollback 2|Rollback crosses opaque base; clear reference and previous settings conservatively.|
|CR4|mid-turn compacted base with adjacent reference, later B, rollback 1|Restore base previous/reference pair.|
|CR5|repeated rollback after opaque-base crossing, then new turn D|New D context/user re-establishes metadata normally.|
|CR6|bare context after compaction, rollback of earlier user|Bare context never absorbs the rollback count.|

## 6. Legacy compaction

|ID|Shape|Expected result|
|---|---|---|
|L1|legacy `replacement_history: None`|History rebuilt from selected historical user messages and summary with no injected current initial context.|
|L2|same rollout reconstructed under cwd/policy contexts X and Y|Historical compacted prefix is identical.|
|L3|complete A followed by legacy standalone compaction|Reference none; previous A.|
|L4|legacy compaction followed by `TurnContext(B), user B`|B commits normally after the opaque base.|
|L5|legacy compaction followed by adjacent `TurnContext(B)`|Reference B; previous unchanged; no fake user checkpoint.|

## 7. Continuation

|ID|History/rollout tail|Expected result|
|---|---|---|
|P1|real user only|Pending continuation.|
|P2|real user, reasoning/call without output|Pending continuation; cleanup trims dangling executable tail.|
|P3|real user, call, completed output, no final assistant|Pending continuation.|
|P4|real user, final assistant|No continuation.|
|P5|interrupted abort marker after incomplete user turn|Pending; marker cleanup behavior unchanged.|
|P6|standalone/manual compaction replacement ending in summary user|No regular continuation.|
|P7|pre-turn compaction with no later user|No continuation.|
|P8|mid-turn compaction plus adjacent context, no later assistant|Pending continuation.|
|P9|mid-turn compaction plus final assistant|No continuation.|
|P10|pending continuation after reference clear|Model comes from previous settings.|
|P11|work notes after an otherwise complete assistant tail|Work notes do not make the turn incomplete.|
|P12|custom pause event in old rollout|Existing policy remains: pause event itself is not required for continuation inference.|

## 8. Prompt projection and preservation

|ID|Case|Expected result|
|---|---|---|
|H1|call without output in raw history|Prompt clone has synthetic `"aborted"` output; raw history unchanged.|
|H2|orphan output|Prompt clone drops it; raw history unchanged.|
|H3|image-capable model|Historical images preserved in prompt.|
|H4|text-only model|Images replaced by placeholder in prompt clone.|
|H5|invalid tool image after contextual user item|Recovery reaches the tool output because contextual item is not a boundary.|
|H6|`GhostSnapshot`|Raw/rollout retained; prompt omitted; zero token estimate.|
|H7|oversized tool output|Live history truncated; rollout stores original; resume reapplies configured policy.|
|H8|system-role response item|Not admitted to raw history.|
|H9|`Other` response item|Not admitted or persisted.|

## 9. Prefix-cache and WebSocket tests

|ID|Case|Assertion|
|---|---|---|
|PC1|two ordinary turns|Second request input starts with the complete first request input.|
|PC2|settings change|Update items are appended after the prior prefix; old items are not rewritten.|
|PC3|queued steering in one logical turn|Incremental suffix begins with server-returned items followed by steering.|
|PC4|unchanged request properties|WebSocket request can use `previous_response_id` and suffix input.|
|PC5|changed tools/instructions/schema/model properties|Incremental WebSocket path is rejected.|
|PC6|compaction|Old prefix is intentionally lost; subsequent requests share the exact replacement as their new base.|
|PC7|legacy resume under two current contexts|No resume-time data appears inside the historical compacted prefix.|
|PC8|rollback|Surviving prefix is exact; rewritten/truncated suffix is not assumed incrementally reusable.|
|PC9|invalid-image mutation|Incremental path rejects the changed input until a new base is recorded.|

## 10. Integration scenarios

1. Start under model A, complete a turn, manually compact, resume, switch to smaller model B, and verify previous-model compaction still has model A evidence.
2. Trigger mid-turn automatic compaction with work notes, interrupt immediately after replacement, resume, and `/continue`; verify no duplicate user/context/skill injection.
3. Trigger standalone remote compaction, resume, and verify no pending regular continuation.
4. Complete A, start and cancel a B turn after context persistence but before user persistence, then rollback A; verify metadata is empty.
5. Compact, add two turns, rollback one, resume, and verify prompt context deltas are computed from the surviving checkpoint.
6. Resume a legacy rollout after changing cwd, sandbox, and user instructions; verify those values appear only in newly appended canonical context.

## 11. Compatibility and serialization

- Deserialize existing `TurnContextItem` and `CompactedItem` records with missing/new optional fields exactly as before.
- Reconstruct rollouts containing no lifecycle IDs.
- Preserve ordering of `Compacted` and adjacent `TurnContext` records.
- Verify fork and resume call the same reconstruction path and hydrate identical metadata.
- Verify live `thread_rollback` uses reconstructed history and metadata rather than a separate approximation.

## 12. Exit criteria

The patch is ready when:

- all rows marked mandatory above have assertions;
- no existing prompt-cache invariant changes;
- no current `/pause`, `/continue`, work-note, or undo test regresses;
- local and remote compaction share replay metadata semantics;
- the full branch test suite passes under the supported feature configuration.
