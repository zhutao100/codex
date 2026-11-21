# Context harness

Use these contracts when changing how evidence enters the model context or how the runtime waits for local work. Existing [history and replay](history-replay.md), [work notes](session-work-notes.md), and [pause and continue](pause-continue.md) contracts still apply.

## Wait without another request

An empty `write_stdin` accepts a 5–300 second wait. Initial `exec_command` and nonempty `write_stdin` accept 250 milliseconds to 30 seconds. An exit can end an empty wait early; output activity alone does not return the call. Pending preview bytes and per-call collection are separately bounded to 1 MiB head/tail buffers. Their known byte omissions are not a count of all bytes emitted by the process.

The [handler](../../core/src/tools/handlers/unified_exec.rs) and [manager](../../core/src/unified_exec/process_manager.rs) implement this policy for both interactive and headless runs. A long empty wait is still a single ordinary tool call.

`[features] exec_job_completion = true` exposes `wait_for_completion` on `exec_command`. Use it for noninteractive pipe jobs whose results the originating turn needs. An initial result for a still-running registered job carries a stable job ID; `write_stdin` must not poll it. Jobs that finish during the initial call return ordinary exit metadata without parking the turn.

The [job ledger](../../core/src/unified_exec/jobs.rs) records ownership before launch. After the tool batch, the turn flushes its paired acknowledgments, then parks the existing owner until all acknowledged jobs reach a terminal state or a five-minute deadline. Terminal status and any artifact receipt are recorded before contextual delivery. Host warnings and outcomes stay outside user-turn boundaries. Delivered outcomes pass through the normal context-budget and compaction decision before another model request. A deadline notice is not a terminal outcome; use `exec_job_status` with the job ID to inspect late outcomes without rerunning or waking a turn.

A paused parent must explicitly continue. Recovery reconciles acknowledged call/result pairs in the latest surviving user request rather than relying on the ID of the last continuation attempt. An over-budget owner compacts before its next ordinary request, including after `/continue`. No execution-end event can reopen an idle or superseded owner, and queued input or a stop takes precedence while waiting. Restarting a session alone never activates a job or assumes its process survived.

The ledger holds at most 64 registrations per session; each tool batch may register at most four. Spawn failures and jobs that exit before acknowledgment release their provisional slots. The ordinary exit watcher finalizes registered jobs and retires only the matching live process; the durable job ID remains available for status. The live waiting path also discards registrations whose acknowledgment did not survive in history. Abrupt host exit before acknowledgment can leave a provisional entry occupying a slot. Rollout flush acknowledgment is not power-loss durability or exactly-once remote inference.

The flushed call/result pair identifies the waiting owner; there is no second waiting checkpoint. A delivered outcome is contextual but belongs to the preceding request. Rolling back the next user request removes its setup, not that outcome. A rollback attempted while a turn is parked is still an active-turn rollback and is rejected.

## Retain command output

`[features] recoverable_exec_output = true` enables experimental local retention for `exec_command` (pipe or terminal) and the shell tool. Readers tap bytes before preview fanout or prefix truncation. A receipt separates command outcome from capture status; failed commands can have complete captures, and successful commands can have incomplete ones. Use `read_exec_output` with the receipt's `artifact`, `stream`, `offset`, and `limit` to read the committed range as a JSON string or base64. Terminal bytes are what the PTY reader observed. Separate stdout and stderr ranges do not imply an inter-stream ordering.

The [store](../../core/src/exec_output.rs) keeps a contiguous prefix per stream, with an 8 MiB shared cap per command attempt, 64 allocated artifacts per session, and a 4 MiB session-wide budget for queued and in-flight output bytes. The first quota, queue or writer loss seals retention across streams; readers can still drain. A receipt acknowledges only the data prefix already written and published in the catalog. Unknown or interrupted drainage is not EOF. Publication supports restart on the same filesystem, not power-loss durability.

Artifacts are stored under `$CODEX_HOME/exec-outputs/`, outside the rollout. Ephemeral sessions do not create artifacts or register jobs. A known locator can be reopened on resume in its source session; a fork cannot read those bytes through `read_exec_output`. There is no automatic catalog discovery, expiry, or eviction. Remove an unneeded session's artifacts only when it is closed; future reads report missing data. Concurrent writers for the same session, export/relocation and legacy Windows restricted-token capture are not supported. Enabling retention also retains raw bytes that would never have appeared in the prompt.

Preview sizing reserves room for command status before history insertion. A running command keeps its usable session ID or job acknowledgment. Legacy shell JSON carries the receipt in `metadata.output_artifact`; a text projection renders it with the exit status. Drop an optional receipt rather than shorten a locator. Reject a budget that cannot reserve the status envelope before launching or writing stdin. A failed spawn does not publish a receipt or retain an empty artifact. Cancellation after allocation can return a locator without claiming that drainage or publication finished. Compaction may remove the original locator from visible context without deleting retained bytes. Record consequential locators in [work notes](session-work-notes.md) if they are needed later. A range read does not edit earlier output.

## Acquire evidence directly

Use `read_file` for a path and range you already know. Its numbered displays mark shortened or lossy lines and report source byte ranges for exact recovery with `byte_offset` and a byte `limit`. Indentation scans and returned text have independent bounds. Revision receipts identify ordinary edits; they do not make a multi-file read atomic. A changed revision requires revalidating consequential evidence.

Use `grep_files` for candidate discovery, not to infer absence outside ripgrep's scope. It streams NUL-delimited paths, reports budget cutoffs, errors and timeouts as partial, and emits one JSON-encoded path per line. An invalid UTF-8 path is encoded as `pathBytesBase64`. Partial results have no stable offset cursor: narrow the scope instead of treating a second search as continuation of a snapshot.

Both tools remain opt-in for ordinary turns through `model_overlay.experimental_supported_tools`. This keeps the effect of exposure separate from a change to acquisition guidance or preview size.

## Delegate independent evidence gathering

Collaboration is opt-in. Assign `explorer` a narrow question, scope and revision, constraints, premises to check, and expected locators. The fresh child retains applicable model/provider settings and project instructions and gets child-specific guidance. Its registry and dispatch permit only `grep_files` and `read_file`; shell, recursive delegation, patching, MCP, and dynamic tools are not available. The search child uses the turn's platform sandbox transform and environment policy, without an unconfined retry. If the platform cannot stream under the requested sandbox, search fails before launch. Read-only does not imply a restricted set of readable paths.

An explorer has a 12-call evidence budget with up to 8 KiB per result and a two-minute sampling deadline. The last 20 seconds are reserved for report-only sampling. An internal pre-compaction work-note request is not a report. If its sampling cutoff expires, use the usual compaction fallback rather than returning work notes as a handoff. The timeout cancels and drains outstanding tool calls; it does not discard saved calls or guarantee another model request will fit. The report includes admitted calls, observed partial results and up to four read revisions, not a claim of exhaustive coverage. An oversized report remains in the child history; request a focused replacement.

## Retry a failed response

A completed streamed tool-call item is recorded before dispatch. If the response disconnects before completion, record a paired not-executed result for each undispatched call. Retrying preserves the request prefix and includes the recorded pairs; it does not launch the rejected batch or rely on the failed WebSocket connection.

## Validation

- [Captured requests and recorder replay](../../core/tests/suite/context_harness.rs): a 32-second job used one empty poll and three scripted requests. A one-shot middle marker survived restart and local compaction; a fork could not retrieve the source bytes, and a deleted artifact reported missing data. Additional fixtures exercise failed spawns with retention, live handles under tight budgets, release of a registered slot before turn completion, and interruption, steering, review and rollback ownership.
- [Storage fault injection](../../core/src/exec_output.rs): a real file accepts a short write, then the writer returns a storage-full error. Reopening returns only the published prefix; this is not a host disk-exhaustion test. [Shell serialization](../../core/tests/suite/shell_serialization.rs) verifies the model-facing JSON and text paths.
- [Headless crash and resume](../../exec/tests/suite/background_completion.rs): kill the actual `codex-exec` process after a job outcome is published but before admission, and after the follow-up POST but before its answer. Resume alone makes no request; explicit continuation sees one outcome, no rerun, and no invented exit status for another unfinished job. Its previously acknowledged bytes reopen with interrupted, not complete, capture status.
- [Search launch probe](../../core/src/tools/handlers/grep_files.rs): run the ignored `explorer_search_confines_the_child_process` in a disposable macOS guest, not inside an enclosing Seatbelt. The control reads a file, writes a marker and connects to a guest-local listener; the explorer can read but cannot write or connect. Do not count a dispatch rejection as this check.

These fixtures establish runtime behavior, not model quality, cache savings or cost. See the [evaluation plan](../dev_plans/context_harness/README.md).
