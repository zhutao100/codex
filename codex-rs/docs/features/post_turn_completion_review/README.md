# Completion review

Independent review of completed work and the exact latest main answer. Read this before changing `/review-completed-turn`, automatic review, or a review-derived continuation.

|Need|Read|
|---|---|
|Capture, tickets, persistence and recovery|[State and decisions](design.md)|
|Request ordering and WebSocket continuation|[Request prefix](request_prefix.md)|
|Usage ledger, exact snapshots and overflow|[Budget accounting](budget_accounting.md)|
|Regression tests and live evaluation|[Validation](validation.md)|

## Invocation and output

- Manual: `/review-completed-turn` after completed main work.
- Automatic: `[features].auto_post_turn_completion_review = true`; disabled by default.
- `review_model` and `review_model_provider` use the same resolution as ordinary `/review`.
- Each reviewer starts a fresh thread in the candidate's completed-task directory, with a read-only sandbox, no approval escalation, disabled web search and disabled collaboration.
- The public result is `{ "evaluation": "...", "fix_actions_advised": true }`: evaluation must be nonempty and the advice flag must be a boolean.

Positive findings become advisory developer input to main work. They do not create a user request. The latest main answer remains streamed and copyable; review status is separate, and a clean verdict does not produce another synthesis answer.

An episode allows three automatic attempts. Positive advice on the final attempt remains visible but does not start another automatic continuation. Manual review can still hand off advice without resetting the automatic allowance. Pause/resume keeps the same ticket and allowance.

## Exact context and replacement answers

The reviewer receives a frozen consumed request, including known prior user rounds and their terminal answers; the episode's initial answer as an optional historical reference; and the exact latest completed answer as the mandatory candidate. The current episode contributes at most two answer texts, deduplicated when identical. Intermediate answer revisions, earlier verdicts, reasoning and tools are excluded. Quoted user content stays exact, including text that resembles review markup.

The initial reference can be wrong or superseded. It cannot compensate for omissions in the candidate. The reviewer inspects current artifacts to distinguish missing work from incomplete communication.

Every review-derived main continuation must produce a complete replacement answer, even if it rejects all findings or changes no files. It preserves still-valid results, incorporates corrections, removes superseded claims, and reports actual verification and remaining limitations. This host-owned contract survives custom prompts, pause, restart and compaction. Its request-only placement and accounting are described in the linked contracts.

## Configuration

|Setting|Purpose|
|---|---|
|`post_turn_completion_review_prompt_file`|Override the reviewer prompt without removing host-owned context semantics.|
|`review_prompt_file`|Configure the separate ordinary-review prompt.|
|`post_turn_completion_review_agents_filename`|Preferred host instruction filename; default `AGENTS.post-turn-review.md`.|
|`review_agents_filename`|Host instruction fallback; default `AGENTS.review.md`.|
|`host_agents_filename`|Final host instruction fallback; default `AGENTS.md`.|

Host instruction filenames resolve under `CODEX_HOME`; project instructions are loaded for the candidate directory. Native model prompt overrides use `model_instructions_file`; `Config.base_instructions` is a programmatic field, not a top-level TOML option.

## Limits and recovery

|Condition|Behavior or next step|
|---|---|
|Another task is running, no completed candidate, or unconsumed input|Wait for the relevant main work to complete; pending input must be consumed.|
|Unknown or invalidated request provenance|Full review is unavailable. Start a new session with the complete request, or recover only from verified host-owned checkpoints. Another prompt or reload cannot restore missing requirements.|
|Nontext input or `request_user_input` clarification answers|Main work continues normally, but the current text projection cannot claim complete review coverage.|
|Reviewer compaction, malformed/empty output, delegate failure or mandatory-packet overflow|No structured clean verdict is accepted. Mandatory scope and candidate are never truncated.|
|Main follow-up budget overflow|The continuation remains available. `/compact` or explicitly select a larger main model, then `/continue`; compaction cannot help if mandatory snapshots alone exceed capacity.|
|Candidate-directory mismatch|Restore the completed task's directory before continuing. Rejection preserves the pending continuation.|
|Checkpoint I/O failure|Repair storage and reload the last durable rollout. Preserve the storage error instead of reporting a missing candidate.|

Tickets enforce identity and scheduling ownership, not a filesystem snapshot or exactly-once tool effects. Current artifacts are inspected at review time. Semantic completeness remains a model judgment; a valid review result does not certify every claim in an answer.
