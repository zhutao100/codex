# Documentation

Start with the topic needed for the change. The implementation and its tests define current behavior; these docs preserve contracts, decisions and limitations that are costly to rediscover.

|Task|Read next|
|---|---|
|Change the agent loop, context, persistence or request transport|[Project principles](principles.md)|
|Change an implemented feature|[Feature index](features/README.md)|
|Build, test or update generated schemas|[Development](development.md)|
|Integrate with core submissions/events|[Core protocol](protocol_v1.md)|
|Expose Codex to an MCP client|[MCP server interface](codex_mcp_interface.md)|
|Build an app-server client|[App-server contract](../app-server/README.md)|
|Continue unfinished design work|[Active plans](dev_plans/README.md)|

For configuration fields, consult `ConfigToml` in [configuration loading](../core/src/config/mod.rs) and the generated [configuration schema](../core/config.schema.json); feature docs explain custom settings and their interactions. Defaults resolved from model metadata or runtime state are not all represented by schema defaults.

## Keep this index useful

Add a route when a durable topic is introduced. Keep detailed rules in the owning document, and link related contracts instead of copying them. Completed plans become feature docs; historical implementation logs and local verification captures stay out of this knowledge base. See [documentation maintenance](../AGENTS.md#documentation-maintenance).
