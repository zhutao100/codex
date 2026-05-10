# Cargo Build Optimization - Design Proposal

## Status

Partially implemented. This proposal records the completed compile-time/profile work and defines the remaining build-structure plan.

## Design Principles

1. Keep the production release profile focused on shipped artifact quality.
2. Provide a fast release-like profile for local verification.
3. Reduce the primary `codex` binary dependency closure only at stable architectural seams.
4. Preserve the one-command user surface unless a sidecar split has an explicit compatibility story.
5. Measure before feature-pruning or crate-splitting.

## Proposal Summary

|Priority|Proposal|Status|Risk|
|---|---|---|---|
|P0|Native async `ToolHandler` and `SessionTask` adapters|Implemented|Low, already landed|
|P0|`dev`, `dev-small`, `release-fast`, and release split-debug profile work|Implemented|Low, already landed|
|P1|Make `release-fast` the documented developer release-like build path|Implemented|Low|
|P1|Add a repeatable measurement protocol for release vs release-fast and dependency closure|Implemented|Low|
|P2|Document opt-in local cache/linker accelerators|Proposed|Low if opt-in only|
|P3|Split cold/internal subcommands out of the primary `codex` binary|Partially implemented for hidden proxy sidecars; remaining candidates are measurement-gated|Medium/high|
|P4|Isolate OSS provider readiness from `codex-common` when not needed|Proposed, measurement-gated|Medium|
|P5|Split embedded skills/assets or macro-heavy subsystems only after attribution|Proposed, measurement-gated|Medium/high|

## Already Implemented Baseline

### Native Async Tool Dispatch

`core/src/tools/registry.rs` now keeps concrete tool handlers on native async trait methods and boxes only at the registry storage boundary.

Expected retained benefits:

- less `async_trait` macro expansion;
- fewer generated future types and lifetime glue in concrete handlers;
- smaller compiler obligation and borrow-checking workload for package-clean `codex-core` rebuilds.

### Native Async Session Tasks

`core/src/tasks/mod.rs` now applies the same adapter pattern to turn workflow tasks. This preserves branch-specific task types such as `ContinueTask` while reducing compile-time tax at the high-fanout task boundary.

### Fast Release-Like Profile

The branch has:

```toml
[profile.release-fast]
inherits = "release"
lto = "thin"
codegen-units = 16
strip = "none"
split-debuginfo = "off"
```

Recommended developer command:

```bash
CODEX_SANDBOX_NETWORK_DISABLED=1 \
  scripts/cargo-local build -p codex-cli --bin codex --profile release-fast --timings
```

Production release remains:

```bash
CODEX_SANDBOX_NETWORK_DISABLED=1 \
  scripts/cargo-local build -p codex-cli --bin codex --release --timings
```

## Phase 1: Standardize Measurements

Create a small repeatable measurement note or script under this proposal before further structural changes. In this branch, use `measure.sh` so local runs go through `scripts/cargo-local` and keep machine-specific captures outside the repository.

Minimum commands:

```bash
# Dependency graph and feature fanout.
scripts/cargo-local tree -p codex-cli -e normal > /tmp/codex-cli-tree-normal.txt
scripts/cargo-local tree -p codex-cli -e features > /tmp/codex-cli-tree-features.txt
scripts/cargo-local tree -p codex-cli --duplicates > /tmp/codex-cli-tree-duplicates.txt

# Production release timing.
scripts/cargo-local clean -p codex-cli --release >/dev/null
RUSTC_WRAPPER= CODEX_SANDBOX_NETWORK_DISABLED=1 \
  /usr/bin/time -p \
  scripts/cargo-local build -p codex-cli --bin codex --release --timings \
  >/tmp/codex-release.stdout \
  2>/tmp/codex-release.stderr

# Fast release-like timing.
scripts/cargo-local clean -p codex-cli --profile release-fast >/dev/null
RUSTC_WRAPPER= CODEX_SANDBOX_NETWORK_DISABLED=1 \
  /usr/bin/time -p \
  scripts/cargo-local build -p codex-cli --bin codex --profile release-fast --timings \
  >/tmp/codex-release-fast.stdout \
  2>/tmp/codex-release-fast.stderr
```

Use package-scoped cleans for measurement isolation. Do not normalize workflows around repeated full workspace `cargo clean`.

Acceptance:

- timings include Cargo HTML timing artifacts for both profiles;
- stdout/stderr captures include total wall time;
- dependency tree captures identify high-level project crates in the selected binary closure;
- results distinguish compiler hot spots from final codegen/link hot spots.

## Phase 2: Make `release-fast` the Default Developer Release-Like Path

Document the intended split:

- `--release`: production shipped artifact profile;
- `--profile release-fast`: developer release-like profile for smoke testing and local iteration;
- `--profile dev-small`: fast non-optimized binary profile when optimizer behavior is not relevant.

Recommended checks:

```bash
CODEX_SANDBOX_NETWORK_DISABLED=1 scripts/cargo-local build -p codex-cli --bin codex --profile release-fast
release_fast_bin="$(scripts/cargo-local --print-target-dir)/release-fast/codex"
"${release_fast_bin}" --help
"${release_fast_bin}" exec --help
"${release_fast_bin}" features list --help
```

Acceptance:

- `release-fast` binary is not used for release packaging;
- documentation does not imply performance or size parity with production release;
- smoke tests can opt into the `release-fast` binary path.

## Phase 3: Split Cold/Internal Subcommands From the Primary Binary

### Candidate Sidecar Splits

|Current command surface|Current dependency pressure|Possible split|
|---|---|---|
|`codex app-server generate-ts` and `generate-json-schema`|`codex-app-server-protocol`, codegen/schema dependencies, test-client adjacency|move protocol generation to a separate developer/codegen binary|
|hidden `responses-api-proxy`|removed from `codex-cli` dependency closure|dispatches to existing `codex-responses-api-proxy` sidecar binary|
|hidden `stdio-to-uds`|removed from `codex-cli` dependency closure|dispatches to existing `codex-stdio-to-uds` sidecar binary|
|`mcp-server`|`codex-mcp-server` in `codex-cli`|dispatch to existing `codex-mcp-server` sidecar binary if packaging already includes it|
|`cloud` / `cloud-tasks`|`codex-cloud-tasks`, TUI, login/core closure|move to an optional sidecar if cloud task UX does not require the primary binary|
|debug app-server tooling|debug/test-client dependencies|move to a developer-only binary or gate behind non-release packaging|
|hidden `execpolicy check`|`codex-execpolicy` dependency|dispatch to `codex-execpolicy` sidecar or remove from shipped front door if internal-only|

### Dispatch Model

Keep `codex` as the user-facing front door where compatibility requires it. Replace direct library calls for cold sidecars with one of these patterns:

1. spawn a sibling executable installed with the distribution;
2. use `arg0`-style dispatch where packaging invokes the same artifact under a sidecar name only if this still reduces dependency closure;
3. create a developer-only cargo command path and remove the cold library dependency from `codex-cli`.

Pattern 1 is most likely to reduce the primary binary dependency closure because the sidecar is a distinct Cargo target with its own dependencies.

### Compatibility Requirements

For each split:

- preserve existing CLI arguments and exit codes where public;
- preserve hidden command behavior for internal scripts or update callers;
- provide a clear error if the sidecar executable is missing;
- update packaging manifests to include the sidecar when the command remains supported;
- keep `codex --help` and subcommand help stable unless the command is intentionally hidden/developer-only.

### Measurement After Each Split

After each candidate split:

```bash
scripts/cargo-local tree -p codex-cli -e normal > /tmp/codex-cli-tree-normal.after.txt
CODEX_SANDBOX_NETWORK_DISABLED=1 scripts/cargo-local build -p codex-cli --bin codex --profile release-fast --timings
CODEX_SANDBOX_NETWORK_DISABLED=1 scripts/cargo-local build -p codex-cli --bin codex --release --timings
```

Acceptance:

- removed crate no longer appears in `scripts/cargo-local tree -p codex-cli -e normal`, unless still reachable through another primary path;
- primary `codex` smoke tests pass;
- sidecar command smoke tests pass;
- production packaging still includes required sidecars.

## Phase 4: Isolate OSS Provider Readiness

Current shape:

- `common/Cargo.toml` unconditionally depends on `codex-lmstudio` and `codex-ollama`;
- `common/src/oss.rs` provides shared readiness/default-model helpers;
- `codex-cli`, `codex-tui`, and `codex-exec` can pull that path through `codex-common`.

Candidate designs:

|Design|Shape|Pros|Cons|
|---|---|---|---|
|Dedicated `codex-oss` crate|move `common/src/oss.rs` and local-provider dependencies to a new crate|clean boundary; primary common crate gets lighter|requires call-site updates|
|Optional `codex-common/oss` feature|gate `codex-lmstudio` and `codex-ollama` behind a feature|lower churn|feature unification can still pull dependencies into `codex-cli` if any selected path enables it|
|Provider-specific sidecar checks|run local-provider readiness through provider sidecars|largest closure reduction|higher UX/runtime complexity|

Recommended first step: introduce a dedicated `codex-oss` crate or module boundary, then measure whether `codex-cli` still requires it for default TUI/exec flows.

Acceptance:

- local provider flows for LM Studio and Ollama remain unchanged;
- non-local default provider flows do not require local-provider crates unless the selected command path needs them;
- `scripts/cargo-local tree -p codex-cli -e normal` proves whether `codex-lmstudio` and `codex-ollama` remain in the primary closure.

## Phase 5: Measured Core Follow-Ups

### Embedded Skills and Build Script

`core/Cargo.toml` still uses `build = "build.rs"`. Keep the existing behavior until invalidation evidence says otherwise.

Follow-up options:

- move embedded sample skills to a smaller skills/assets crate;
- narrow `rerun-if-changed` paths if the build script currently watches too broad a directory;
- feature-gate embedded samples only if runtime behavior remains covered by tests.

### Macro-Heavy and Schema-Heavy Surfaces

Only after measurement, inspect:

- `rmcp` macros and schema generation;
- `schemars` derives;
- app-server protocol generation;
- `serde`/`ts-rs` fanout;
- high-copy generic APIs via `cargo llvm-lines`.

Do not remove derives or feature flags from wire/protocol types without compatibility tests.

## Phase 6: Opt-In Local Build Acceleration

Document, but do not force, local accelerators:

- `sccache` through `RUSTC_WRAPPER` for repeated clean dependency builds;
- platform-specific faster linker configuration for local builds;
- external build directory on high-endurance storage if SSD write amplification is a concern.

Do not commit universal linker flags to `.cargo/config.toml`; this workspace targets macOS, Linux, and Windows.

## Recommended Sequencing

1. Preserve the implemented native-async and profile work.
2. Capture release vs release-fast measurements.
3. Promote `release-fast` in developer docs/scripts.
4. Split the lowest-risk hidden sidecars first: `responses-api-proxy` and `stdio-to-uds`.
5. Measure again.
6. Split codegen/debug tooling if still material.
7. Evaluate OSS provider isolation.
8. Defer embedded asset and macro-heavy crate splits until attribution justifies them.
