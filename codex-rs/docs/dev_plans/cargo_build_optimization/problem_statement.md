# Cargo Build Optimization - Problem Statement

## Goal

Reduce the latency of developer-facing release-like builds for the `codex` executable without weakening the production release profile or destabilizing branch-specific behavior.

The concrete pain point is:

```bash
cargo build --release --bin codex
```

A captured run showed many project crates compiling before the final binary and completed in roughly `7m28s`. The compile list included crates such as `codex-protocol`, `codex-tui`, `codex-cli`, `codex-app-server-protocol`, `codex-api`, `codex-rmcp-client`, `codex-core`, `codex-codexd`, `codex-app-server-test-client`, `codex-lmstudio`, `codex-ollama`, `codex-login`, `codex-common`, `codex-cloud-tasks`, `codex-chatgpt`, `codex-mcp-server`, `codex-exec`, `codex-app-server`, and related utility crates.

## Correct Interpretation of the Build Output

`cargo build --release --bin codex` is not intentionally building every other binary first. From the workspace root, Cargo selects the `codex` binary target from `codex-cli`, then compiles the normal dependency closure needed by that binary.

Several packages look like binaries because their package names are executable-like, but the `Compiling ...` lines are package/crate compilation units. For example, `codex-app-server-test-client`, `codex-lmstudio`, and `codex-ollama` appear because they are reachable through `codex-cli`'s library dependency graph, not because Cargo is necessarily producing their standalone executable artifacts for the selected target.

This matters because the fix is not simply "tell Cargo to build only one binary". Cargo is already building the selected binary target. The remaining cost comes from what that target directly or transitively includes.

## Problem Class 1: Core Compile-Time Hot Spots

The older branch shape had two high-fanout `#[async_trait]` boundaries in `codex-core`:

- `core/src/tools/registry.rs`: tool dispatch through `ToolHandler`;
- `core/src/tasks/mod.rs`: turn workflow execution through `SessionTask`.

The implemented design now matches the low-churn upstream direction:

- concrete `ToolHandler` implementations use native return-position `impl Future`;
- `ToolRegistry` stores `Arc<dyn AnyToolHandler>` only at the object-safe storage boundary;
- concrete `SessionTask` implementations use native return-position `impl Future`;
- active task state stores `Arc<dyn AnySessionTask>` only at the running-task boundary.

This addresses an important compiler hot spot, especially for package-clean `codex-core` rebuilds with dependencies warm. It does not by itself solve the final `codex-cli` release link and codegen cost.

## Problem Class 2: The `codex` Binary Is a Multitool

`cli/src/main.rs` exposes a broad command surface through one binary:

- no-subcommand interactive TUI;
- `exec` and `review`;
- `login` and `logout`;
- `mcp` and `mcp-server`;
- `app-server`, including protocol code generation and codexd management;
- macOS `app` command;
- completion generation;
- sandbox helper commands;
- debug app-server tooling;
- hidden `execpolicy` tooling;
- `apply`;
- cloud task browsing/apply;
- hidden `responses-api-proxy`;
- hidden `stdio-to-uds`;
- feature flag inspection and mutation.

`cli/Cargo.toml` therefore directly depends on many high-level crates, including:

- `codex-tui`;
- `codex-exec`;
- `codex-mcp-server`;
- `codex-app-server`;
- `codex-app-server-protocol`;
- `codex-app-server-test-client`;
- `codex-codexd`;
- `codex-cloud-tasks`;
- `codex-responses-api-proxy`;
- `codex-stdio-to-uds`;
- `codex-chatgpt`;
- `codex-login`;
- `codex-common`;
- `codex-core`.

This creates a large release codegen and link workload even if a local iteration only needs the default TUI or the `exec` path.

## Problem Class 3: Production Release Profile Favors Artifact Quality Over Latency

The workspace root `Cargo.toml` intentionally configures production release builds for size and runtime quality:

```toml
[profile.release]
lto = "fat"
strip = "symbols"
split-debuginfo = "off"
codegen-units = 1
```

`lto = "fat"` plus `codegen-units = 1` is a slow final-codegen/link profile. It is appropriate for shipped artifacts, but it is a poor default for developer smoke testing when the binary graph is large.

This branch already adds:

```toml
[profile.release-fast]
inherits = "release"
lto = "thin"
codegen-units = 16
strip = "none"
split-debuginfo = "off"
```

That profile is the correct near-term developer path, but it should be documented and measured as a first-class build mode rather than treated as an incidental profile.

## Problem Class 4: Cold Tooling Is Linked Into the Primary Front Door

Some `codex` subcommands are operationally cold or internal:

- app-server protocol TypeScript generation;
- app-server JSON schema generation;
- debug app-server send-message tooling;
- hidden responses API proxy;
- hidden stdio-to-UDS relay;
- execpolicy check tooling;
- codexd launch-agent management;
- cloud task browsing/apply if not part of the common local loop.

When those live as direct library calls from `cli/src/main.rs`, they force their dependencies into the primary binary's compile/link closure.

The current architecture optimizes UX simplicity: one `codex` front door. The build problem is that this front door also becomes a compile-time aggregation point.

## Problem Class 5: OSS Provider Helpers Are Pulled Through `codex-common`

`common/Cargo.toml` unconditionally depends on `codex-lmstudio` and `codex-ollama`:

```toml
codex-lmstudio = { workspace = true }
codex-ollama = { workspace = true }
```

`common/src/oss.rs` uses those crates to provide shared OSS provider readiness and default-model utilities. Because `codex-cli` depends on `codex-common` with the `cli` feature, local provider support can enter the final binary dependency closure even when the immediate command path does not use local OSS providers.

This is likely legitimate product behavior, but it is a measurable build-cost seam. It should be isolated only if measurement shows it materially affects release-like build latency or binary size.

## Problem Class 6: Build-Script and Embedded-Asset Invalidation Remains a Follow-Up

`core/Cargo.toml` still declares `build = "build.rs"`, and `core/build.rs` tracks embedded skill sample assets. The existing compile-time design already flags this as lower priority than the async-trait work.

This is not the primary cause of the final `codex` release link cost, but it can cause unnecessary `codex-core` invalidation if the tracked asset tree changes often. It remains a follow-up optimization seam.

## Consolidated Root Causes

|Root cause|Primary evidence|Optimization direction|
|---|---|---|
|`codex-core` compiler hot spots|implemented `AnyToolHandler` and `AnySessionTask` adapters in `core/src/tools/registry.rs` and `core/src/tasks/mod.rs`|keep implemented native-async refactor; measure remaining hot spots|
|Large `codex-cli` dependency closure|direct dependencies in `cli/Cargo.toml`; broad dispatch in `cli/src/main.rs`|split cold/internal subcommands into sidecar binaries or optional crates|
|Slow production release profile|`[profile.release]` uses FatLTO and one codegen unit|use `release-fast` for developer release-like builds; keep production release unchanged|
|OSS helper dependency fanout|`codex-common` unconditionally depends on `codex-lmstudio` and `codex-ollama`|make OSS readiness a separate optional crate/feature if measurement justifies it|
|Embedded asset build-script invalidation|`core/Cargo.toml` uses `core/build.rs`|split or narrow asset embedding only after measuring invalidations|

## Non-Goals

- Do not weaken `[profile.release]` as a workaround for developer iteration.
- Do not remove user-facing subcommands from the installed CLI surface.
- Do not blindly cherry-pick the upstream project.
- Do not rely on repeated full `cargo clean` as the measurement or mitigation strategy.
- Do not speculate on dependency feature pruning without `cargo tree -e features`, timing, and smoke-test evidence.
