# Evaluation for context harness changes

Status: see [the implemented contract](../../features/context-harness.md#validation) for deterministic storage, process, sandbox, recorder and request evidence. The controlled pilots have not run. Scripted request counts and recovered bytes do not measure model quality, provider usage, cost or cache behavior.

## Controlled comparisons

Hold the task/fixture revision, model/provider/effort, effective instructions, tool schemas, compaction/work-note behavior, output budgets, deadlines and worker limits constant within each comparison, except for its named treatment. Record the resolved configuration and captured request shape rather than relying on a feature label. Keep transcripts, source records, run manifests, model consultations and per-run measurements outside persistent project docs.

Use the following independent experiments before combining improvements:

|Experiment|Treatment|Keep fixed|Question|
|---|---|---|---|
|Acquisition guidance|Baseline versus lightweight guidance using the same available tools|Tool exposure, preview size, artifacts, delegation and waiting|Does guidance improve coverage per returned byte without excessive tiny reads?|
|Structured tool exposure|Existing exposure versus corrected search/read handlers|Guidance, model and the other harness features|Does the tool interface improve actual acquisition behavior?|
|Recoverable output|Artifact retention off/on|Identical model preview size, task and waiting behavior|Does addressable evidence reduce reruns and recover missing information?|
|Preview sizing|Existing versus candidate preview policy|Artifact retention on and all other settings|How does inline size trade against recovery calls and quality?|
|Explorer specialization|Existing versus specialized explorer|Resolved child model/effort, concurrency and task|Do report/capability changes improve isolation without losing evidence?|
|Waiting|Short empty waits versus long bounded waits versus completion-driven continuation|Worker task/model, handoff, output budgets and deadline|How many inference requests occur only because of waiting?|

For the combined acquisition/delegation comparison, first select and freeze an acquisition implementation. Do not fold long waits, a different worker model, new artifacts or an alternate endpoint into the acquisition label.

|Condition|Acquisition policy|Fresh exploration|
|---|---|---|
|A|Baseline|Off|
|B|Candidate|Off|
|C|Baseline|On|
|D|Candidate|On|

Keep existing remote compaction and notes constant. Disabling a remote-compaction flag does not isolate note retention. Do not compare a full-history child against a fresh child while attributing the difference solely to delegation.

Start with three independent agent runs per condition across at least three task shapes; these are a pilot, not a universal sufficient sample. Randomize condition order and pair conditions on the same task revisions. Repeated judge calls are not repeated agent runs. Increase sampling only when variance prevents the concrete decision; keep inconclusive outcomes explicit. Separate cold/warm cache conditions where controllable and label unknown cache state. Infrastructure-invalid reruns require a recorded reason and retention of the failed attempt; a genuine task deadline or incorrect answer is an outcome, not an exclusion.

## Deterministic counterexamples

|Fixture|Required observation|
|---|---|
|Directed lookup|Find a known symbol/caller; measure unnecessary delegation and slicing overhead.|
|Cross-module pipeline|Recover required edges, feature gates and decoys; score missed dependencies and unsupported links.|
|Large output and lagged consumer|Recover a seeded middle marker beyond preview/buffer limits even when a UI subscriber lags.|
|Stateful one-shot command|An invocation counter proves that retrieval reads original retained bytes without rerunning the command.|
|Failure and capture independence|Nonzero exit with complete EOF capture, zero exit with quota loss, cancellation with/without drained EOF, and pre-spawn failure have truthful separate states.|
|Storage pressure|Queue saturation, slow disk, partial writes, disk full and finalization failure keep memory bounded and expose exact retained-prefix limitations.|
|Search and long lines|A late match beyond result limits and a critical condition beyond the display-line cap cannot be reported as exhaustive absence or a complete line.|
|Changed evidence and handoff conflict|A file edit or false version premise produces qualified findings and targeted revalidation.|
|Compaction/restart/fork|Actual recorder and replay preserve ordered history and retrieve a known artifact reference or explicit unavailability.|
|Long wait with heavy output|No unbounded accumulator growth; exit/cancellation returns without waiting out the requested maximum.|
|Completion delivery races|Polling, notification admission, pause, superseding input, rollback and review/compaction ownership do not create duplicate logical outcomes or stale-owner inference.|
|Crash around consumption|Crash before/after admission or model-send leaves replayable ownership; do not claim exactly-once remote inference.|
|Interactive and headless hosts|Explicit waiting keeps the correct owner alive; ordinary completed or paused tasks stay closed to automatic wake-up.|

Use the actual formatter, tool dispatch, sandbox and recorder/replay paths for the corresponding contract. An in-memory helper fixture cannot establish filesystem reachability or durable restart behavior. Test the budget boundary where mandatory locator/status metadata barely fits and where it cannot fit. Assert complete call/result pairs and stable earlier response bytes across later reads, polls and compaction replacement.

## Measurements

|Measure|Interpretation and limits|
|---|---|
|Produced, reader-observed, committed and rendered bytes|Ground-truth producers can count emitted bytes in fixtures. Production code may know only reader-observed bytes; expose unknown remainders instead of inventing totals.|
|Parent and per-child peak context|Compaction pressure within each conversation; summing separate peaks is not one context window.|
|Cumulative input and provider usage|Include uncached input, cache writes/reads where reported, output/reasoning, children, extraction/report helpers, notes, compaction, retries and failures. Avoid double-counting cached input when the provider includes it in total input.|
|Cost|Use a dated price table and separately report host/worker/helper totals. API-equivalent dollars do not directly measure remaining subscription quota.|
|Waiting|Count inference requests whose purpose is waiting/status inspection, actual tool wait durations, completion-to-continuation latency and runtime-only waits. Runtime polling is not an LLM call.|
|Acquisition and recovery|Repeated reads, complete small-file reads, result omissions, artifact range reads, reruns, stale locators and unavailable artifacts.|
|Quality|Known-answer coverage, correctness, unsupported claims, valid/fresh locators, counterevidence and explicit unresolved scope. Confidence and model agreement are not correctness evidence.|
|Resource use and time|Memory/disk bounds, worker lifetimes, elapsed and critical-path time, interruption latency and task deadlines.|

Logical input-token accounting does not prove bytes retransmitted or full prefill recomputed. Record transport/cache observations separately; do not assume a universal cache TTL. If opaque compaction quality matters to a scenario, measure answer retention after actual compaction instead of inferring it from the item's format.

## Decisions and execution

Declare task-specific quality/resource budgets before interpreting the results; do not invent a universal percentage improvement or compaction-rate trigger. False completeness, lost critical evidence, stale-owner activation and recovery failures are correctness failures even if token counts improve. Select a default only when the observed benefit is repeatable within the declared quality and resource requirements; retain negative and uncertain results.

Use small scoped checks through the [development workflow](../../development.md). Loopback tests use existing capability guards, and platform-specific sandbox/resource tests require a capable disposable environment. Record skipped cases distinctly. Do not add a test-only flush after stop to hide recorder durability gaps, or remove task timeouts from published outcomes.

Move verified runtime contracts into feature docs after implementation. Keep live evaluation claims tied to the actual runs and their effective versions; repository plans retain decisions and unresolved work rather than a growing experiment log.
