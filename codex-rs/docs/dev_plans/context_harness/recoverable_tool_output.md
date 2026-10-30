# Recoverable Tool Output

## Status

Proposed. Primary target: snapshot C in the [source audit](source_audit.md). No artifact store or runtime setting is implemented by these documents.

## Problem

Unified execution can lose the middle of output at the 1 MiB capture buffer before its approximately 10,000-token tool preview is produced. Legacy shell capture has a different bounded-prefix path. Neither supplies a durable retrieval address for all omitted bytes. A rollout containing the truncated result cannot reconstruct those bytes; rerunning a stateful or expensive command is not an equivalent recovery mechanism.

Work notes preserve conclusions, not arbitrary raw evidence. Optional upstream history retrieval and programmatic output filtering are useful reference designs, but do not close this capture-time gap.

## Contract

1. Capture an output artifact before the first destructive capture limit, not by writing the already-truncated tool response.
2. Keep existing in-memory caps and small-result formatting. A complete artifact is an additional bounded retention surface, not permission to increase prompt output.
3. Give omitted output a model-readable retrieval address, completeness state, retained/observed byte counts and a bounded preview. A file path is sufficient initially if the existing permitted file/shell tools can actually read it.
4. Preserve that address in the recorded tool result. Completed artifact contents and recorded previews are immutable; normal history projection must not silently change an earlier result.
5. Cover success, nonzero exit, timeout, cancellation and spawn failure explicitly. Never emit "full output saved" for a partial or unavailable artifact.
6. Retrieval must not reexecute the command or require a healthy original process handle.

## Minimal Design

### Writer

- Add one small session-owned output-artifact writer shared by unified execution and legacy shell capture. Integrate at their actual byte-read boundaries: `utils/pty/src/pipe.rs`, `utils/pty/src/pty.rs` and platform counterparts for unified execution, and `core/src/exec.rs` for legacy shell.
- Tap before PTY/pipe broadcast fanout. The channels have finite capacity, and `UnifiedExecProcess::new` continues on receiver lag; a downstream copy cannot truthfully promise all reader-observed bytes.
- Tee the observed byte stream into the bounded artifact writer and existing preview buffers. Do not replace the command with shell redirection, alter TTY mode, or change stdout/stderr semantics.
- Preserve a PTY's merged stream as merged; non-PTY execution may store separate streams with explicit identity. Do not claim an exact cross-stream ordering that the operating system did not provide.
- Bound disk bytes per artifact, aggregate session bytes, queued writer memory and shutdown time. Calibrate concrete limits using the evaluation fixtures before choosing defaults; infinite retention is not an acceptable fallback.
- Continue draining subprocess pipes after storage fills or fails. If a bounded writer queue cannot keep up, mark the artifact incomplete rather than stalling the child indefinitely or growing memory.
- Use an internal typed outcome such as complete, still-writing, size-limited, write-failed, or unavailable. Record the original exit/cancellation result independently of artifact I/O.

### Storage and access

- Use a runtime-derived private per-session directory and opaque call-owned names. Do not let command text supply storage paths or reuse a mutable "last output" file.
- Select the location only after testing reads under the branch's actual sandbox policies. A path the harness can write but the model cannot read is not a retrieval contract. Prefer existing session/temp ownership over a new global service.
- Publish completed status only after the writer has closed successfully. An interrupted prefix can remain addressable but must be labeled incomplete.
- Reopen completed artifacts after resume/fork according to explicit source-session ownership. Do not accidentally create a new empty file at a persisted address.
- Never rewrite or delete a completed artifact merely because compaction removed its original tool result. Apply a documented retention/quota policy; expired addresses must yield a clear unavailable result.
- Initial retrieval uses existing bounded file/shell access. Add a dedicated retrieval tool only if sandbox readability or client transport requires it, and give it explicit byte/range limits rather than an unbounded "read all" mode.

### Model result

Add a compact text footer only when output is omitted and an artifact outcome is known. This avoids changing tool-call schemas in the first patch.

```text
Output preview omitted content.
Saved output: <runtime-selected-address>
Artifact state: complete
Retained bytes: <n>; observed bytes: <n>
Read/search a bounded range of the saved output instead of rerunning the command.
```

The footer must survive both tool rendering and `ContextManager::process_item`. Reserve its small budget instead of appending it to text that the next truncation layer can remove. Include terminal state at exit; earlier polling results may describe a still-writing artifact and remain historical facts.

Report size-limited or write-failed states just as explicitly, retaining the existing preview if artifact storage is unavailable. Distinguish raw bytes from UTF-8 display text, estimated tokens, and bytes already discarded by the subprocess's own pipeline.

### History and cache

- Decide offloading before the result first reaches normal sampling history. Do not retroactively replace arbitrary historical outputs as part of this feature.
- Keep the compact address/status record in ordinary tool output; keep raw artifact bytes out of rollout response items and request bodies.
- Compaction can summarize away the original address. Preserve important addresses in existing work notes when needed; the mandatory recovery fixture supplies a known address after compaction to prove retrieval, without assuming automatic rediscovery.
- Reuse current replacement epochs and rollout replay. No new compaction control item, work-note sentinel, response item variant or history envelope is justified for initial output storage.

## Patch Sequence

1. Add the bounded writer and pure result-formatting helpers with injected storage faults.
2. Integrate unified execution, including `write_stdin` polling and process completion; keep runtime exposure opt-in until checks pass.
3. Integrate legacy shell with the same outcome/formatting contract; do not copy a second storage implementation.
4. Exercise persistence, compaction, restart, cancellation and cleanup together. Document retention limits and unavailable-artifact behavior.
5. Measure real acquisition/recovery behavior before considering a smaller default preview. Store-on-omission is useful even without changing the preview budget.

If rollout/config changes become necessary, explain why the existing text result is insufficient. Any `ConfigToml` addition requires schema regeneration; no such change is assumed in this plan.

## Verification

|Case|Required evidence|
|---|---|
|Small successful command|Same result text, exit semantics and prompt input as baseline; no unnecessary model-facing footer.|
|Output above model limit but below capture cap|Exact middle marker retrieved from the artifact without rerunning.|
|Output above the 1 MiB capture cap|Exact marker deliberately located outside retained head/tail recovered from the pre-cap artifact.|
|Legacy shell prefix cap|Tail marker recovered, proving capture was not merely copied from the bounded prefix.|
|PTY and split streams|Preserved observed bytes, explicit stream identity, unchanged interactive behavior.|
|Lagged UI/preview consumer|Artifact retains reader-observed bytes despite broadcast lag; no downstream buffer is mistaken for the original stream.|
|Nonzero exit, timeout and interruption|Original failure remains visible; retained evidence is addressable and completeness is truthful.|
|Disk full, permission failure and slow writer|Bounded memory/time, no pipe deadlock, useful preview and explicit incomplete/unavailable status.|
|Artifact byte/session quota|No fabricated complete status, no silent destruction of an already-promised file.|
|UTF-8 split, binary bytes and very long lines|No corruption in stored bytes; display/retrieval limits and decoding are explicit.|
|Compaction and restart|Mocked full turn retrieves a known address after real compaction and reconstructed rollout, not a handcrafted snapshot alone.|
|Repeated request/cache prefix|Earlier result bytes unchanged; no new full-output replay into the model prompt.|
|Expired output|Clear unavailable result; no silent command rerun or source-session mix-up.|

Run scoped `codex-core` and affected PTY utility tests through `cargo-local`, with explicit network-disabled settings. Use loopback-aware guards for scripted Responses fixtures. Host tests stay small and concurrency-bounded; slow-writer/resource-pressure measurements belong in a disposable VM.

The end-to-end baseline and candidate must run the same seeded command output, retrieve the same hidden marker, and record produced/stored/rendered bytes, command executions, latency, peak memory and disk use. Passing scripted recovery establishes a transport/storage contract, not improved live reasoning; see the [evaluation plan](evaluation_plan.md).

## Non-Goals

- Lossless retention after the subprocess itself discarded output, or after a documented quota/retention boundary.
- Storing every output indefinitely, suppressing all compaction, or importing Claude's exact thresholds.
- Automatic broad retrieval of old transcripts, aggressive rewriting of cached history, or a new general file-search API.
