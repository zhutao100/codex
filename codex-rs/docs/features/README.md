# Feature contracts

Choose the feature being changed. Each page states current behavior, non-obvious decisions, limitations and relevant source/tests; detailed review and compaction pages have their own indexes.

## Session and context

|Change|Read|
|---|---|
|Pause, interrupt, continue or resume unfinished work|[Pause and continue](pause-continue.md)|
|Accept mid-turn input or manage queued messages|[Steering and queue](steering-and-queue.md)|
|Capture exact work notes around compaction|[Session work notes](session-work-notes.md)|
|Reconstruct history, compaction epochs or rollback|[History and replay](history-replay.md)|
|Select/run streaming compaction or install its replacement|[Remote compaction](remote_compaction_v2/README.md)|
|Change Responses transport, caching or fallback|[WebSocket transport](responses-websocket.md)|

## Reviews and delegated work

|Change|Read|
|---|---|
|Review a completed answer or continue from advice|[Completion review](post_turn_completion_review/README.md)|
|Select a review model/provider and resolve its authentication|[Review provider](review-provider.md)|
|Display the active delegate's model, status or usage|[Delegate runtime status](delegate-runtime-status.md)|

## Models and configuration

|Change|Read|
|---|---|
|Refresh bundled models or change discovery/cache identity|[Model catalog](model-catalog.md)|
|Overlay model metadata, instructions or provider bindings|[Model overlay](model-overlay.md)|
|Select, normalize or persist service tiers|[Service tier](service-tier.md)|

## Tools and presentation

|Change|Read|
|---|---|
|Install local skills/MCP bundles or change activation|[Local plugins](local_plugins.md)|
|Grant dedicated temp writes in read-only sessions|[Read-only temporary directories](read-only-temp.md)|
|Fetch and display account token activity|[Usage command](usage-command.md)|

For core operations or client integration, return to the [documentation index](../README.md). Proposed output-storage and explorer changes remain in [active plans](../dev_plans/README.md).
