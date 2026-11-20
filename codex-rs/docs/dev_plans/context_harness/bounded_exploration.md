# Plan: bounded exploration

Status: proposed specialization of the existing opt-in `collab` backend. It does not enable collaboration by default.

## Confirmed behavior and gap

[Agent roles](../../../core/src/agent/role.rs) give Explorer a model/effort override but no dedicated exploration instructions and no forced read-only policy. Its tool description broadly encourages trust and delegation. [AgentControl](../../../core/src/agent/control.rs) already creates fresh child threads rather than copying parent history. Applicable inherited configuration/instructions still consume child context.

The exposed [tool schema](../../../core/src/tools/spec.rs) accepts `spawn_agent` with `message` and optional `agent_type`; communication uses `send_input`, `wait` and `close_agent`. Do not add unsupported `fork_turns`, `fork_context`, model or effort arguments in examples. The [collaboration handler](../../../core/src/tools/handlers/collab.rs) owns the lifecycle and depth checks.

## Intended behavior

- Use direct lookup for a known path or narrow symbol question. Delegate independent research when its raw evidence is much larger than the report the parent needs.
- Assign explicit scope, revision/CWD, question, applicable constraints and expected evidence. Distinguish established facts from assumptions the child should check. Preserve task constraints without copying parent-only orchestration instructions into the child's assignment.
- The parent integrates and selectively checks consequential/disputed claims. It can do independent work while a worker runs and waits when its next useful step depends on that worker.
- Do not silently discard an oversized report's qualifications. Request a focused replacement or retain an addressable full report and disclose unreviewed scope.

## Handoff and report contract

Use the current `message` field for a concise task brief; a new delegation RPC or mandatory JSON report is unnecessary for the initial specialization. The brief separates objective, allowed scope, constraints, premises to check, and expected output. Pass the remaining task deadline or budget when one exists. Do not forward a host instruction to delegate the task again or require the child to rediscover already established facts without a reason.

The report distinguishes findings with source locators, interpretation, coverage gaps, and conflicts with the supplied premises. Give evidence for consequential conclusions, including counterevidence; avoid a citation ritual for every descriptive sentence. A worker may preserve the assigned goal while reporting that a version number, API contract or other concrete premise is false. An uncertainty marker is useful context, not a substitute for an actual source.

Return a bounded useful report on deadline or budget exhaustion, with explicit partial status and unexamined scope. Reserve space for conflicts and missing coverage before trimming optional detail. Harness-owned storage may retain an oversized full report through the [artifact contract](recoverable_tool_output.md); the read-only worker does not need arbitrary write access to create it. If a replacement summary is requested, count that extra inference and preserve access to the original report rather than silently dropping its qualifications.

Use host-observed tool outcomes for a small coverage footer: completed/partial/failed status, relevant omission or timeout markers, and bounded source identities. These observations qualify what was retrieved; they do not prove that the worker's interpretation is correct or that every dependency was searched. Avoid dumping its full access ledger into the parent context. A hard deadline, cancellation or API failure may prevent a final synthesis call; return the deterministic partial metadata and any already completed report instead of promising that another model request always fits.

Record the relevant source revision or content identity with locators, and qualify findings when the working tree changes during exploration. Hashes detect mismatched evidence; they do not establish that a claim is correct or make a multi-file reading session an atomic snapshot. Recheck only the consequential changed evidence instead of repeating every worker read.

## Remaining implementation

1. Replace blanket preference/trust wording with a short routing description. Add child-only exploration guidance through the existing configuration path without replacing unrelated model instructions.
2. Intentionally apply the existing read-only profile and disable dedicated write tools and recursive collaboration at construction and dispatch. A role name or omitted schema alone is not enforcement.
3. Withhold unclassified side-effecting MCP/dynamic tools. `ToolHandler::is_mutating` defaults to false, so it cannot be treated as proof that every unmarked tool is read-only. Use one explicit capability boundary.
4. Preserve supported model selection, provider, overlays and applicable project instructions. Record actual resolved configuration in tests; avoid a speculative instruction-elision or full-history-fork feature.
5. Apply [bounded acquisition](bounded_acquisition.md) to children as well as the parent. Bound aggregate returned bytes, preserve honest coverage, and read a complete modest file when that is cheaper than repeated slices. Acquisition improvements remain independently useful without delegation.
6. Resolve task deadlines and child budgets at the harness boundary. Reserve an opportunity to produce a partial report rather than exhausting all available work on another search. Distinguish a wait timeout from worker completion, and keep existing stop/close ownership explicit rather than assuming every parent pause automatically cancels every child.

An enforced read-only profile covers dispatched tools and the effective sandbox. Hiding `Edit`/`Write` alone does not establish that Bash or a dynamic tool cannot mutate state. Preserve necessary runtime-managed temporary resources without granting the worker a general write escape. Do not introduce automatic child cancellation or cross-session lifetime changes without checking the existing lifecycle contract.

Fresh context means a separate history, not zero instructions. Preserve applicable project instructions, provider settings and overlays; do not copy another product's instruction-elision default. A full-history fork, if added separately in the future, would still isolate later child output but would have different initial-context and cost behavior.

Claude's `src/tools/AgentTool/built-in/exploreAgent.ts` is useful for narrow role guidance, while its denylist alone does not establish an OS read-only boundary. Newer Codex's guidance to avoid overlapping delegated work is preferable to requiring a parent to do nothing. Neither reference establishes a universally optimal worker model, effort, report size or child count.

## Acceptance evidence

|Boundary|Required result|
|---|---|
|Fresh child|Parent-history-only marker absent; delegated task and applicable instructions present.|
|Capabilities|Dedicated writes, recursive collaboration and unclassified mutating external tools cannot dispatch; ordinary workers remain unchanged.|
|Shell policy|Actual filesystem/network behavior matches the promised role, including necessary runtime temp resources.|
|Model/provider|Captured child request matches resolved model, effort, provider and overlays.|
|Handoff|Parent receives conclusions and locators, without child raw tool traffic; missing/oversized reports are explicit.|
|False premise and changed evidence|Worker reports the conflict; parent does not accept a stale locator as current proof.|
|Deadline and partial report|A bounded useful report preserves critical findings and missing scope; deadline failure remains in end-to-end results.|
|Cancellation and capacity|Timeout is not completion; interrupt/close joins owned work and releases the slot.|
|Behavior|Known-answer coverage is preserved and parent context, aggregate usage and elapsed time are reported separately.|

Use scripted requests for schema/isolation/lifecycle assertions and a disposable environment for shell-policy probes. Run the [evaluation](evaluation_plan.md) before changing defaults. Concurrency auto-tuning, instruction elision and a new agent RPC schema remain outside this plan.
