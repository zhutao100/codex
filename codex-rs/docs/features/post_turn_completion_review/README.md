# Post-Turn Completion Review

Independent read-only review of current work and the exact latest main answer. [State and decisions](design.md). [Verification](validation.md).

## Invocation

- Manual: `/review-completed-turn` after a completed main task.
- Automatic: `[features].auto_post_turn_completion_review = true`; disabled by default.
- Uses `review_model` and `review_model_provider`, with the same provider selection as `/review`.
- Always uses a fresh `InitialHistory::New` delegate, read-only policy, no approval escalation, disabled web search and disabled collaboration.

```toml
[features]
auto_post_turn_completion_review = true
```

## Context Contract

An episode contains one consumed user-request basis and its review-derived main continuations. The version 2 packet contains:

1. Frozen genuine user context, including consumed steering and earlier known user rounds with their terminal answers.
2. The episode's initial answer as a historical, unverified reference.
3. The exact latest completed main answer as `candidate_final_answer`.

The current episode contributes at most two answer texts. Identical initial/candidate text is serialized once. Older candidate revisions, reviewer evaluations, hidden reasoning, tool transcript and intermediate assistant messages are not appended. Text quoting review material remains exact data; it is not scrubbed.

The historical reference may be incorrect or superseded. It must not compensate for an incomplete current candidate. Independent inspection distinguishes missing implementation from incomplete communication.

## Replacement Answers

Every review-derived main continuation receives a host-owned complete-answer contract, including after pause, restart and compaction. It must answer the active user request at the requested level of detail, preserve still-valid results, incorporate material corrections, remove superseded claims, and accurately state verification and limitations. This applies when all findings are rejected or no files change.

The latest main answer remains streamed and copyable. Review evaluation/status stays separate; a clean review does not generate another synthesis answer or rewrite previous waterfall entries.

## Outcomes And Limits

- Public output remains `{ evaluation, fix_actions_advised }`, with a nonempty evaluation and a strict boolean.
- Positive findings are advisory developer input, never a fabricated user request.
- Invalid/empty output, delegate failure, lost coverage or mandatory context overflow produces no structured clean verdict.
- Three automatic attempts are allowed per episode. Positive advice on the last attempt remains visible and stops automatic continuation; it is not approval.
- Manual review retains one advisory handoff after that allowance is exhausted and does not reset it. Pause/resume reuses the same ticket and allowance.
- Payload budgeting uses the resolved review model's effective window, counts instructions and static tool schemas, reserves additional capacity for tools/output/inspection, and omits the optional historical reference first. Mandatory user scope and candidate text are never truncated.
- If a delegate starts compaction, coverage becomes unavailable rather than accepting a verdict against rewritten context. Main continuations similarly retain exact mandatory snapshots or stop with a recoverable budget diagnostic.

## Configuration

Optional prompt overrides remain supported:

```toml
review_prompt_file = "~/.codex/prompts/review.md"
post_turn_completion_review_prompt_file = "~/.codex/prompts/post-turn-review.md"

host_agents_filename = "AGENTS.md"
review_agents_filename = "AGENTS.review.md"
post_turn_completion_review_agents_filename = "AGENTS.post-turn-review.md"
```

Post-turn host instructions prefer the post-turn filename, then the generic review filename, then the host filename. Filenames resolve under `CODEX_HOME`; project instructions remain available. A configured post-turn prompt cannot remove host-owned candidate/reference semantics or the parent's replacement-answer contract.

## Provenance Limits

Versioned rollout metadata restores exact snapshots without relying on nonpersisted `TurnComplete` events. Prepared handoffs are recoverably queued; started handoffs resume as interrupted main work, not a fresh advisory action.

Legacy histories without trusted metadata, or discarded histories whose request basis cannot be established, are unavailable for full review. Arbitrary assistant-role text is never promoted to a known completion. Start a new session with the complete request when that provenance is unavailable. Nontext user input also produces an explicit coverage limitation rather than a claimed full text-only review.

The harness enforces identity, isolation, revision bounds and scheduling ownership. Natural-language completeness remains a model requirement and an independent review judgment, not a deterministic certification. Current repository files are inspected at review time; tickets do not provide filesystem snapshot isolation.
