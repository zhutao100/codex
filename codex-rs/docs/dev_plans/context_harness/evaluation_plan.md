# Evaluation Plan

## Status and Goal

Proposed. No private article trajectory was reproduced and no live model comparison was run in this research task.

The target is reliable context acquisition and recovery with acceptable total resource use. Parent-context reduction alone cannot establish correctness, coverage, lower cost or better continuation.

## Reproducible Manifest

Record these inputs once per run, without embedding user-specific paths or credentials:

- Source revision, build options, prompt/instruction hashes, task and fixture revision, random seed where supported.
- Actual parent and every worker's resolved model, provider, effort, context limits, metadata source, tool schemas, collaboration backend and relevant feature/configuration overrides.
- Compaction implementation, raw/effective windows and actual auto limit; note capture availability and whether a capture actually completed.
- Worker history inheritance and effective initial instructions, including whether project text was omitted.
- Output preview/capture/artifact limits; wrapper versions, caps and omission metadata for instruction-based experiments.
- Task completion limits, maximum workers, timeout, cache condition, concurrency and execution environment.
- Price schedule and observation date when estimating money; report usage counts separately and do not equate list-price estimates with subscription quota debits.

Keep transcript/artifact evidence in controlled evaluation storage, not in project documentation. Reports should contain sanitized summaries, hashes, counters and selected source-located claims.

## Experiment Matrix

Hold parent/worker model selection, effort, task and harness version constant within each comparison.

|Run family|Bounded acquisition policy|Independent exploration|
|---|---|---|
|A|Baseline policy|Off|
|B|Corrected policy|Off|
|C|Baseline policy|On|
|D|Corrected policy|On|

Keep C's existing notes/compaction behavior constant across A-D. Instruction text is part of the treatment and its token overhead must be recorded.

Then add separate ablations:

1. Artifact storage off/on with the same preview size, so retrieval benefits are not confounded with reducing visible output.
2. Smaller preview only after complete-output retrieval is verified.
3. Existing explorer versus specialized explorer, with the same actual worker model/effort.
4. Optional notes present/absent only through an explicit evaluation-only implementation variant that keeps compaction dispatch constant. Do not pretend `remote_compaction=false` disables notes alone.
5. Fresh worker versus bounded inherited history only on backends that expose both; do not inject unsupported fork parameters into C.

Start with at least three task shapes and three independent repeats per condition; report individual outcomes and dispersion, not just a best run. Increase repetitions if variance prevents a decision. Randomize condition order and separate cold-start from warm-cache runs.

## Tasks

|Task|Known ground truth and failure signal|
|---|---|
|Directed lookup|One path/symbol with a short caller chain; needless worker orchestration is measurable overhead.|
|Cross-module pipeline|Seeded ingestion, transformation, storage and serving chain with decoy implementations and feature gates; score every required edge.|
|Large search/read|Many candidate files, a long line, ignored/hidden paths and a crucial middle match beyond ordinary display/capture limits.|
|Stateful output recovery|A command emits a one-time marker and increments a counter; retrieval must recover the marker without rerunning.|
|Continuation after compaction|Record a validated fact, a ruled-out hypothesis and a known artifact address; perform actual compaction and continue against the real recorder/replay path.|
|Evidence changed on disk|Edit the fixture between reads while preserving the task; require version-qualified findings rather than stale-cache reuse.|

Use small generated repositories for deterministic integration. Add representative real repositories only with a separately reviewable answer rubric and fixed revision. Never use the article's unavailable private financial repository as an assumed reproducible fixture.

## Measurements

Separate counters that are often conflated:

```text
command-produced bytes
  -> bytes observed by the harness
  -> bytes retained in preview/artifact
  -> bytes/tokens rendered into each request
  -> provider-reported input, output and cache usage
```

- Parent and per-worker active-context peaks, with accounting method and sampling point. Do not sum peaks and call the result one larger context window.
- Aggregate provider usage across parent and all workers, including report generation, notes, compaction, retries and failed attempts.
- Cached read/write usage where available. Compare byte-stable prior results and reusable prefixes, not a binary claim that any editing invalidates every cached token.
- Command/tool calls, repeated identical reads, reruns attributable to missing output, truncation layers, omitted bytes, artifact retrievals and actual retrieved ranges.
- Parent/worker compaction count and implementation, notes completion/failure, restart success and recovery attempts.
- Wall time and critical-path time, worker lifetimes, peak memory, disk bytes and concurrency. Idle parent inference savings do not erase worker cost.
- Answer correctness, required-edge coverage, unsupported claims, evidence locator validity and explicit unresolved scope.

Score all answers against the same rubric, preferably without exposing treatment labels to the reviewer. A self-reported confidence score is not correctness evidence.

## Gates

### Deterministic contracts

- Every promised-complete artifact returns exact seeded bytes; quota/fault cases are explicitly incomplete or unavailable.
- Recovery after actual compaction/restart does not rerun the one-time command.
- Fresh children exclude parent-history-only markers while retaining required inherited instructions.
- Explorer capability checks cover tool construction and dispatch, including dynamic/MCP paths and real VM shell behavior.
- Completed/stopped events do not precede required installation or output finalization boundaries.
- Previously recorded tool results remain byte-stable across later sampling; unchanged small outputs retain baseline behavior.
- A fixture skipped for lack of transport or VM capability is reported skipped, never counted as coverage.

### Behavioral decisions

Predeclare the required claim set, unacceptable errors and task-level resource budgets before looking at results. Block rollout for missing critical pipeline edges, invented evidence or recovery failures even if the parent uses fewer tokens.

For passing answers, report paired parent-context, aggregate usage, latency and rerun changes with the run distribution. A default change requires a repeatable gain on the intended task shapes without unacceptable directed-lookup overhead. A cost/latency tradeoff is acceptable only when explicitly recorded; do not label a more expensive run globally "more efficient."

## Execution Boundaries

- Local deterministic fixtures stay small, bounded and noninteractive. All Cargo tests use `CODEX_SANDBOX_NETWORK_DISABLED=1 cargo-local ...`; loopback fixtures use the repository's localhost-aware guards.
- Run relevant `codex-core`, changed utility and consumer tests first. Workspace-wide all-features testing requires separate authorization.
- Resource-pressure tests, shell-policy probes and broad/live parallel evaluations run in a disposable macOS VM, not on the developer's active host.
- Preserve repository test exclusions and real stop/shutdown paths. Do not forge capability flags or insert a test-only flush after stop to hide durability gaps.
- Record scoped command, executed/skipped counts, outcome and sanitized artifact hashes in the eventual feature validation document.

## Completion

The implementation is complete only when the deterministic contracts and declared behavioral gates are met, limitations are documented, and the relevant plans are converted into feature/validation documentation. If the live evaluation is not run, retain these as proposed plans and state that limitation rather than claiming the article's gains were reproduced.
