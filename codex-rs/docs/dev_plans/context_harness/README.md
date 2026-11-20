# Context harness improvements

Status: proposed runtime work. Existing work-note carryover, streaming remote compaction, pause/continue and isolated completion review are implemented contracts to preserve. This work addresses evidence lost before it reaches history, unnecessarily large exploration contexts, and inference calls spent waiting for shell jobs. A documentation update does not establish that any proposal below is implemented.

|Plan|Decision|Initial scope|
|---|---|---|
|[Recoverable tool output](recoverable_tool_output.md)|Capture before lossy fanout and truncation; expose a bounded preview plus a usable reference.|Unified execution, then legacy shell; all execution outcomes.|
|[Bounded acquisition](bounded_acquisition.md)|Make search/read limits and incomplete coverage explicit before wider tool exposure.|Reuse existing handlers; keep direct shell access and modest whole-file reads useful.|
|[Bounded exploration](bounded_exploration.md)|Fresh, capability-restricted explorers return concise evidence and unresolved scope.|Specialize the existing opt-in collaboration backend and existing schema.|
|[Waits and background completion](background_completion.md)|First extend empty waits with bounded collection; later add explicitly owned completion delivery.|Long waits can ship separately; idle continuation requires durable waiting ownership.|
|[Evaluation](evaluation_plan.md)|Measure acquisition, artifacts, delegation and scheduling separately before combining them.|Deterministic counterexamples, repeated known-answer tasks and complete usage accounting.|

## Decisions

- Improve acquisition and scheduling before raising context limits or adding another compactor. Notes preserve established conclusions; they cannot recreate discarded raw bytes. An opaque compaction item does not prove perfect semantic retention.
- Keep model-visible history, UI events and durable host metadata distinct. Stored output belongs outside the prompt until explicitly retrieved; completion events do not themselves authorize another inference turn.
- Preserve ordered call/result pairs, stable earlier messages, exact opaque items and applicable project instructions. Coordinate context replacement with [WebSocket continuation](../../features/responses-websocket.md), [work notes](../../features/session-work-notes.md) and [remote compaction](../../features/remote_compaction_v2/README.md).
- Preserve accepted-versus-consumed input, review provenance and cancellation ownership through [steering](../../features/steering-and-queue.md), [pause/continue](../../features/pause-continue.md) and [completion review](../../features/post_turn_completion_review/README.md). A late job result must not reopen a finished or superseded user task.
- Keep collaboration opt-in and use the existing schema: fresh children with `message`/`agent_type`, `send_input`, `wait` and `close_agent`. Do not copy newer `fork_turns`/`fork_context` examples into this backend.
- Parent peak context, cumulative input, cache usage, total cost, elapsed time and correctness are separate outcomes. Do not infer physical recomputation from logical input counts or select waits against a universal cache TTL.
- Reuse concrete reference mechanisms: Claude's artifact-reference previews and completion queue; newer Codex's metadata-aware result sizing, bounded collection and separate empty-wait limit. Their defaults, feature availability and failure behavior are not contracts for this implementation.

## Work sequence

1. Capture the baseline with small known-answer fixtures. Establish effective limits and executed paths from source and captured requests, not tool descriptions alone.
2. Land bounded per-wait collection and longer empty waits as one small scheduling change. In parallel, implement capture-before-loss and actual artifact retrieval; neither feature substitutes for the other.
3. Correct acquisition completeness/line handling before exposing structured tools more widely. Evaluate lightweight acquisition guidance separately from tool exposure.
4. Specialize fresh explorers with enforced capabilities and bounded evidence reports. Compare acquisition and delegation independently while holding output storage and waiting behavior fixed.
5. Add durable shell-outcome delivery only after registration, history admission and consumption are explicit. Add idle continuation after the waiting-owner and interactive/headless lifecycle contracts are implemented; retain long waits as a usable earlier milestone.
6. Publish measured tradeoffs with the corresponding feature contracts. Move implemented behavior into `docs/features/` and remove completed plan sections; keep raw research and run records outside durable project docs.
