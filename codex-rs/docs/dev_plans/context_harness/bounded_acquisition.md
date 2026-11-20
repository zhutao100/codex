# Plan: bounded evidence acquisition

Status: proposed corrections and selective exposure of existing search/read handlers. Acquisition applies to the parent and children independently; it does not require delegation, a different compactor, or an external extraction model.

## Current behavior and reuse points

The [search handler](../../../core/src/tools/handlers/grep_files.rs) already uses filename-only ripgrep output. It collects the subprocess output before limiting returned paths, and a capped result does not establish exhaustive coverage. Its modification-time ordering is not a stable pagination contract. The [read handler](../../../core/src/tools/handlers/read_file.rs) already supports ranges and indentation-aware blocks, but it shortens long displayed lines without a continuation marker and uses line readers whose buffers can grow with a single large line.

[Tool registration](../../../core/src/tools/spec.rs) and model metadata determine exposure. Handler existence does not imply that the current model can call it. Correct the boundaries below before making catalog changes merely to advertise these tools.

## Acquisition policy

Use a known path or symbol directly when available. For unfamiliar scope, discover candidate paths, inspect the relevant definitions/callers, and then expand only the unresolved evidence. This is lightweight guidance rather than a mandatory command sequence. A complete modest file can be cheaper and clearer than many small slices; preserve that option.

Bound aggregate results and collected bytes, not only matches per file. `rg -m` limits matches within each file and is insufficient as a repository-wide budget. Machine-readable data should use parsers or streams rather than repeatedly asking the model to reconstruct truncated displays.

## Required search and read behavior

|Boundary|Required behavior|
|---|---|
|Search collection|Bound memory while draining or terminating the search process according to an explicit resource/deadline policy. A post-collection result limit is insufficient.|
|Search completeness|Distinguish complete results, a known lower bound, and unknown remainder. Result/byte limits, timeout and errors cannot become “no matches” or exhaustive absence.|
|Continuation|The first version can request a narrower scope or return an addressable retained result. Do not promise an offset cursor over unstable modification-time ordering.|
|Path representation|Handle separators, unusual filenames and decoding explicitly; do not let newline splitting manufacture a different path or silently discard candidates.|
|Range reads|Report the source range actually returned and whether lines or bytes were omitted. Bound a single-line read before formatting, including indentation-aware processing.|
|Long lines|Keep the original evidence recoverable and expose omission/continuation information. A shortened condition must not look like a complete source line.|
|Display versus source|Line numbers, tab expansion and UTF-8 replacement are display transformations. Use source bytes or a separate exact read when machine processing or a patch depends on them.|
|Changed evidence|Attach a revision or content identity sufficient to detect relevant changes; unresolved identity is explicit. A location alone is not a freshness guarantee.|

An omitted-result notice must survive the result's final history budget. Reuse the metadata-preserving sizing rule in [recoverable tool output](recoverable_tool_output.md). The output artifact store can preserve results already captured, but it cannot retroactively repair bytes dropped upstream of its insertion point.

## Recovery without repeated broad reads

Return a small useful result first, with its coverage boundary and a concrete next operation when more evidence is needed. Recovery must obtain omitted evidence from a retained artifact or an exact source range; it must not depend on rerunning a stateful source command. Repository searches can be repeated against a named revision when appropriate, but that is a new observation and must not be presented as the original result.

For a partial source line, prefer an exact bounded byte range or a parser that extracts the relevant field. For too many file matches, narrow the path/symbol scope or inspect a retained listing. Do not turn every truncation into an automatic request for the entire file.

Claude's `src/tools/GrepTool/GrepTool.ts` offers a useful filename-discovery default, and `src/tools/FileReadTool/FileReadTool.ts` offers range requests and unchanged-read detection. The inspected mirror's advertised default line count differs from its omitted-limit implementation, so tests must exercise the actual dispatch path. Unchanged-read suppression is deferred: an unchanged filesystem version does not prove the earlier content remains available after compaction. Any later cache must account for context replacement, not just file modification time.

## Implementation and acceptance

1. Make bounded collection, long-line behavior and completeness visible in the existing handlers without changing unrelated tool interfaces. Choose output fields/wording when implementing; this plan does not introduce a new public schema.
2. Exercise the actual exposed tools with filename caps, byte caps, late matches, errors, timeouts, large lines, unusual filenames and an edit between reads. Verify effective sandbox access through dispatch.
3. Enable a controlled tool-exposure trial only after those cases are truthful. Compare guidance and tool exposure separately with the same model, instructions, preview budget and task set.
4. Apply successful acquisition behavior to both parent and child contexts. Use the [evaluation plan](evaluation_plan.md) to measure answer coverage, repeated reads, total usage and context peaks; smaller output alone is not a quality result.

Semantic indexes, automatic page-extraction models, adaptive window tuning and a new generic retrieval RPC are deferred until the basic interfaces show a measured deficiency that requires them.
