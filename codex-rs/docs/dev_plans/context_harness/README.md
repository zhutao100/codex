# Context acquisition improvements

Status: proposed runtime work. Existing work-note carryover and streaming remote compaction are implemented features; preserve them. This plan addresses output lost before it reaches history and disposable exploration evidence that needlessly enlarges the parent's context.

|Plan|Current gap|Next step|
|---|---|---|
|[Recoverable tool output](recoverable_tool_output.md)|Capture/preview truncation has no durable address for all omitted bytes.|Add bounded storage at the original byte reader and prove retrieval without rerunning the command.|
|[Bounded exploration](bounded_exploration.md)|The opt-in explorer has a model profile but no dedicated bounded-report or enforced read-only contract.|Specialize the existing role and verify effective capability and fresh-thread behavior.|
|[Evaluation](evaluation_plan.md)|Source inspection does not establish live quality or total-cost gains.|Create small known-answer fixtures and measure parent and aggregate resource use separately.|

## Decisions

- Improve acquisition before raising context limits or adding a new compaction mechanism. Notes preserve working conclusions; they cannot recreate discarded raw bytes.
- Keep existing tool/result and rollout boundaries. No app-server migration, new history envelope or newer multi-agent API is required for the initial changes.
- Preserve applicable instructions, exact notes, review provenance, pending input, cancellation ownership and WebSocket reset semantics. Use [feature docs](../../features/README.md) for those contracts.
- Keep collaboration opt-in. Existing runtime schemas determine which arguments and tools are valid; this backend uses fresh children and `wait`, without `fork_turns` or `fork_context`.
- Parent-context reduction, total usage, elapsed time and correctness are separate outcomes. Do not promote defaults based solely on fewer parent tokens or compactions.

## Work sequence

1. Capture baseline behavior with the small fixtures in the evaluation plan.
2. Implement output storage for unified execution, then legacy shell, using one bounded storage contract. Keep exposure opt-in until failure and retrieval checks pass.
3. Implement the explorer specialization independently; retain a direct path for simple lookups.
4. Compare acquisition guidance and delegation independently, then add output storage as a separate ablation.
5. Record the supported contract and any measured tradeoffs. Move implemented behavior into feature docs and remove completed plan sections; retain only unresolved decisions here.
