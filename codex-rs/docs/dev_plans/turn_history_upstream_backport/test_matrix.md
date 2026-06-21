# Test Matrix

## 1. Replay association

|Case|Rollout shape|Expected metadata|
|---|---|---|
|Normal turn|`TurnContext(A), user(A), assistant(A)`|Reference A; previous model A|
|No-diff normal turn|No context `ResponseItem`, then `TurnContext(A), user(A)`|Still commits A|
|Bare trailing context|`TurnContext(A), end of rollout`|No new reference or previous settings|
|Multiple candidates before user|`TurnContext(A), TurnContext(B), user`|Latest candidate B commits|
|Old rollout without context|`user, assistant`|History restored; no invented model metadata|
|Duplicate user event|`user ResponseItem, UserMessage event`|One checkpoint, not two|
|Pending user steer within one logical run|Second non-contextual user `ResponseItem`|A second branch-local rollback checkpoint, matching `drop_last_n_user_turns`|
|Synthetic compacted summary in replacement base|User-role summary message|Part of the opaque base; no fabricated per-original-turn metadata checkpoint|

## 2. Compaction

|Case|Rollout shape|Expected result|
|---|---|---|
|Pre-compaction task context|`TurnContext(X), Compacted(replacement)`|Pending X discarded|
|Compaction clears reference|`Compacted(replacement)`|Reference none; previous settings preserved|
|Compaction reinjects context|`Compacted(replacement), TurnContext(X)` adjacent|Reference X; previous settings unchanged|
|Next normal turn after no-inject compaction|`Compacted, context ResponseItem, TurnContext(Y), user(Y)`|TurnContext Y remains pending until user Y|
|Legacy compaction|`Compacted(replacement_history=None)`|Rebuild with empty initial context; reference none|
|Crash after compacted record|`Compacted, end of rollout`|No assumed reference baseline|
|Incomplete turn compacted after user|`TurnContext(A), user(A), Compacted`|Previous A preserved; reference cleared|

## 3. Rollback

|Case|Expected result|
|---|---|
|Rollback one of two ordinary turns|History and metadata both restore first turn|
|Rollback after bare `TurnContext`|Bare record does not consume a rollback count|
|Rollback after non-user compact task|Compaction metadata does not count as a user turn|
|Rollback one post-compaction turn|Restore epoch base metadata|
|Rollback crosses compacted base|Apply history truncation; clear uncertain reference and previous settings|
|Rollback exceeds all known turns|Preserve only pre-first-user history; clear metadata|
|Two sequential rollback events within an epoch|Second event operates on already-updated checkpoints|
|Rollback crosses opaque base, then a new turn and rollback|Epoch was reset; later rollback uses only post-reset checkpoints|
|Rollback zero|Existing handler rejection remains unchanged|

## 4. `/pause` and `/continue`

|Case|Expected result|
|---|---|
|Interrupted after real user boundary|Continuation model comes from committed previous settings|
|Interrupted before real user boundary|Bare pending context does not select continuation model|
|Continue without new input|No new user checkpoint and no new previous-settings commit|
|Continue after dangling function call|Cleanup removes/normalizes incomplete tail before request|
|Continue after interrupted marker|Configured marker removal is mirrored in history and rollout|
|Compaction during continuation|Adjacent post-compaction context may restore reference; no fake user turn|
|Pending steer during continuation|Can be drained under existing continuation ordering|

## 5. Context-boundary classification

Test each contextual user item before rollback and invalid-image recovery:

- user instructions;
- skill instructions;
- session prefix;
- user shell command wrapper;
- preserved work notes.

Expected:

- none counts as a real user-turn checkpoint;
- none stops invalid-image recovery as though it were a new real user turn;
- rollback may trim contiguous contextual updates associated with a removed real turn.

## 6. Invalid-image recovery

|Case|Expected result|
|---|---|
|Tool image is latest eligible item|Replace image with placeholder text|
|Contextual user message follows tool image|Still replace tool image|
|A new real user message follows tool image|Do not cross boundary|
|Image belongs to user message|Do not rewrite user image|
|No eligible tool image|Emit existing error behavior|

## 7. Legacy-resume determinism

Construct one legacy rollout and reconstruct it with different current turn contexts:

|Changed resume setting|History before next turn must remain equal|
|---|---|
|Working directory|Yes|
|Sandbox/approval policy|Yes|
|Developer instructions|Yes|
|User instructions|Yes|
|Collaboration mode/personality|Yes|
|Current model|Yes|

After the next real turn begins, current context is appended canonically because the reference baseline is absent.

## 8. Prefix and request shape

|Case|Expected request relation|
|---|---|
|Second ordinary turn with no rewrite|Previous full input is a prefix of new full input|
|Tool follow-up within turn|Previous input + server items is a prefix; suffix-only WebSocket request allowed when properties match|
|Settings diff only|Old history remains unchanged; diff is appended|
|Full reinjection after conservative clear|Old history remains, full current context is appended once|
|Compaction|Replacement becomes new base; WebSocket continuation cleared|
|Rollback|Latest continuation invalidated; reconstructed history equals selected older prefix/rewrite|
|Resume with the same truncation policy|Normal rollout outputs reconstruct to the same processed content as the live session|
|Resume with a different truncation policy|Document existing behavior: normal rollout outputs may reconstruct differently; the mandatory patch does not silently change persistence semantics|
|Resume from a replacement-history checkpoint|Checkpoint vector is installed without retruncation|

## 9. Existing features that must remain green

- previous-larger-model downshift compaction;
- preserved-work-notes capture and reinsertion;
- local and remote compaction replacement history;
- post-turn completion review;
- delegate/subagent workflows used by this branch;
- ghost snapshot storage and prompt omission;
- modality-aware image stripping;
- token accounting after the last model-generated item;
- fresh explicit input before queued pending input;
- original-output rollout persistence and resume-time retruncation;
- replacement-history installation without item reprocessing.
