# Bounded Exploration

## Status

Proposed, using the existing opt-in `collab` backend. This is not a request to enable collaboration globally or port newer multi-agent APIs.

## Confirmed Gap

Snapshot C's explorer has a supported model override and Medium effort, but no dedicated exploration prompt and no forced read-only policy. Its description encourages using explorers for all codebase questions and trusting results without verification. Fresh thread creation avoids parent-history copying, but inherited instructions/tool definitions still consume context.

The feature defaults off, unlike the newer branch described in the article. The optimization opportunity is therefore an explicit, bounded explorer contract once collaboration is available, not removal of a nonexistent explicit-request prohibition from this branch.

## Intended Behavior

- Direct lookup remains appropriate for a known path/symbol or a small question with an obvious bounded answer.
- Delegate broad, independent questions when their disposable evidence is much larger than the report the parent needs.
- Give each worker a distinct scope, initial revision/CWD, question, relevant constraints and expected evidence. No recursive delegation by exploratory workers.
- Workers return findings, `file:line` plus symbol anchors, inspected scope, uncertainty and unresolved questions. They do not return raw search logs or entire files.
- The parent owns integration and selective checks of disputed or consequential claims. It does not repeat delegated exploration or accept unsupported conclusions merely because a worker sounds authoritative.
- While workers run, the parent does bounded independent work; when useful work depends on their answers, it waits. Universal blocking and universal busywork are both rejected.

## Minimal Runtime Changes

### Role specialization

1. Replace the explorer's blanket preference/trust wording with a short routing description. Keep long procedural guidance in the child-only prompt, not every parent tool description.
2. Add a concise explorer-specific instruction suffix through the existing child configuration path. Preserve applicable user/developer instructions, effective provider, CWD and model overlay. Do not replace the entire model base prompt with a search-only prompt.
3. Set the explorer's existing `AgentProfile::read_only` behavior intentionally and test it; do not infer read-only behavior from the role name.
4. Remove dedicated edit/write tools and recursive collaboration from the effective explorer registry. Enforce the same restriction at dispatch so an omitted spec is not the only barrier.
5. Withhold unclassified side-effecting MCP/dynamic tools from this local-research role. Any allowed external tool needs an explicit read-only capability decision; a local filesystem policy cannot prevent remote state changes.
6. Verify shell commands under the effective filesystem/network policy in the VM. Retain runtime-owned temporary resources required by normal execution, without granting arbitrary model-command writes.

Use existing registry/permission metadata only where it is sufficiently expressive. In particular, `ToolHandler::is_mutating` defaults to false and cannot classify every unmarked tool as read-only. Add a small role-specific filter at one construction/dispatch boundary rather than duplicating per-handler policy. Document which operations remain possible; "read-only" must match observed behavior.

### Models and inherited context

- Preserve catalog support and existing explicit configuration. Do not replace the explorer model with a hardcoded supposedly cheap model or force an unrequested effort change based on the article.
- Record actual resolved model/effort/provider and initial prompt size in evaluations. The parent's model label is not the worker's bill of materials.
- Keep inherited project requirements initially. Unlike M's deliberate CLAUDE.md omission, C does not have a proven safe split between editing-only instructions and research obligations.
- Only consider instruction omission after introducing an explicit, auditable separation that retains applicable boundaries and task context. Do not silently summarize away required instructions to save tokens.

### Existing API, not imagined parameters

In C the initial operation is conceptually:

```json
{
  "agent_type": "explorer",
  "message": "Answer this bounded question at the supplied revision/CWD. Scope: ... Return claims, evidence, coverage and uncertainties. Do not edit or delegate."
}
```

It creates a new child thread already. Do not send `fork_context`, `fork_turns`, `model` or `reasoning_effort` fields that this schema does not expose. The wait operation is `wait`; reusing a worker is appropriate only if its retained state still belongs to the current scope/revision.

Treat worker report size as an explicit prompt target, with room for essential evidence. A hard truncation that silently removes qualifications is not a valid substitute for a concise report. If a report overflows, retain an addressable complete report or request a focused replacement and mark what the parent has not reviewed.

## Acquisition Pilot

Start with corrected navigation instructions, not a new search subsystem:

1. Discover candidate paths, then symbols/callers, then bounded evidence.
2. Use `rg -m` only as a per-file control; inspect wrapper completeness metadata and bound total returned bytes.
3. Read a whole small relevant file when cheaper than many overlapping slices. Use AST ranges for large functions and retain caller-side gates in the trace.
4. Keep raw indexes/logs outside active model output and return compact locators. Use native, unannotated streams for machine inputs.
5. Preserve omitted command output through the separate [artifact plan](recoverable_tool_output.md) when available.

C's `grep_files`/`read_file` handlers are potential reuse points, not an automatic rollout recommendation. Before exposing them more broadly:

- Add explicit continuation/completeness for filenames and matching-content modes. The current filenames-only result cap has no cursor.
- Avoid unbounded `Command::output()` capture merely to return a small result.
- Keep exact file retrieval distinct from the 500-character line display view.
- Decide exposure through supported model/configuration paths and regenerate schemas if configuration changes. Do not edit bundled model metadata just to force unvalidated tool availability.

Promote tool changes only when the instruction pilot identifies a reproducible contract gap. A dedicated structured reader is not a prerequisite for testing the role specialization.

## Patch and Test Plan

|Stage|Scope|Acceptance|
|---|---|---|
|Routing/prompt|`core/src/agent/role.rs`, child-only template and spawn configuration|Known lookup stays viable; broad tasks have explicit worker ownership; original applicable instructions remain.|
|Role capability|Role config, tool registry and dispatch|Explorer cannot invoke dedicated writes, nested agents or unclassified mutating MCP tools; general workers remain unchanged.|
|Isolation|`AgentControl::spawn_agent`, collab handler and captured requests|A marker only in parent history is absent from a fresh child's request, while inherited constraints and the delegated task are present.|
|Handoff|Existing completed status and `wait` results|Parent input contains worker conclusions, not child raw tool traffic; missing/oversized reports are not treated as complete evidence.|
|Cancellation|Existing task ownership and shutdown paths|Timeout does not imply completion; interrupt/close releases capacity and does not abandon running task handles.|
|Model/provider|Resolved child request capture|Catalog-supported model and actual provider/effort match policy without changing unrelated task delegates.|
|Behavioral pilot|Repeated live tasks in the evaluation matrix|Quality/coverage gates pass; parent and aggregate cost changes are reported separately.|

Use scripted mock requests for isolation, capability and lifecycle contracts. They cannot prove that a model chooses sensible delegation. Use a disposable VM for shell-policy checks or broad parallel runs, retain repository sandbox exclusions, and run scoped `codex-core` checks through the prescribed wrappers.

## Deferred Decisions

- Default collaboration enablement and any proactive global policy.
- A user-configurable report token limit, concurrency auto-tuning or a new agent RPC schema.
- Per-role instruction elision, semantic file caching or proactive replay of all worker transcripts.
- Porting the newer backend's full-history fork defaults, model catalogs or optional history service.

The end state is a small, inspectable explorer specialization on the existing backend, supported by evidence that it reduces parent noise without hiding missed scope or inflating total work unnoticed.
