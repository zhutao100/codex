# Native deployment acceptance

## Entry point

[Acceptance script](../../../scripts/validate-remote-compaction.py), Python 3.13+, a rebuilt `codex` executable, and valid ChatGPT access credentials.

```bash
CODEX_SANDBOX_NETWORK_DISABLED=1 python3 scripts/validate-remote-compaction.py \
  "$(cargo-local --print-target-dir)/debug/codex"
```

|Option|Purpose|
|---|---|
|`--generation-only`|Compare ordinary transport with a pre-backport executable.|
|`--auth-source PATH`|Read credentials from a prepared private input; defaults to the user's auth file.|
|`--model MODEL`|Choose the deployed model; otherwise read the source config.|
|`--timeout SECONDS`|Bound each turn; default 120 seconds.|
|`--websocket`|Opt into Responses WebSockets v2 for an additional transport check.|

The script uses the built-in OpenAI provider and default request compression. It creates temporary state/workspace directories, copies credentials, clears the copied refresh token and leaves the source authentication untouched. Shell execution, undo, remote-model refresh, auto-rename and completion review are disabled. Successful output is a compact list of JSON milestones; failures include bounded diagnostics and return nonzero.

## Assertions

1. Ordinary generation completes.
2. Manual compaction completes exactly once; follow-up recalls a synthetic codeword.
3. A real tool round with 8,000 inert reference lines crosses a 30,000-token automatic threshold.
4. Automatic compaction completes and its follow-up recalls a second codeword.
5. Replacement history excludes triggers and preserves emitted work notes byte-for-byte.
6. A new app-server process resumes the persisted thread and recalls the codeword.

An 8 MiB subprocess stdout limit accommodates large user-message notifications. Shutdown drains the app-server pipes. No real conversation history is used.

## Disposable macOS validation

Use the `ghostvm-safe-testing` workflow with a prepared automation snapshot. Mount a private directory containing the executable, script, model ID and an access-only auth copy read-only; mount exported logs writable. Copy inputs into the guest workspace and invoke the script with absolute guest paths and explicit `--model`/`--auth-source`.

Keep shared inputs immutable during a run. Use new filenames when adding a revised executable or script to a kept-running VM to avoid stale shared-file reads. Remove guest credential copies, stop the VM and restore its configuration/defaults before finishing.

## Recorded evidence

Verified on 2026-09-07 with `gpt-6-astra` in macOS 15.6.1 (24G90):

|Executable/scenario|Result|
|---|---|
|Pre-backport `1c74f68455`, ordinary native generation|Passed in 2.39 seconds.|
|Final rebuilt client, ordinary native generation|Passed in 2.71 seconds.|
|Manual compaction and follow-up|Passed.|
|Automatic compaction and follow-up|Passed with one compaction.|
|Restart/resume and exact notes|Passed with one preserved notes item.|

All client traffic used ordinary guest networking directly to the deployed Responses endpoint. No HTTP/TLS forwarder or production transport modification was required. The authenticated run used native HTTP; WebSocket lifecycle has separate loopback fixture coverage.

The Host API completion acknowledgement stalled after guest execution had finished. Exported exit codes were zero and all assertion milestones were present; guest processes had exited. VM shutdown and configuration/default restoration were verified independently.

On the development host, both baseline and backport ordinary requests timed out before compaction. Independent standard-library TCP and Tokio/reqwest probes also timed out, while stock curl succeeded. Those same binaries connected successfully in the clean VM. The remaining host connectivity problem is independent of compaction; its host-specific cause was not identified.
