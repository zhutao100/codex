# Native compaction acceptance

Optional deployed-endpoint check using [the native acceptance script](../../../scripts/validate-remote-compaction.py), a rebuilt `codex` executable and valid ChatGPT access credentials. It runs synthetic conversations through the native app-server client.

## Requirements and invocation

The current script requires macOS's `/private/tmp` and Python 3.11+ (`tomllib` and `datetime.UTC`). Use a disposable macOS environment when evaluating a native build; a standard Linux environment without that path cannot run the script unchanged.

```bash
CODEX_SANDBOX_NETWORK_DISABLED=1 python3 scripts/validate-remote-compaction.py "$(cargo-local --print-target-dir)/debug/codex" --model "$ACCEPTANCE_MODEL"
```

|Option|Purpose|
|---|---|
|`--generation-only`|Check ordinary generation before attributing connection failures to compaction.|
|`--auth-source PATH`|Read a prepared credential file; otherwise use the user's auth file.|
|`--model MODEL`|Use an explicit available model; otherwise read the source configuration.|
|`--timeout SECONDS`|Set the per-turn deadline; default 120 seconds.|
|`--websocket`|Enable Responses WebSockets v2 for an additional transport check.|

The script creates temporary state/workspaces, copies authentication, clears the copied refresh token and leaves source credentials untouched. It uses the built-in OpenAI provider and disables shell execution, undo, remote-model refresh, auto-rename and completion review. It emits JSON milestones and bounded failure diagnostics.

The generated config currently contains `base_instructions`, which is not a supported top-level TOML override. This runner therefore does not establish a custom base-prompt configuration; use `model_instructions_file` when changing that behavior.

## Assertions and interpretation

1. Ordinary native generation completes.
2. Manual compaction completes once and follow-up recalls a synthetic codeword.
3. A real tool round with inert reference text crosses an automatic-compaction threshold; compaction and codeword follow-up complete.
4. Replacement history excludes triggers and preserves emitted notes exactly by concatenating assistant output-text parts without separators.
5. A new app-server process resumes the persisted thread and recalls the codeword.

These checks use the deployed endpoint, but do not measure broad summary quality or a model/provider matrix. HTTP success does not establish WebSocket behavior; run that mode explicitly when relevant. If generation fails before compaction, compare ordinary transport before changing compaction code.

For a VM run, copy the executable, script, model ID and access-only credential input into a disposable workspace, and keep shared inputs immutable for its duration. Use explicit guest paths. Keep credentials out of logs and remove temporary credential copies afterward. Record the model, transport, executable identity, milestones and exit status with the run output rather than growing this feature document with per-run results.
