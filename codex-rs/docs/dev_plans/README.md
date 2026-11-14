# Active plans

This index contains unfinished work only. A plan records current status, accepted constraints, unresolved decisions and concrete next steps. Implemented behavior belongs in the [feature index](../features/README.md) or [development guidance](../development.md).

|Plan|Status|Next decision|
|---|---|---|
|[Context acquisition](context_harness/README.md)|Proposed runtime changes; exact work notes and remote compaction already exist.|Define and implement bounded recoverable output and explorer contracts, then evaluate quality and aggregate cost.|
|[Build optimization](cargo_build_optimization/README.md)|Partially implemented; profiles, async adapters and hidden sidecars already exist.|Measure the remaining CLI dependency cost before selecting another split.|

When work completes, move its durable contract and choices into the appropriate feature/development page, update links and remove the completed plan. Delete abandoned work instead of accumulating inactive proposals or repeated implementation histories.
