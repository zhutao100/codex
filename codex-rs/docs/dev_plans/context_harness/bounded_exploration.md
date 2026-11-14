# Plan: bounded exploration

Status: proposed specialization of the existing opt-in `collab` backend. It does not enable collaboration by default.

## Confirmed behavior and gap

[Agent roles](../../../core/src/agent/role.rs) give Explorer a model/effort override but no dedicated exploration instructions and no forced read-only policy. Its tool description broadly encourages trust and delegation. [AgentControl](../../../core/src/agent/control.rs) already creates fresh child threads rather than copying parent history. Applicable inherited configuration/instructions still consume child context.

The exposed [tool schema](../../../core/src/tools/spec.rs) accepts `spawn_agent` with `message` and optional `agent_type`; communication uses `send_input`, `wait` and `close_agent`. Do not add unsupported `fork_turns`, `fork_context`, model or effort arguments in examples. The [collaboration handler](../../../core/src/tools/handlers/collab.rs) owns the lifecycle and depth checks.

## Intended behavior

- Use direct lookup for a known path or narrow symbol question. Delegate independent research when its raw evidence is much larger than the report the parent needs.
- Assign explicit scope, revision/CWD, question, applicable constraints and expected evidence. Workers return findings, source locators, coverage and unresolved uncertainty, with a bounded report target.
- The parent integrates and selectively checks consequential/disputed claims. It can do independent work while a worker runs and waits when its next useful step depends on that worker.
- Do not silently discard an oversized report's qualifications. Request a focused replacement or retain an addressable full report and disclose unreviewed scope.

## Remaining implementation

1. Replace blanket preference/trust wording with a short routing description. Add child-only exploration guidance through the existing configuration path without replacing unrelated model instructions.
2. Intentionally apply the existing read-only profile and disable dedicated write tools and recursive collaboration at construction and dispatch. A role name or omitted schema alone is not enforcement.
3. Withhold unclassified side-effecting MCP/dynamic tools. `ToolHandler::is_mutating` defaults to false, so it cannot be treated as proof that every unmarked tool is read-only. Use one explicit capability boundary.
4. Preserve supported model selection, provider, overlays and applicable project instructions. Record actual resolved configuration in tests; avoid a speculative instruction-elision or full-history-fork feature.
5. Pilot bounded acquisition: paths, then symbols/callers, then relevant evidence. Bound aggregate returned bytes; `rg -m` is a per-file limit. Read a complete small file when cheaper than repeated slices and use native streams for machine inputs.

The existing `grep_files` and `read_file` tools are possible reuse points, not prerequisites. Before wider exposure, address filenames-only pagination/completeness, unbounded subprocess capture and line-display truncation. Do not edit bundled models merely to expose an unvalidated search path.

## Acceptance evidence

|Boundary|Required result|
|---|---|
|Fresh child|Parent-history-only marker absent; delegated task and applicable instructions present.|
|Capabilities|Dedicated writes, recursive collaboration and unclassified mutating external tools cannot dispatch; ordinary workers remain unchanged.|
|Shell policy|Actual filesystem/network behavior matches the promised role, including necessary runtime temp resources.|
|Model/provider|Captured child request matches resolved model, effort, provider and overlays.|
|Handoff|Parent receives conclusions and locators, without child raw tool traffic; missing/oversized reports are explicit.|
|Cancellation and capacity|Timeout is not completion; interrupt/close joins owned work and releases the slot.|
|Behavior|Known-answer coverage is preserved and parent context, aggregate usage and elapsed time are reported separately.|

Use scripted requests for schema/isolation/lifecycle assertions and a disposable environment for shell-policy probes. Run the [evaluation](evaluation_plan.md) before changing defaults. Concurrency auto-tuning, instruction elision and a new agent RPC schema remain outside this plan.
