# Codex MCP server interface

Read when integrating an MCP client or modifying `mcp-server`. Start `codex mcp-server` (or `codex-mcp-server`) as a child process with bidirectional stdio. Messages are newline-delimited JSON-RPC. `codex mcp` manages external MCP-server configuration; it does not start this server.

The server exposes MCP tools `codex` and `codex-reply`. Thread/account RPCs such as `newConversation`, `thread/start`, `turn/start` and `model/list` belong to the separate [app-server interface](../app-server/README.md).

## Discovery and calls

Perform the MCP initialization handshake, then discover schemas through `tools/list` and call tools through `tools/call`. For interactive inspection:

```bash
npx @modelcontextprotocol/inspector codex mcp-server
```

|Tool|Inputs and behavior|
|---|---|
|`codex`|Required `prompt`; optional model, profile, cwd and configuration overrides; starts a session|
|`codex-reply`|`threadId` and `prompt`; continues an existing session; legacy `conversationId` remains accepted|

The initial tool uses kebab-case keys such as `approval-policy`, `base-instructions`, `developer-instructions` and `compact-prompt`. The reply uses camelCase `threadId`. Its initial `sandbox` enum accepts `read-only`, `workspace-write` and `danger-full-access`. Read [tool parameter/schema definitions](../mcp-server/src/codex_tool_config.rs) for exact supported fields; do not substitute app-server parameter names.

Example after initialization:

```json
{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"codex","arguments":{"prompt":"Explain the failing test.","approval-policy":"on-request","sandbox":"read-only"}}}
```

A successful result includes ordinary text content and structured continuation data:

```json
{"content":[{"type":"text","text":"Answer text"}],"structuredContent":{"threadId":"<returned thread ID>","content":"Answer text"}}
```

Use the returned ID in a later call:

```json
{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"codex-reply","arguments":{"threadId":"<returned thread ID>","prompt":"Apply the focused fix."}}}
```

`codex-reply` requires a thread still loaded in the same server process; it does not reopen a rollout after restart. Keep at most one outstanding tool call per thread because each call consumes that thread's event queue. See [reply dispatch](../mcp-server/src/message_processor.rs).

`structuredContent.content` is a string, not a duplicate array of content blocks. Handle tool errors as MCP errors/results rather than assuming every call provides continuation data. See [tool execution/result construction](../mcp-server/src/codex_tool_runner.rs).

## Events and approvals

During a call, the server sends `codex/event` notifications containing the serialized core event and `_meta` correlation (`requestId`, and `threadId` when available). Keep reading the connection while a tool call is pending. See [notification serialization](../mcp-server/src/outgoing_message.rs) and the [core protocol](protocol_v1.md).

Execution and patch approvals use server-to-client `elicitation/create` requests. Codex-specific fields identify the thread, tool call and approval event; the client must answer the actual JSON-RPC request ID.

The implementation currently expects a Codex-specific `{ "decision": ... }` result, using `ReviewDecision` values such as `approved`, `approved_for_session`, `denied` or `abort`. It does not use `{ "decision": "allow" }`. This response shape is not the standard MCP elicitation `action`/`content` result; clients must account for that implementation limitation. The typed policy-amendment variant and exact request fields are defined in [execution approvals](../mcp-server/src/exec_approval.rs), [patch approvals](../mcp-server/src/patch_approval.rs) and [ReviewDecision](../protocol/src/protocol.rs).

## Interaction limitations

Only execution and patch approvals have answer bridges. Generic MCP elicitation, `RequestUserInput` and dynamic-tool requests are forwarded as events without a corresponding answer bridge.

Cancellation submits a core interrupt, but the runner currently forwards `TurnAborted` without completing its `tools/call` response or stopping its event-consumer loop. Interrupted-call cleanup is incomplete: a core terminal notification does not guarantee a tool result or a safely reusable per-thread call slot. The runner does return results for `TurnComplete`, `Error` and `TurnPaused`. See [cancellation dispatch](../mcp-server/src/message_processor.rs) and [event handling](../mcp-server/src/codex_tool_runner.rs).

## Change entry points

[Message dispatch](../mcp-server/src/message_processor.rs) defines supported MCP methods and tools. [MCP integration tests](../mcp-server/tests/suite/codex_tool.rs) exercise actual calls; schema and event/result serialization tests live with their owning types. Update the relevant tests when changing wire shapes. This interface remains experimental.
