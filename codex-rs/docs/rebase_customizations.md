# Customization maintenance

## Upstream replacements

|Customization|Rebase decision|Upstream evidence|
|---|---|---|
|Model-switch base-instruction rewrite|Retired. Keep the request instruction prefix and add target-model/personality instructions incrementally to developer context. Preserve resumed/forked instruction provenance and atomic settings publication.|`a17da5e6e4`, `df72fdb415`, `68301fa45f`|
|Fallback model parallel-tool flag and shell-command mode|Use upstream's global parallel-tool support and unified exec. Retain custom unknown-model instructions, reasoning defaults, freeform patches, and context/compaction limits.|`86b1123ff6`, `bce5f2fcfc`|
|Orphan-output normalization|Use upstream's indexed cleanup, including standalone named outputs. Retain linear missing-output insertion and cached token estimates.|`80ceab7aaa`, `763787d061`|
|World-state baseline reset after rollback|Use upstream's envelope replacement path, which clears the baseline. Retain the custom truncation regression test.|`99915080b6`, `1c1e17782a`|
|Local Cargo recipes|Drop the retired `mcp-server-run` recipe. Keep wrapper delegation for the current schema, logs, and code-mode host targets.|`531f3836a1`|
|Fallback personality templates|Retire the slug-specific Friendly/Pragmatic templates and render the custom unknown-model prompt through `codex-prompts`. Keep fallback reasoning and context limits.|`132c739171`, `a8c36ca6d2`, `d63a9b8344`|
|Guardian image-tool dispatch|Use the upstream reviewer `ToolPolicy`, including managed-sandbox and stable-environment gates. Keep `view_image` registered but hidden for text-only models.|`ebc05da3bd`, `550eb50545`, `335c7f8eca`|
|Post-send event allocation reduction|Keep completion routing in the agent controller and preserve realtime reasoning-status delivery. Capture only realtime callback data; retain original events only for terminal callbacks and preserve post-delivery ordering.|`7a15548c6a`, `90e653014a`|
|Review rollout assertions|Paginated delegates persist structured completed items instead of legacy user/reasoning events. Keep transient forwarding and legacy duplicate suppression; verify transcript isolation for both legacy and paginated parents.|`12cb14f7b7`|
|App-server schema recipe|Keep the upstream Python driver, which also generates Python SDK types. Invoke it through `just-local` so its Cargo subprocess uses the shared wrapper.|`45134c0463`|
|Status and background-terminal snapshots|Keep borderless status rendering and upstream fixture additions/removals. Retain fallback-model limits and full multiline `/ps` wrapping rather than restoring old bordered snapshots or abbreviated previews.|`819cdb726d`, `e75b36efde`, `e57fc9ea5a`|
|Deferred execution preview|Keep upstream's compact first-lines preview and platform-specific transcript shortcut. Retain bounded stored output and interruption/out-of-order command materialization.|`321c50fc2c`, `d2f2c40095`|
|Local formatting Cargo wrapper|Use upstream's Git-indexed, batched `rustfmt` driver. Formatting no longer invokes Cargo, so the formatter-specific wrapper override is retired; build/test wrappers remain.|`95cafae96c`|
|Fallback metadata marker|Use upstream's catalog/runtime metadata origin instead of the removed `used_fallback_model_metadata` field. Retain the custom fallback prompt, reasoning defaults, patch format, and context limits.|`dc40446064`|
|Prompt-history fast path and token tail estimates|Use upstream's request history snapshot, async image resolution, and last usable server-usage boundary. Retire the separate `prompt_history` helper; retain shared envelopes, cached estimates, and normalization outside the session lock.|`7a030b9769`, `cb66dbecf2`|

## Preservation rules

- Skill isolation remains intentional: only add `$HOME/.agents/skills` when the user config directory is inside the resolved home. Port this check when skill ownership moves; do not restore the deleted legacy loader.
- Prefer the branch's tracking remote while retaining `SanitizedGitUrl` credential removal.
- Share history envelopes and token estimates across snapshots; clone prompt payloads after releasing session state. Preserve envelope metadata, stable synthetic output IDs, retained review context, and rollback behavior.
- Count cached tail tokens from the last usable server-usage boundary, including failed responses. Preserve request input-item counts and async attachment resolution; consumed prompt snapshots must not rebuild unused token caches.
- Review delegates retain their own transcripts. Forward client events transiently and keep parent review summary records.
- Accept historical review-event payload shapes without dropping current turn/item IDs. Deserialize history payloads in `codex-history`; read JSONL records through `codex_rollout::parse_rollout_line` so nested decimal values retain their precision.
- Keep upstream feature gates and per-step model selection when hiding `view_image` from text-only models.
- Preserve deferred parallel commands through interruption and keep their streamed output bounded.
- Keep the standalone origin-URL accessor's semantics separate from tracking-remote-aware repository metadata.

## Local verification

- Use `CODEX_SANDBOX_NETWORK_DISABLED=1` and the shared Cargo wrappers for tests.
- Keep package-scoped `just-local test -p <crate>` runs separate from workspace benchmark smoke tests.
- Use Cargo/nextest quiet flags and capture logs; `just --quiet` suppresses failure diagnostics as well as successful output.
- Nextest uses `--cargo-quiet --status-level fail --final-status-level fail`, not `--quiet`. Select insta snapshot operations with `--manifest-path tui/Cargo.toml`.
- Before scoped core/app-server integration runs, build the `codex`, `codex-code-mode-host`, and `test_stdio_server` binaries from their owning packages. Scoped nextest builds do not automatically produce these cross-package fixtures.
- Match both V8 archive and bindings to the locked version and sandbox features; use the checksum-verifying package artifact helper instead of reusing an arbitrary cached archive.
- For deterministic TUI checks, unset `NO_COLOR`, `TERM_PROGRAM`, `TERM_PROGRAM_VERSION`, and `TERM_SESSION_ID`. Use a temporary Git config without global ignore rules so worktree fixtures include their `.codex/config.toml`.
- Limit build jobs and test workers on the developer host. Run tests that can interfere with the host in a disposable VM.
- Core suites using `skip_if_no_network!` return early under the required network-disabled flag. Distinguish those guards from executed localhost regressions and public app-server RPC tests when reporting coverage.
