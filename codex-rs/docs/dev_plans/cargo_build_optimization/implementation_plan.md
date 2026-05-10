# Cargo Build Optimization - Implementation Plan

## Phase 0: Preserve Implemented Work

Confirm these files remain in the implemented shape:

- `core/src/tools/registry.rs` uses `ToolHandler` plus `AnyToolHandler`;
- `core/src/tasks/mod.rs` uses `SessionTask` plus `AnySessionTask`;
- `core/src/state/turn.rs` stores active tasks as `Arc<dyn AnySessionTask>`;
- root `Cargo.toml` contains `[profile.dev]`, `[profile.dev-small]`, `[profile.release]`, `[profile.release-fast]`, and `[profile.ci-test]`.

Checks:

```bash
rg -n "trait ToolHandler|trait AnyToolHandler|Arc<dyn AnyToolHandler>" core/src/tools/registry.rs
rg -n "trait SessionTask|trait AnySessionTask|Arc<dyn AnySessionTask>" core/src/tasks/mod.rs core/src/state/turn.rs
rg -n "^\[profile\.(dev|dev-small|release|release-fast|ci-test)\]" Cargo.toml
```

Acceptance:

- no regression to `#[async_trait]` on `ToolHandler` or `SessionTask`;
- profile settings remain explicit in the workspace root manifest.

## Phase 1: Establish Build Baseline

Run from the project root on a development host with the normal Rust toolchain installed.

Dependency closure:

```bash
scripts/cargo-local tree -p codex-cli -e normal > /tmp/codex-cli-tree-normal.txt
scripts/cargo-local tree -p codex-cli -e features > /tmp/codex-cli-tree-features.txt
scripts/cargo-local tree -p codex-cli --duplicates > /tmp/codex-cli-tree-duplicates.txt
```

Production release baseline:

```bash
scripts/cargo-local clean -p codex-cli --release >/dev/null
RUSTC_WRAPPER= CODEX_SANDBOX_NETWORK_DISABLED=1 \
  /usr/bin/time -p \
  scripts/cargo-local build -p codex-cli --bin codex --release --timings \
  >/tmp/codex-release.stdout \
  2>/tmp/codex-release.stderr
```

Fast release-like baseline:

```bash
scripts/cargo-local clean -p codex-cli --profile release-fast >/dev/null
RUSTC_WRAPPER= CODEX_SANDBOX_NETWORK_DISABLED=1 \
  /usr/bin/time -p \
  scripts/cargo-local build -p codex-cli --bin codex --profile release-fast --timings \
  >/tmp/codex-release-fast.stdout \
  2>/tmp/codex-release-fast.stderr
```

Acceptance:

- production and fast-profile timings are both captured;
- Cargo timing HTML artifacts are saved or referenced;
- dependency closure output is saved with the measurement.

## Phase 2: Developer Build Path Update

Update developer-facing docs or scripts to prefer:

```bash
CODEX_SANDBOX_NETWORK_DISABLED=1 scripts/cargo-local build -p codex-cli --bin codex --profile release-fast
```

for release-like local validation.

Keep production release packaging on:

```bash
CODEX_SANDBOX_NETWORK_DISABLED=1 scripts/cargo-local build -p codex-cli --bin codex --release
```

Smoke checks:

```bash
release_fast_bin="$(scripts/cargo-local --print-target-dir)/release-fast/codex"
"${release_fast_bin}" --help
"${release_fast_bin}" exec --help
"${release_fast_bin}" features list --help
```

Acceptance:

- docs clearly distinguish `release-fast` from production release;
- local smoke-test scripts can point at the fast binary;
- no release packaging script is migrated to `release-fast` by accident.

## Phase 3: Split Hidden Proxy Sidecars

Status: completed for `responses-api-proxy` and `stdio-to-uds`.

Start with commands that already have separate packages and standalone binary targets:

- `responses-api-proxy/Cargo.toml` provides `codex-responses-api-proxy`;
- `stdio-to-uds/Cargo.toml` provides `codex-stdio-to-uds`.

Implementation outline:

1. remove direct `codex-responses-api-proxy` and `codex-stdio-to-uds` library dependencies from `cli/Cargo.toml` if no other direct code path needs them;
2. replace `Subcommand::ResponsesApiProxy` and `Subcommand::StdioToUds` direct calls in `cli/src/main.rs` with sibling-executable dispatch;
3. preserve arguments and exit status;
4. add a clear missing-sidecar error path;
5. update packaging to include the sidecars when those hidden commands remain supported.

Smoke checks:

```bash
CODEX_SANDBOX_NETWORK_DISABLED=1 scripts/cargo-local build -p codex-cli --bin codex --profile release-fast
CODEX_SANDBOX_NETWORK_DISABLED=1 scripts/cargo-local build -p codex-responses-api-proxy --bin codex-responses-api-proxy --profile release-fast
CODEX_SANDBOX_NETWORK_DISABLED=1 scripts/cargo-local build -p codex-stdio-to-uds --bin codex-stdio-to-uds --profile release-fast
scripts/cargo-local tree -p codex-cli -e normal | rg "codex-responses-api-proxy|codex-stdio-to-uds" || true
```

Acceptance:

- primary `codex` binary still parses the hidden command arguments;
- hidden commands successfully dispatch to sidecars when installed;
- missing sidecar produces actionable error text;
- removed crates no longer appear in `codex-cli`'s normal dependency tree unless reachable through another path.

## Phase 4: Split MCP Server and Internal Tooling If Still Material

Candidates:

- `mcp-server/Cargo.toml` already provides `codex-mcp-server`;
- `execpolicy/Cargo.toml` already provides `codex-execpolicy`;
- app-server codegen/debug tooling can become a developer-only binary or package command.

For each candidate:

1. verify whether the sidecar binary already exists;
2. verify packaging and update paths;
3. replace direct library call only if command compatibility can be preserved;
4. measure `codex-cli` dependency closure before and after.

Smoke checks should include both primary and sidecar help output:

```bash
release_fast_dir="$(scripts/cargo-local --print-target-dir)/release-fast"
"${release_fast_dir}/codex" --help
"${release_fast_dir}/codex" mcp-server --help
"${release_fast_dir}/codex-mcp-server" --help
```

Acceptance:

- CLI compatibility is preserved or intentionally documented;
- removed crate disappears from `codex-cli`'s dependency closure;
- the sidecar is included by release packaging if the command remains supported.

## Phase 5: Evaluate `codex-common` OSS Split

Baseline:

```bash
scripts/cargo-local tree -p codex-cli -e normal | rg "codex-(lmstudio|ollama|common)"
```

Preferred implementation direction:

1. create a new `codex-oss` crate for local provider readiness/default-model helpers;
2. move `common/src/oss.rs` responsibilities there;
3. update call sites in `tui`, `exec`, and other local-provider flows;
4. keep `codex-common` focused on lightweight shared CLI/config helpers;
5. measure whether `codex-lmstudio` and `codex-ollama` remain in the primary `codex-cli` closure.

Alternative lower-churn direction:

1. make `codex-lmstudio` and `codex-ollama` optional dependencies of `codex-common`;
2. gate `common/src/oss.rs` behind an `oss` feature;
3. enable the feature only in crates that need local-provider readiness.

Acceptance:

- LM Studio and Ollama setup flows remain unchanged;
- non-local provider smoke flows still pass;
- feature unification does not accidentally reintroduce the dependencies into all `codex-cli` builds.

## Phase 6: Measure Core Follow-Ups

Run compiler-attribution commands only when the remaining bottleneck is clearly inside `codex-core` or another high-fanout crate:

```bash
scripts/cargo-local +nightly rustc -p codex-core --lib -- -Z time-passes -Z time-passes-format=json
scripts/cargo-local +nightly rustc -p codex-core --lib -- -Z self-profile=/tmp/codex-core-self-profile
scripts/cargo-local +nightly rustc -p codex-core --lib -- -Z macro-stats
```

Candidate follow-ups:

- split embedded skill assets out of `codex-core`;
- narrow `core/build.rs` invalidation paths;
- isolate schema/codegen-heavy modules;
- reduce proven monomorphization hot spots.

Acceptance:

- a follow-up is opened only with measurement evidence;
- protocol/wire compatibility tests cover any schema/derive changes;
- build-script changes preserve embedded asset behavior.

## Rollback Strategy

|Change|Rollback|
|---|---|
|Developer docs prefer `release-fast`|revert documentation/script references; production release unaffected|
|Hidden sidecar dispatch|restore direct library call and dependency in `cli/Cargo.toml`|
|MCP/internal tooling split|restore direct dependency and original match arm implementation|
|`codex-common` OSS split|move helpers back or re-enable the original feature by default|
|core asset split|restore `core/build.rs` and embedded asset paths|

## Final Acceptance for the Umbrella Plan

- `scripts/cargo-local build -p codex-cli --bin codex --profile release-fast` is the documented local release-like path.
- `scripts/cargo-local build -p codex-cli --bin codex --release` remains the production path.
- Every dependency-closure reduction is proven by `scripts/cargo-local tree -p codex-cli -e normal` before/after output.
- Every timing claim is backed by `--timings` and wall-clock captures.
- Primary user flows remain smoke-tested after each split.
- Sidecar-based commands have explicit packaging and missing-binary behavior.
