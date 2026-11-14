# Development

## Local build commands

Use `cargo-local` for Cargo work. The checked-in [shim](../scripts/cargo-local) delegates to `CARGO_LOCAL_BIN`, or to `$HOME/.local/share/codex-rust-wrappers/cargo-local`, and sets `CARGO_LOCAL_WORKSPACE_DIR`. It exits if that executable is absent; the shared wrapper is an environment prerequisite, not vendored code. Install/configure the wrapper before running commands. Use `just-local` when an available recipe calls Cargo directly.

The shared wrapper owns target-directory selection, test-output filtering and platform sandboxing. Keep separate targets for independent worktrees. Confirm its selected target with `cargo-local --print-target-dir`; do not assume artifacts are in `target/`. Keep cache configuration and output captures outside the repository.

|Purpose|Command|
|---|---|
|Type-check the CLI|`cargo-local check -p codex-cli --bin codex`|
|Small debug binary|`cargo-local build -p codex-cli --bin codex --profile dev-small`|
|Release-like local iteration|`cargo-local build -p codex-cli --bin codex --profile release-fast`|
|Production artifact|`cargo-local build -p codex-cli --bin codex --release`|

The [workspace profiles](../Cargo.toml) keep production FatLTO and a single codegen unit. `release-fast` uses ThinLTO and more codegen units to reduce local iteration cost; it does not replace production packaging. Native-async tool/task implementations use narrow object-safe adapters at their storage boundaries; preserve that separation when changing dispatch.

The hidden `responses-api-proxy` and `stdio-to-uds` commands dispatch to separately built sidecars. [CLI dispatch](../cli/src/main.rs) searches beside the executable and on `PATH`; packaging must provide the sidecars when those commands are supported. Further dependency-closure work is in the [build plan](dev_plans/cargo_build_optimization/README.md).

## Scoped validation

Set `CODEX_SANDBOX_NETWORK_DISABLED=1` for tests. Start with the affected package and relevant test names:

```bash
CODEX_SANDBOX_NETWORK_DISABLED=1 cargo-local test -p codex-core --lib <filter>
CODEX_SANDBOX_NETWORK_DISABLED=1 cargo-local test -p codex-core --test all <filter>
```

- Keep test exclusions in [the sandbox skip list](../scripts/cargo-local-network-sandbox-skips.txt). On a configured macOS host, the shared wrapper supplies localhost-only access and handles nested-sandbox exclusions. `CODEX_SANDBOX_NETWORK_DISABLED` alone is not an OS sandbox on arbitrary hosts.
- Use `skip_if_no_network_unless_localhost!` from [core test support](../core/tests/common/lib.rs) for localhost-only HTTP/WebSocket fixtures. Do not forge the wrapper's capability flag or report early-return tests as executed transport coverage.
- Prefer `-- --show-output` for diagnostics. Shared-wrapper raw output mode can bypass its exclusions; use it only for a deliberately scoped compatible test.
- Persistence and lifecycle changes need real recorder flush/error paths and replay after pause, interrupt and shutdown. A test-side flush after stopping can conceal a production durability bug.
- Review TUI snapshot changes before accepting them. Regenerate affected config/protocol schemas with their repository generator when their source types change. The [config generator](../core/src/bin/config_schema.rs) and [app-server generation instructions](../app-server/README.md) define those outputs.
- Documentation-only work needs relative-link/anchor checks, source-backed contract checks and a diff review for unwrapped prose and compact tables. It does not require a Rust rebuild.

## Bazel changes

Cargo manifests and the lockfile define dependencies. Keep relevant `BUILD.bazel` targets aligned when adding a crate, executable or runtime fixture; consult the owning target and enclosing Bazel workspace definitions for the actual build and lockfile procedure. Do not assume editing `Cargo.lock` alone completes Bazel integration. Nested sandbox tests may need dedicated target handling; preserve their existing sandbox assumptions.

## Reporting

Report which behavior was checked, the relevant result and anything that prevented the check. Fixture success does not establish hosted-model behavior, and historical pass counts do not establish the current checkout's result. Keep raw logs, machine paths and benchmark captures out of project docs.
