# Align stabilized and shared implementations

Status: proposed work under [feature alignment](README.md). A stable feature can remain disabled, and an enabled registry default can still be constrained by auth, provider, model, platform, managed requirements or tool registration. Resolve those conditions before changing defaults.

## Features that became stable

|Feature|Implementation comparison|Decision for this branch|Acceptance before changing the default|
|---|---|---|---|
|`shell_snapshot`|Current starts an asynchronous session snapshot and has a small export/lifetime model. Reference adds policy-filtered capture, environment-aware lazy initialization, credential handling and an optional in-memory executor implementation.|Adapt applicable capture/filtering/lifetime fixes to the current shell owner. Promote to Stable/default-on only after those supported-shell checks; do not make executor-memory V2 a prerequisite.|Bash/Zsh environment, PATH, aliases/functions and command overrides; timeout/failure fallback; filtered variables; concurrent sessions; cleanup; custom exec receipts/jobs.|
|`memory_tool` → `memories`|Current provides a SQLite-backed `get_memory` lookup. Reference has startup extraction, asynchronous consolidation and file-backed memory tooling. The reference Stable feature remains default-off.|Keep opt-in. Preserve the minimal tool unless the broader memory system is deliberately selected as its own project. A key rename/alias must describe the semantics it actually provides.|Current lookup and unavailable-storage behavior; root/ephemeral/child startup; cancellation and job leases if extraction is adopted; bounded injection and placement; no substitution for exact work notes.|
|`apps`|Reference is Stable/default-on but effective availability requires appropriate ChatGPT authentication; discovery, deferred tools and plugin ownership have expanded.|Keep the present product scope and default until there is a reason to enable connectors broadly. Adopt relevant discovery/account-change correctness independently of hosted product surfaces.|API-key and unsupported-auth paths expose no unusable tools; account changes refresh caches; tool names/order and approval dispatch match the advertised catalog.|

History anchors: shell snapshot stabilization [753821c90fda](https://github.com/zhutao100/codex/commit/753821c90fda514ec43ee01b1932ba9626c4138f), memory rename [e8709bc11a00](https://github.com/zhutao100/codex/commit/e8709bc11a00f9a5ad7976c011dad724d9b6b712) and stabilization [091b2ada945e](https://github.com/zhutao100/codex/commit/091b2ada945e4e74c4d2d70701d297bc7116ff40), apps stabilization/default history [0bff38c54a89](https://github.com/zhutao100/codex/commit/0bff38c54a89f4658cfe79466e8a17a469f9e201) and [4b50446ffa33](https://github.com/zhutao100/codex/commit/4b50446ffa3385f8b72a5ced71a1306d28646d02). The temporary apps default reversal is retained in the accompanying history data; the final state, not one intermediate commit, drives this plan.

Compare current [`shell_snapshot.rs`](../../../core/src/shell_snapshot.rs) with [reference capture](https://github.com/zhutao100/codex/blob/33411a01d7dae5fe89728ecfa2ab8b1c09002329/codex-rs/core/src/shell_snapshot.rs#L156), current [`get_memory`](../../../core/src/tools/handlers/get_memory.rs) with [memory startup](https://github.com/zhutao100/codex/blob/33411a01d7dae5fe89728ecfa2ab8b1c09002329/codex-rs/memories/write/src/start.rs#L25), and current [`connectors.rs`](../../../core/src/connectors.rs) with the [auth-qualified reference](https://github.com/zhutao100/codex/blob/33411a01d7dae5fe89728ecfa2ab8b1c09002329/codex-rs/features/src/lib.rs#L700). These are distinct owner changes, not a single registry edit.

## Unified exec on Windows

`unified_exec` is already Stable and enabled by default on non-Windows builds here. The reference changed the Windows default to true in [d8d7ca73f80e](https://github.com/zhutao100/codex/commit/d8d7ca73f80e992cade867970ceb264431cea33c). Keep the existing Unix behavior. Before changing the Windows default, compare PTY/ConPTY availability, non-TTY execution, unsupported-ConPTY fallback, cancellation, large output and terminal lifetime. Exercise the custom retained-output and job-completion contracts on the supported Windows path.

The reference additionally normalizes unified exec on unless a managed requirement pins it off. Do not import that normalization while this branch still supports intentional legacy shell selection. Retiring old tool families requires a separate caller/alias/model-capability migration.

## Shared metadata with different implementations

### Shell-tool capability precedence

Current [`ToolsConfig::new`](../../../core/src/tools/spec.rs) can select UnifiedExec when `shell_tool` is enabled even if the model's shell type is Disabled; it reads the model shell type only after the UnifiedExec branch. The [reference](https://github.com/zhutao100/codex/blob/33411a01d7dae5fe89728ecfa2ab8b1c09002329/codex-rs/core/src/tools/spec_plan.rs#L1225) honors both the global switch and the explicit model-disable value before registering command tools.

Plan a focused precedence correction: explicit Disabled suppresses shell registration and dispatch even with UnifiedExec enabled. Preserve supported legacy aliases, Windows fallback, restricted explorer/reviewer policies and custom output/job fields. Add a configuration/model matrix rather than assuming this affects a currently selected model; source inspection identified the difference but did not exhaustively audit all model catalogs or reproduce a live failure.

Useful reference fixtures include `disabling_shell_tools_disables_command_tools_for_all_environments`, `shell_family_registers_only_unified_exec_tools` and `reviewer_tool_policy_respect_managed_shell_restrictions` in [spec-plan tests](https://github.com/zhutao100/codex/blob/33411a01d7dae5fe89728ecfa2ab8b1c09002329/codex-rs/core/src/tools/spec_plan_tests.rs). Adapt their assertions to the current single-environment owner without importing unrelated executor infrastructure.

### Skill/MCP dependency admission

The flag is Stable/default-on in both branches, but the current [`skill_dependencies.rs`](../../../core/src/mcp/skill_dependencies.rs) computes missing servers from turn configuration and writes the global map without the reference's candidate-admission step. The [reference installer](https://github.com/zhutao100/codex/blob/33411a01d7dae5fe89728ecfa2ab8b1c09002329/codex-rs/core/src/mcp_skill_dependencies.rs#L40) stages candidates through constrained configuration and the runtime policy-aware catalog before prompting, repeats admission after the wait, then installs only the admitted candidates.

Adapt that admission/revalidation at the current configuration/MCP owner. Keep canonical dependency deduplication, cancellation, first-party-originator checks, repository-scoped server merging and explicit environment injection. Resolve `Never` behavior coherently: under `Never`, the reference auto-installs only when policy permits; otherwise it skips installation. It never opens the installation prompt. Its originator check is client identity, not ChatGPT user authentication.

Acceptance: a server rejected before the prompt is neither offered nor persisted; policy changes during a prompt are respected; runtime-installed dependencies are recognized; refresh preserves repository/managed constraints; constrained `Never` does not wait for an impossible interaction. Existing reference `mcp_dependency_refresh_preserves_current_server_constraints` and current canonical-key tests provide starting points. Larger OAuth/multi-environment catalog machinery is not required for this focused change.

### Sampled-output replay

`output_token_replay` has matching UnderDevelopment/default-off metadata and is already explicitly backported. Keep its current default and inspect behavior through [`client.rs`](../../../core/src/client.rs), protocol items, history normalization, remote compaction and the [replay contract](../../features/responses-output-replay.md).

Preserve the distinction between requesting new sampled-output ciphertext and replaying ciphertext already in history. Turning the flag off must not erase saved data. Non-OpenAI provider projection must filter only unsupported outgoing sampled-output data without mutating stored history or conflating it with reasoning ciphertext. Retain fresh-socket full replay after rejected continuation, hosted-search/tool history pairing, item metadata and the custom stable review prefix. Matching metadata alone establishes none of these properties.

Use `output_token_replay_respects_provider_and_preserves_history` and `missing_previous_response_replays_full_hosted_search_history` plus review/WebSocket fixtures in the [validation map](validation.md). Any future claim about cache hits or model quality requires a live measurement; request fixtures establish shape and ordering only.

### Metrics and compression

|Flag|Meaning retained here|Reference difference|Disposition|
|---|---|---|---|
|`runtime_metrics`|Manual reader and WebSocket timing header; a metrics exporter is separately required.|Reference distinguishes explicit OTLP exporters from built-in analytics suppression and has broader telemetry routing.|Keep default-off. Treat exporter routing as optional observability work, preserving timing-header/reader semantics.|
|`enable_request_compression`|Zstd for eligible OpenAI/Codex-backend traffic, currently selected through ChatGPT auth.|Reference tests `uses_codex_backend`, which includes newer auth modes.|Keep currently supported behavior; expand only with the corresponding auth/provider implementation and endpoint tests. Do not compress arbitrary providers or confuse HTTP body compression with WebSocket extensions.|

Both branches have request-compression fixtures that check `Content-Encoding` and decoded request JSON for eligible backend traffic versus API-key traffic. The current runtime metrics and WebSocket timing tests should remain when touching these shared owners. No feature alignment is required solely because the surrounding telemetry/auth implementation became larger.

The two unchanged deprecated web-search entries are covered in [cleanup](cleanup.md#mixed-and-still-active-deprecations). Their unchanged metadata does not make their loader conversions dead.

## Default policy

Keep custom controls, memory, broader architecture and product surfaces at their current opt-in state during these changes. Change a default only in the same scoped patch that establishes its implementation, supported activation and failure behavior. Record platform differences explicitly; a source `cfg!` evaluation is not a successful test on that platform.
