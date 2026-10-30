# Context Harness Improvements

## Status

Research complete; runtime changes proposed, not implemented. The instruction corrections described in [instruction audit](instruction_audit.md) are applied separately from the proposed runtime work.

## Assessment

The article's useful result is that independent exploration contexts can reduce parent-history growth. It does not establish a general model ranking, a universal compaction cause, or lower total cost. This fork already has exact work-note carryover and streaming remote compaction; those mechanisms should be preserved, not proposed again.

The remaining opportunities are complementary: acquire less disposable output, keep omitted evidence addressable, and give exploratory workers bounded responsibilities and verifiable reports.

## Read Next

|Document|Purpose|
|---|---|
|[Source audit](source_audit.md)|Revision-qualified verification of the article and supplied prior assessment, including differences between the primary fork and reference snapshots.|
|[Recoverable tool output](recoverable_tool_output.md)|First runtime candidate: bounded output artifacts captured before destructive buffering, with stable retrieval after compaction and restart.|
|[Bounded exploration](bounded_exploration.md)|Second runtime candidate: specialize the existing opt-in explorer without importing a newer multi-agent backend.|
|[Evaluation plan](evaluation_plan.md)|Shared correctness, coverage, acquisition, recovery, latency and aggregate-cost gates; separates scripted fixtures from live model evaluations.|
|[Instruction audit](instruction_audit.md)|Observed navigation failures, canonical instruction fixes and verification evidence.|

## Sequence

1. Establish the evaluation manifest and small synthetic fixtures before modifying runtime behavior.
2. Implement recoverable output for unified execution, then the legacy shell path using the same storage contract. Keep each path disabled until its failure and retrieval checks pass.
3. Specialize the opt-in explorer and pilot bounded acquisition guidance. Do not enable collaboration globally as part of this work.
4. Run the acquisition/delegation factorial evaluation, then add output artifacts as a separate ablation.
5. Promote defaults only after correctness and resource gates pass; retain negative and inconclusive results.

## Constraints

- Keep drop-in patches on existing branch types, tool results and rollout boundaries. No app-server migration, history-envelope port, new compaction implementation or multi-agent-v2 backport.
- Preserve exact notes, review provenance, pending input, cancellation ownership, reference context and WebSocket reset behavior.
- Do not silently remove applicable project instructions from workers or claim read-only behavior from a prompt alone.
- Bound parent output and worker output independently. Lower parent occupancy is not lower aggregate usage, and zero compactions is not an answer-quality metric.
- Prefer the existing shell/file access and optional exploration tools over introducing a second general search framework.
