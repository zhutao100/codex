# Task

Understand the `run_turn` workflow (line 3365-3599 in `core/src/codex.rs`), then design/propose the changes following the objective, requirements and restrictions.

## Context

- The model's `auto_compact_limit` is a conservative number with buffers, so the semantic meaning of `token_limit_reached` is actually like "token limit approaching".
- the current `run_auto_compact` mainly preserves the user inputs, and it largely compress the model's multi-turn run results within the session.
- The current `run_auto_compact` can be very lossy, especially in the cases like
  - the model has mainly been making analysis and reasonings efforts within the current session, and has not reached to conclusions,  made file edits or output analysis results. In this case,
    - the model may have made a few hypotheses, validated some, ruled out some, and left some to be further investigated.
    - a lossy compact can trap the new session into starting from scratch then repeating working on the hypotheses that has been validated or ruled out.
  - the model has read and inspected a lot of files, and was in the middle of relevant file edits. In this case,
    - the model may have found out some files were relevant and some others were irrelevant to the task goal; or that, the model may have concluded the relevance of some files to the task can be simply summarized in one sentence, so that such files do not need to be read again. 
    - a lossy compact can make the new session repeat on reading the irrelevant files, or the files that do not need to be read again.

## Objective

When the token limit is approaching and follow up work is still needed, before invoking compaction, 
- make the model work within the current session for one more round, systematically summarize and explicitly output the session work notes, so that the notes can help the new session minimize the losses and avoid the compression loss traps like stated above.
  - **note that** the work note generation round should preserve the existing input prefix unchanged, so that it can leverage the existing input prefix cache and avoid re-processing the long multi-round session history without cache; tricks like "with the tools disabled" "change the tools" will likely change the input prefix and make the prefix cache unmatched, take it into careful consideration when making the design.
- preserve the notes unchanged/uncompressed, and properly inject it into the new session.

## Possible Change Injection Spots

At line 3548 in `run_turn` in `core/src/codex.rs`,
```rust
                if token_limit_reached && needs_follow_up {
                    run_auto_compact(&sess, &turn_context).await;
                    continue;
                }
```

## Restrictions

- The changes are going to be a drop-in diff commit, staying on top of the current project. The long-term maintenance patterns are like
  - check out a branch from the current upstream/origin main branch.
  - apply the change commit.
  - over time, keep calling `git pull origin main` to rebase the change commit on top of the latest upstream code.

  Thus, the design of the changes target to minimize the potentials of future rebase conflicts.