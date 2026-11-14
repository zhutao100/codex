# Agent and harness principles

Read before changing orchestration, model context, persistence or transport. The linked OpenAI articles explain the design rationale; the source and feature contracts below define this implementation.

## The harness owns the loop

The model produces output and tool requests. The harness constructs the next request, dispatches tools, applies execution policy and decides when work can finish. A user turn may contain multiple inference/tool cycles; one completed API response does not necessarily complete the user's task. Context has a finite budget, so compaction and tool-output handling are part of the loop's correctness. Source: [Unrolling the Codex agent loop](https://openai.com/index/unrolling-the-codex-agent-loop/).

Project consequences:

- Trace submission handling, task execution and terminal events together. Use [core protocol](protocol_v1.md) for terminology and [pause/continue](features/pause-continue.md) for interruption semantics.
- Preserve the distinction between accepted input and input consumed by a model request. [Steering](features/steering-and-queue.md) and [completion review](features/post_turn_completion_review/README.md) share this ownership boundary.
- Treat review, compaction and tool execution as workflows with their own cancellation and recovery contracts. A role label or a prompt instruction does not enforce tool permissions.

## Context structure is a performance contract

Stable instructions, tool definitions and history prefixes permit reuse of prior inference work. Reordering tools or editing early items can invalidate that reuse. Prefer append-only context updates where supported, and make unavoidable prefix replacement explicit. The agent-loop article explains why prompt construction matters; [WebSocket acceleration](https://openai.com/index/speeding-up-agentic-workflows-with-websockets/) explains the additional request-processing savings from reusing connection state.

Project consequences:

- Maintain complete tool-call/result pairs and preserve opaque reasoning/compaction data without reinterpretation.
- Keep request-only review frames at their stable insertion boundary, before follow-up reasoning and tool work; do not append them at the changing transcript tail. See [review request prefixes](features/post_turn_completion_review/request_prefix.md).
- Budget the request actually sent, including request-only frames and items after the usage baseline. Compaction replaces model-visible context; it does not establish exact user-request provenance. See [review accounting](features/post_turn_completion_review/budget_accounting.md) and [remote compaction](features/remote_compaction_v2/README.md).

## Transport acceleration must preserve recovery

WebSocket mode uses `response.create` and `previous_response_id` with connection-local cached state. The article's earlier `response.append` prototype was not the launched design. Reuse removes repeated processing; it does not replace the client's conversation record. Source: [Speeding up agentic workflows with WebSockets](https://openai.com/index/speeding-up-agentic-workflows-with-websockets/).

Project consequences:

- Only send a delta when the saved input/output prefix and request settings permit continuation. Preserve a complete logical input for reconnect or HTTP fallback.
- Compaction, edits to history, cancellation and changed settings can invalidate the chain. Keep transport cache invalidation aligned with those operations; never persist a response ID as a substitute for a rollout.
- HTTP/SSE and WebSocket paths must preserve tool and history semantics. Consult [Responses WebSocket transport](features/responses-websocket.md) for the implemented retry and fallback behavior; newer API capabilities are not implied by the article.

## Repository knowledge is loaded progressively

A short `AGENTS.md` points agents to structured, versioned knowledge. Durable design decisions should be discoverable in the repository; details should be loaded when relevant. Repeated operational mistakes are better addressed with an executable check or a scoped contract than more always-loaded prose. Source: [Harness engineering](https://openai.com/index/harness-engineering/).

Project consequences:

- Start at the [docs index](README.md), then the relevant feature contract, then its source/tests. Keep indexes task-oriented rather than mirroring the source tree.
- Retain invariants, choices and useful limitations; remove session narratives, incident dumps and stale duplication. Active plans state what remains; completed work belongs in feature docs.
- Match evidence to the claim: source inspection establishes implementation; fixture tests establish deterministic paths; a live evaluation establishes observed model behavior. Record the relevant checks for a change without turning docs into a growing run history.
