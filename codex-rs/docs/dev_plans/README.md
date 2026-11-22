# Active plans

This index contains unfinished work only. A plan records current status, accepted constraints, unresolved decisions and concrete next steps. Implemented behavior belongs in the [feature index](../features/README.md) or [development guidance](../development.md).

|Plan|Status|Next decision|
|---|---|---|
|[Native feature alignment](feature_alignment/README.md)|Cross-platform controls and the Linux sandbox migrated.|Qualify Windows ConPTY and broader shell snapshot support on their native targets before changing defaults.|
|[Context harness](context_harness/README.md)|Experimental evidence and waiting contracts implemented.|Run controlled model comparisons before changing defaults; validate additional platform boundaries before extending support.|
|[Build optimization](cargo_build_optimization/README.md)|Partially implemented; profiles, async adapters and hidden sidecars already exist.|Measure the remaining CLI dependency cost before selecting another split.|

When work completes, move its durable contract and choices into the appropriate feature/development page, update links and remove the completed plan. Delete abandoned work instead of accumulating inactive proposals or repeated implementation histories.
