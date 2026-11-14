# Evaluation for context acquisition changes

Status: planned. Static source inspection and scripted transport fixtures do not establish live model quality, token savings or cost improvements.

## Controlled comparisons

Use the same task, source/fixture revision, model/provider/effort, tool schemas and effective compaction settings within each comparison. Record instruction hashes, preview/capture/artifact limits, cache conditions, timeouts and maximum workers. Keep raw transcripts and run-specific manifests outside persistent project docs.

|Condition|Bounded acquisition guidance|Independent exploration|
|---|---|---|
|A|Baseline|Off|
|B|Candidate|Off|
|C|Baseline|On|
|D|Candidate|On|

Keep existing notes/compaction behavior constant. Evaluate artifact storage off/on with the same preview size before testing smaller previews. Compare existing and specialized explorers with the same resolved worker model/effort. Do not use a remote-compaction flag as though it disabled only work-note retention.

Start with three independent repeats across at least three relevant task shapes, randomize condition order, and separate cold from warm cache runs. Increase repeats when variance prevents a conclusion.

## Fixtures

|Task|Known-answer requirement|
|---|---|
|Directed lookup|Known symbol/caller; measure unnecessary orchestration overhead.|
|Cross-module pipeline|Required edges, decoys and feature gates; score missed dependencies.|
|Large output|Long lines and a crucial marker beyond preview/capture caps.|
|Stateful recovery|One-time marker and invocation counter; retrieval cannot rerun the command.|
|Compaction/restart|Actual recorder and replay preserve required conclusions and retrieve a known artifact address.|
|Changed evidence|Fixture edit between reads requires version-qualified findings.|

## Measurements and decisions

- Separate produced, reader-observed, retained and model-rendered bytes/tokens. Report preview, artifact and history truncation separately.
- Measure parent and per-worker context peaks, plus aggregate provider usage including report generation, notes, compaction, retries and failed attempts. Do not sum independent peaks into one context-window claim.
- Record cache usage, repeated reads, command reruns, retrieval ranges, compactions, elapsed/critical-path time, memory, disk use and worker lifetimes.
- Score required-edge coverage, correctness, unsupported claims, valid locators and unresolved scope using the same rubric. Self-reported confidence is not correctness evidence.
- Reject false complete-artifact claims, lost critical scope and recovery failures even when parent token use falls. Promote default changes only after repeatable gains within declared quality/resource budgets; retain negative and inconclusive outcomes.

## Execution and completion

Use small deterministic fixtures through the [development workflow](../../development.md). Loopback tests use the existing capability guards. Platform-policy/resource-pressure tests need an isolated capable environment; record skips distinctly. Do not add a test-only flush after stop to hide recorder durability gaps.

Keep proposed runtime changes in plans until implemented. Move durable contracts into feature docs after implementation; keep evaluation claims limited to the runs actually performed and leave untested behavior explicit.
