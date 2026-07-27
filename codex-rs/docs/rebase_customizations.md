# Customization maintenance

## Upstream replacements

|Customization|Rebase decision|Upstream evidence|
|---|---|---|
|Model-switch base-instruction rewrite|Retired. Keep the request instruction prefix and add target-model/personality instructions incrementally to developer context. Preserve resumed/forked instruction provenance and atomic settings publication.|`a17da5e6e4`, `df72fdb415`, `68301fa45f`|
|Fallback model parallel-tool flag and shell-command mode|Use upstream's global parallel-tool support and unified exec. Retain custom unknown-model instructions, reasoning defaults, freeform patches, and context/compaction limits.|`86b1123ff6`, `bce5f2fcfc`|
|Orphan-output normalization|Use upstream's indexed cleanup, including standalone named outputs. Retain linear missing-output insertion and cached token estimates.|`80ceab7aaa`, `763787d061`|
|World-state baseline reset after rollback|Use upstream's envelope replacement path, which clears the baseline. Retain the custom truncation regression test.|`99915080b6`, `1c1e17782a`|
|Local Cargo recipes|Drop the retired `mcp-server-run` recipe. Keep wrapper delegation for the current schema, logs, and code-mode host targets.|`531f3836a1`|

## Preservation rules

- Skill isolation remains intentional: only add `$HOME/.agents/skills` when the user config directory is inside the resolved home. Port this check when skill ownership moves; do not restore the deleted legacy loader.
- Prefer the branch's tracking remote while retaining `SanitizedGitUrl` credential removal.
- Share history envelopes and token estimates across snapshots; clone prompt payloads after releasing session state. Preserve envelope metadata, stable synthetic output IDs, retained review context, and rollback behavior.
- Review delegates retain their own transcripts. Forward client events transiently and keep parent review summary records.
- Accept historical review-event payload shapes without dropping current turn/item IDs. Deserialize history payloads in `codex-history`; read JSONL records through `codex_rollout::parse_rollout_line` so nested decimal values retain their precision.
- Keep upstream feature gates and per-step model selection when hiding `view_image` from text-only models.
- Preserve deferred parallel commands through interruption and keep their streamed output bounded.

## Local verification

- Use `CODEX_SANDBOX_NETWORK_DISABLED=1` and the shared Cargo wrappers for tests.
- Keep package-scoped `just-local test -p <crate>` runs separate from workspace benchmark smoke tests.
- Use Cargo/nextest quiet flags and capture logs; `just --quiet` suppresses failure diagnostics as well as successful output.
- Before scoped core/app-server integration runs, build the `codex`, `codex-code-mode-host`, and `test_stdio_server` binaries from their owning packages. Scoped nextest builds do not automatically produce these cross-package fixtures.
- Match both V8 archive and bindings to the locked version and sandbox features; use the checksum-verifying package artifact helper instead of reusing an arbitrary cached archive.
- For deterministic TUI checks, unset `NO_COLOR`, `TERM_PROGRAM`, `TERM_PROGRAM_VERSION`, and `TERM_SESSION_ID`. Use a temporary Git config without global ignore rules so worktree fixtures include their `.codex/config.toml`.
- Limit build jobs and test workers on the developer host. Run tests that can interfere with the host in a disposable VM.
