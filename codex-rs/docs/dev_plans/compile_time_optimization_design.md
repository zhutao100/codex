# Compile-time optimization proposal

## Status

Implemented in this branch. The native async `ToolHandler` and `SessionTask` refactors removed the broad `#[async_trait]` hotspots, the dev profiles were added, and the fast release-like profile plus release split-debug alignment were applied. Recorded measurements showed roughly 70% faster package-clean `codex-core` rebuilds and an 80.9% faster final `codex-cli` rebuild for the fast release-like profile versus the production release profile on the captured macOS run.

For this project's branch shape.

Upstream reference changes:
- PR #16630 / upstream commit `3c7f013`: native async `ToolHandler`.
- PR #16631 / upstream commit `7a3eec6`: native async `SessionTask`.

## Problem statement

`codex-core` in this branch still uses `#[async_trait]` at two high-fanout internal async trait boundaries:

1. `core/src/tools/registry.rs`: `ToolHandler` is object-safe and stored as `Arc<dyn ToolHandler>`. Its `is_mutating` and `handle` methods are async trait methods expanded by `async_trait`.
2. `core/src/tasks/mod.rs`: `SessionTask` is object-safe and stored as `Arc<dyn SessionTask>`. Its `run` and `abort` methods are async trait methods expanded by `async_trait`.

The concrete tool handlers and session tasks are numerous enough that the macro-generated futures and lifetime glue become a broad compile-time tax. This branch also carries a customized `ContinueTask` for `/pause` and `/continue`, so any session-task refactor must preserve the pause/continue lifecycle rather than blindly copying upstream branch.

The upstream branch addressed the same class of issue by moving the object-safe boundary from the implementation trait to a small internal adapter trait. Concrete impls use native return-position `impl Future` in traits (RPITIT) with explicit `Send` bounds; only the registry/session storage boundary boxes the future. The expected result for this branch is the same shape of compile-time improvement: materially less trait obligation evaluation, borrow-checking, monomorphization graph walk, and generated async-trait glue in `codex-core` package-clean rebuilds.

## Upstream reference analysis

|Upstream change|Mechanism|Reported impact|Why it matters for this branch|
|---|---|---:|---|
|PR #16630 / `3c7f013`|Removed `#[async_trait]` from concrete `ToolHandler` impls. `ToolHandler` methods now return native `impl Future + Send`; an internal `AnyToolHandler` boxes at the registry boundary.|`rustc total` for package-clean `codex-core` rebuild dropped from 187.15s to 68.98s, a 63.1% reduction.|this branch has the older object-safe `Arc<dyn ToolHandler>` design and many concrete handlers under `core/src/tools/handlers/`, so the same hotspot exists.|
|PR #16631 / `7a3eec6`|Removed `#[async_trait]` from concrete `SessionTask` impls. `SessionTask` methods now return native `impl Future + Send`; an internal `AnySessionTask` boxes at the running-task storage boundary.|On top of #16630, package-clean `codex-core` rebuild `rustc total` dropped from 67.21s to 35.08s, a 47.8% reduction.|this branch has the older `Arc<dyn SessionTask>` design plus a branch-specific `ContinueTask`; the same boundary can be adapted with one extra task impl.|

Important upstream lesson: the useful benchmark is a package-clean `codex-core` rebuild with dependencies warm, not a warm touched-file incremental check. #16631 explicitly found the touched-file check almost flat while package-clean rebuilds showed the real win.

## This branch code inspection summary

### `ToolHandler` hotspot

Current branch shape in `core/src/tools/registry.rs`:

```rust
#[async_trait]
pub trait ToolHandler: Send + Sync {
    fn kind(&self) -> ToolKind;

    async fn is_mutating(&self, _invocation: &ToolInvocation) -> bool {
        false
    }

    async fn handle(&self, invocation: ToolInvocation) -> Result<ToolOutput, FunctionCallError>;
}

pub struct ToolRegistry {
    handlers: HashMap<String, Arc<dyn ToolHandler>>,
}
```

Representative implementers inspected in this branch:

- `core/src/tools/handlers/apply_patch.rs`
- `core/src/tools/handlers/collab.rs`
- `core/src/tools/handlers/dynamic.rs`
- `core/src/tools/handlers/get_memory.rs`
- `core/src/tools/handlers/grep_files.rs`
- `core/src/tools/handlers/list_dir.rs`
- `core/src/tools/handlers/mcp.rs`
- `core/src/tools/handlers/mcp_resource.rs`
- `core/src/tools/handlers/plan.rs`
- `core/src/tools/handlers/read_file.rs`
- `core/src/tools/handlers/request_user_input.rs`
- `core/src/tools/handlers/shell.rs`
- `core/src/tools/handlers/test_sync.rs`
- `core/src/tools/handlers/unified_exec.rs`
- `core/src/tools/handlers/view_image.rs`

This branch has a simpler `ToolOutput` enum than the upstream branch. It does not need the upstream branch’s `ToolHandler::Output` associated type unless this branch also wants to backport later typed output machinery. The minimal branch adaptation can keep `ToolOutput` as the single output type.

### `SessionTask` hotspot

Current branch shape in `core/src/tasks/mod.rs`:

```rust
#[async_trait]
pub(crate) trait SessionTask: Send + Sync + 'static {
    fn kind(&self) -> TaskKind;

    async fn run(
        self: Arc<Self>,
        session: Arc<SessionTaskContext>,
        ctx: Arc<TurnContext>,
        input: Vec<UserInput>,
        cancellation_token: CancellationToken,
    ) -> Option<String>;

    async fn abort(&self, session: Arc<SessionTaskContext>, ctx: Arc<TurnContext>) {
        let _ = (session, ctx);
    }
}

// In spawn_task:
let task: Arc<dyn SessionTask> = Arc::new(task);
```

Concrete this-branch session-task impls using `#[async_trait]`:

- `core/src/tasks/compact.rs`
- `core/src/tasks/continue_task.rs`
- `core/src/tasks/ghost_snapshot.rs`
- `core/src/tasks/regular.rs`
- `core/src/tasks/review.rs`
- `core/src/tasks/undo.rs`
- `core/src/tasks/user_shell.rs`
- test-only impls in `core/src/codex.rs`.

The branch-specific `ContinueTask` must be migrated with the rest. Its existence does not change the adapter design; it only expands the implementation list and the pause/continue verification matrix.

### Build/profile observations

- Root `Cargo.toml` already optimizes release artifact size with:
  - `[profile.release] lto = "fat"`
  - `[profile.release] strip = "symbols"`
  - `[profile.release] codegen-units = 1`
- Root `Cargo.toml` already has the local-dev profile optimization:
  - `[profile.dev] debug = 1`
  - `[profile.dev-small]` inheriting `dev` with `debug = 0` and `strip = true`
- this branch has `core/build.rs`, which recursively emits `cargo:rerun-if-changed` for `core/src/skills/assets/samples`. Upstream branch no longer has `core/build.rs`; skills were moved into separate crates. This is lower priority than the async-trait hotspots but worth keeping in the follow-up queue.
- `core/tests/all.rs` aggregates the `core/tests/suite/*` modules into one integration-test binary, so the core test suite has already avoided the common “many integration test binaries” compile-time pitfall.

## Applicable Rust compile-time optimization research

### Measurement first

Use multiple complementary views because each answers a different question:

|Tool/command|Purpose|Use in this project|
|---|---|---|
|`cargo build --timings`|Crate-level critical path, parallelism, slow units, duplicate crate versions/features.|Run at workspace and `codex-core` focus levels to prove whether `codex-core` is the long pole. Current Cargo emits HTML under `target/cargo-timings/`; do not assume `--timings=json` is available.|
|`cargo +nightly rustc -p codex-core --lib -- -Z time-passes -Z time-passes-format=json`|rustc pass-level wall time.|Match the upstream #16630/#16631 measurement style.|
|`cargo +nightly rustc -p codex-core --lib -- -Z self-profile=...` + `measureme summarize`|Query-level and artifact-size attribution.|Confirm reductions in `evaluate_obligation`, `mir_borrowck`, and monomorphization rather than only process wall time.|
|`cargo +nightly rustc -- -Zmacro-stats`|Procedural/declarative macro expansion cost.|Check whether `async_trait`, `serde`, `schemars`, `rmcp`, `clap`, and other macros remain compile-time bottlenecks after the native async refactor.|
|`cargo llvm-lines`|Monomorphized LLVM IR line/copy counts.|Find generic helpers worth type-erasing or converting to non-generic inner functions.|
|`CARGO_LOG=cargo::core::compiler::fingerprint=info cargo build -vv`|Rebuild cause diagnosis.|Use if incremental rebuilds are unexpectedly invalidated by build scripts, environment variables, generated files, or feature drift.|

### Code-shape optimizations

|Pattern|Applicability to this branch|Recommendation|
|---|---|---|
|Remove broad `#[async_trait]` use on internal traits|Directly applicable to `ToolHandler` and `SessionTask`.|Primary implementation target. Use native RPITIT plus private object-safe adapters.|
|Move boxing/type-erasure to the actual storage boundary|Directly applicable: `ToolRegistry` and `RunningTask` are the storage boundaries.|Preserve dynamic dispatch at those boundaries; avoid forcing every concrete impl to hand-write boxed futures.|
|Reduce proc-macro fanout|Likely applicable after measuring. `codex-core` depends on macro-heavy crates (`clap`, `serde`, `schemars`, `rmcp`, `thiserror`, etc.).|Do not preemptively rewrite. Measure with `-Zmacro-stats`; feature-gate or split only proven hotspots.|
|Split large crates / isolate optional subsystems|Applicable but higher churn. Upstream branch has split many subsystems out of `core`.|Treat as future upstream-alignment work after low-churn async-trait backports. Candidate areas: system skills embedding, MCP-heavy code, shell/escalation runtimes.|
|Disable unused dependency features|Applicable, but risky without `cargo tree -e features` evidence.|Audit in a follow-up. Avoid speculative feature pruning that changes runtime behavior.|
|Use non-generic inner functions in hot generic APIs|Unknown until `cargo llvm-lines`/self-profile.|Apply opportunistically only to measured monomorphization hotspots.|

### Cargo/profile optimizations

|Setting/tool|Compile-time effect|This branch recommendation|
|---|---|---|
|`[profile.dev] debug = 1`|Reduces debug-info generation versus full dev debug info while preserving useful line tables/backtraces.|Already applied.|
|`[profile.dev-small] debug = 0`, `strip = true`|Fast/small local debug artifacts for scenarios that do not need debugger-friendly output.|Optional convenience profile; do not make it the default.|
|`codegen-units`|More units can reduce compile time by increasing backend parallelism, but can reduce runtime performance/size.|Leave release `codegen-units = 1` because this branch explicitly optimizes shipped binaries. Do not use release profile to evaluate edit-build speed.|
|`lto = "fat"`|Improves release runtime/size at substantial link-time cost.|Keep for release artifacts, but add documentation that developer loops should use `cargo check`, default dev profile, or `dev-small`.|
|`incremental`|Improves local rebuilds for workspace members; default dev already enables it.|No explicit change required. CI may keep incremental disabled unless cache strategy says otherwise.|
|`sccache`|Can improve repeated clean builds/CI by caching rustc outputs.|Recommended as environment/CI configuration, not committed as a hard repo default.|
|Faster linkers (`lld`/`mold`)|Can reduce link time, mostly relevant for binaries/release.|Optional developer docs; avoid checked-in universal linker flags because Codex targets macOS/Linux/Windows and already has target-specific Windows flags.|

## Proposed design for this branch

### Phase 0: establish baseline

Run all measurements from a clean but dependency-warm state. The `-Z` output is written by rustc on stderr, so capture stderr as the artifact. If the shell resolves a non-rustup `cargo`/`rustc` first, put rustup proxies at the front of `PATH` and clear `RUSTC_WRAPPER` so `cargo +nightly` really invokes nightly rustc:

```bash
CODEX_SANDBOX_NETWORK_DISABLED=1 cargo check -p codex-core --lib >/dev/null
cargo clean -p codex-core >/dev/null
PATH="$HOME/.cargo/bin:$PATH" RUSTC_WRAPPER= \
  CODEX_SANDBOX_NETWORK_DISABLED=1 \
  /usr/bin/time -p cargo +nightly rustc -p codex-core --lib -- \
  -Z time-passes \
  -Z time-passes-format=json \
  >/tmp/codex-core-time-passes-baseline.stdout \
  2>/tmp/codex-core-time-passes-baseline.stderr

cargo clean -p codex-core >/dev/null
PATH="$HOME/.cargo/bin:$PATH" RUSTC_WRAPPER= \
  CODEX_SANDBOX_NETWORK_DISABLED=1 \
  /usr/bin/time -p cargo +nightly build -p codex-core --lib --timings \
  >/tmp/codex-core-timings-baseline.stdout \
  2>/tmp/codex-core-timings-baseline.stderr
```

Also capture:

```bash
CODEX_SANDBOX_NETWORK_DISABLED=1 cargo tree -p codex-core -e features > /tmp/codex-core-features-baseline.txt
PATH="$HOME/.cargo/bin:$PATH" RUSTC_WRAPPER= \
  CODEX_SANDBOX_NETWORK_DISABLED=1 \
  cargo +nightly rustc -p codex-core --lib -- -Zmacro-stats \
  >/tmp/codex-core-macro-stats-baseline.stdout \
  2>/tmp/codex-core-macro-stats-baseline.stderr
```

Acceptance: baseline artifacts exist and clearly identify whether `codex-core` is the current long pole.

### Phase 1: native async `ToolHandler`

Minimal this-branch API sketch:

```rust
use futures::future::BoxFuture;

pub trait ToolHandler: Send + Sync {
    fn kind(&self) -> ToolKind;

    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(
            (self.kind(), payload),
            (ToolKind::Function, ToolPayload::Function { .. })
                | (ToolKind::Mcp, ToolPayload::Mcp { .. })
        )
    }

    fn is_mutating(
        &self,
        _invocation: &ToolInvocation,
    ) -> impl std::future::Future<Output = bool> + Send {
        async { false }
    }

    fn handle(
        &self,
        invocation: ToolInvocation,
    ) -> impl std::future::Future<Output = Result<ToolOutput, FunctionCallError>> + Send;
}

trait AnyToolHandler: Send + Sync {
    fn matches_kind(&self, payload: &ToolPayload) -> bool;
    fn is_mutating<'a>(&'a self, invocation: &'a ToolInvocation) -> BoxFuture<'a, bool>;
    fn handle_any<'a>(
        &'a self,
        invocation: ToolInvocation,
    ) -> BoxFuture<'a, Result<ToolOutput, FunctionCallError>>;
}

impl<T> AnyToolHandler for T
where
    T: ToolHandler,
{
    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        ToolHandler::matches_kind(self, payload)
    }

    fn is_mutating<'a>(&'a self, invocation: &'a ToolInvocation) -> BoxFuture<'a, bool> {
        Box::pin(ToolHandler::is_mutating(self, invocation))
    }

    fn handle_any<'a>(
        &'a self,
        invocation: ToolInvocation,
    ) -> BoxFuture<'a, Result<ToolOutput, FunctionCallError>> {
        Box::pin(ToolHandler::handle(self, invocation))
    }
}
```

Implementation notes:

- Change `ToolRegistry.handlers` from `HashMap<String, Arc<dyn ToolHandler>>` to `HashMap<String, Arc<dyn AnyToolHandler>>`.
- Keep `ToolRegistryBuilder::register_handler<T>` generic over `T: ToolHandler + 'static` and perform the cast to `Arc<dyn AnyToolHandler>` only when storing.
- Remove `use async_trait::async_trait;` and `#[async_trait]` from all concrete handlers.
- Keep existing `async fn handle` method bodies. In trait impls, native async syntax is accepted because the trait method desugars to `impl Future`; no manual `Box::pin` is needed in concrete handlers.
- Keep `ToolOutput` as-is. Do not backport upstream branch’s associated output type unless a later feature requires it.

Verification after Phase 1:

```bash
CODEX_SANDBOX_NETWORK_DISABLED=1 cargo check -p codex-core --lib
CODEX_SANDBOX_NETWORK_DISABLED=1 cargo test -p codex-core --test all --profile ci-test
cargo clean -p codex-core >/dev/null
PATH="$HOME/.cargo/bin:$PATH" RUSTC_WRAPPER= \
  CODEX_SANDBOX_NETWORK_DISABLED=1 \
  /usr/bin/time -p cargo +nightly rustc -p codex-core --lib -- \
  -Z time-passes \
  -Z time-passes-format=json \
  >/tmp/codex-core-time-passes-toolhandler.stdout \
  2>/tmp/codex-core-time-passes-toolhandler.stderr
```

Acceptance:

- Tool dispatch behavior unchanged for function, custom/freeform, local shell, and MCP payloads.
- Mutating-tool gate still waits before mutating handlers execute.
- Package-clean `codex-core` timing improves materially; if not, inspect self-profile before proceeding.

### Phase 2: native async `SessionTask`

Minimal this-branch API sketch:

```rust
use futures::future::BoxFuture;

pub(crate) trait SessionTask: Send + Sync + 'static {
    fn kind(&self) -> TaskKind;

    fn run(
        self: Arc<Self>,
        session: Arc<SessionTaskContext>,
        ctx: Arc<TurnContext>,
        input: Vec<UserInput>,
        cancellation_token: CancellationToken,
    ) -> impl std::future::Future<Output = Option<String>> + Send;

    fn abort(
        &self,
        session: Arc<SessionTaskContext>,
        ctx: Arc<TurnContext>,
    ) -> impl std::future::Future<Output = ()> + Send {
        async move {
            let _ = (session, ctx);
        }
    }
}

pub(crate) trait AnySessionTask: Send + Sync + 'static {
    fn kind(&self) -> TaskKind;

    fn run(
        self: Arc<Self>,
        session: Arc<SessionTaskContext>,
        ctx: Arc<TurnContext>,
        input: Vec<UserInput>,
        cancellation_token: CancellationToken,
    ) -> BoxFuture<'static, Option<String>>;

    fn abort<'a>(
        &'a self,
        session: Arc<SessionTaskContext>,
        ctx: Arc<TurnContext>,
    ) -> BoxFuture<'a, ()>;
}

impl<T> AnySessionTask for T
where
    T: SessionTask,
{
    fn kind(&self) -> TaskKind {
        SessionTask::kind(self)
    }

    fn run(
        self: Arc<Self>,
        session: Arc<SessionTaskContext>,
        ctx: Arc<TurnContext>,
        input: Vec<UserInput>,
        cancellation_token: CancellationToken,
    ) -> BoxFuture<'static, Option<String>> {
        Box::pin(SessionTask::run(self, session, ctx, input, cancellation_token))
    }

    fn abort<'a>(
        &'a self,
        session: Arc<SessionTaskContext>,
        ctx: Arc<TurnContext>,
    ) -> BoxFuture<'a, ()> {
        Box::pin(SessionTask::abort(self, session, ctx))
    }
}
```

Implementation notes:

- Change `RunningTask.task` in `core/src/state/turn.rs` from `Arc<dyn SessionTask>` to `Arc<dyn AnySessionTask>`.
- Change `Session::spawn_task` to store `Arc<dyn AnySessionTask>`.
- Remove `async_trait` imports and attributes from:
  - `compact.rs`
  - `continue_task.rs`
  - `ghost_snapshot.rs`
  - `regular.rs`
  - `review.rs`
  - `undo.rs`
  - `user_shell.rs`
  - any test-only `SessionTask` impls.
- Preserve this branch pause/continue semantics:
  - `pause_all_tasks` and `abort_all_tasks` still flow through `stop_all_tasks`.
  - `handle_task_abort` must call `task.abort(...)` through `AnySessionTask` before or during the same lifecycle points as today.
  - `ContinueTask` must remain a normal `SessionTask`; do not special-case it.
  - Existing `PendingContinuation`, `TurnPausedEvent`, `TurnContinuationSource`, and `/continue` tests should remain unchanged except for imports/attributes.
- Do not add upstream branch’s `span_name` unless needed separately. It is not required for the compile-time optimization.

Verification after Phase 2:

```bash
CODEX_SANDBOX_NETWORK_DISABLED=1 cargo check -p codex-core --lib
CODEX_SANDBOX_NETWORK_DISABLED=1 cargo test -p codex-core --test all --profile ci-test
cargo clean -p codex-core >/dev/null
PATH="$HOME/.cargo/bin:$PATH" RUSTC_WRAPPER= \
  CODEX_SANDBOX_NETWORK_DISABLED=1 \
  /usr/bin/time -p cargo +nightly rustc -p codex-core --lib -- \
  -Z time-passes \
  -Z time-passes-format=json \
  >/tmp/codex-core-time-passes-sessiontask.stdout \
  2>/tmp/codex-core-time-passes-sessiontask.stderr
```

Pause/continue targeted checks:

```bash
CODEX_SANDBOX_NETWORK_DISABLED=1 cargo test -p codex-core --test all pause --profile ci-test
CODEX_SANDBOX_NETWORK_DISABLED=1 cargo test -p codex-core --test all continue --profile ci-test
CODEX_SANDBOX_NETWORK_DISABLED=1 cargo test -p codex-core --test all abort_tasks --profile ci-test
```

Acceptance:

- `/pause` still cancels/parks active turn work according to this branch behavior.
- `/continue` still resumes from this branch’s pending-continuation state.
- Interrupt/replace abort paths still emit the expected `TurnAborted`/`TurnPaused`/completion events.
- Package-clean `codex-core` timing improves again after the ToolHandler refactor.

### Phase 3: backport low-risk dev profile changes

Add to root `Cargo.toml`:

```toml
[profile.dev]
# Keep line tables/backtraces while avoiding expensive full variable debug info
# across local dev builds.
debug = 1

[profile.dev-small]
inherits = "dev"
opt-level = 0
debug = 0
strip = true
```

Do not change `[profile.release]` by default. This branch already trades release compile/link time for shipped artifact size with fat LTO and `codegen-units = 1`. If release build time becomes a separate problem, create a separate `profile.release-fast` or build-script path rather than weakening the production release profile.

Verification:

```bash
CODEX_SANDBOX_NETWORK_DISABLED=1 cargo check -p codex-core --lib
CODEX_SANDBOX_NETWORK_DISABLED=1 cargo build -p codex-cli --profile dev-small
```

Acceptance:

- Default dev builds retain usable line-level diagnostics.
- Developers who only need smoke-test binaries can explicitly use `--profile dev-small`.

### Phase 4: measured follow-ups only

Treat these as follow-up tickets, not part of the minimal backport:

1. **System skills embedding/build script**
   - This branch embeds `core/src/skills/assets/samples` through `include_dir` and uses `core/build.rs` to track all sample files.
   - Upstream branch moved skills-related code into separate crates and no longer has `core/build.rs`.
   - Follow-up option: move embedded system skills into a smaller `codex-core-skills`-style crate or feature-gate it if most `codex-core` builds do not need embedded sample assets.

2. **MCP/schema/macro-heavy feature isolation**
   - `rmcp`, `schemars`, and serialization derives can be expensive.
   - Use `-Zmacro-stats` and `cargo build --timings` before changing features.
   - Avoid removing schema/serialization derives from protocol types unless wire compatibility is explicitly covered by tests.

3. **CI cache and linker policy**
   - Add `sccache` in CI or developer docs if clean builds dominate.
   - Consider faster linkers only as opt-in platform-specific developer configuration.
   - Do not commit universal linker flags in `.cargo/config.toml`; this repo targets macOS, Linux, and Windows and already carries target-specific Windows stack flags.

4. **Dependency feature audit**
   - Run `cargo tree -p codex-core -e features` and `cargo tree --duplicates`.
   - Disable unused dependency features only when tests prove behavior unchanged.

## Risk matrix

|Risk|Area|Mitigation|
|---|---|---|
|Native async trait methods are not object-safe|Tool/session trait storage|Keep private `AnyToolHandler` and `AnySessionTask` adapter traits as the only `dyn` boundaries.|
|Returned futures fail `Send` bounds|Concrete handlers/tasks|Compile errors identify non-`Send` values held across `await`; narrow scopes or move non-`Send` work before awaits.|
|Lifetime mismatch in adapter|`handle_any`, `is_mutating`, `abort`|Use explicit adapter lifetimes mirroring upstream: borrowed calls return `BoxFuture<'a, ...>`, task `run(self: Arc<Self>, ...)` returns `BoxFuture<'static, ...>`.|
|Pause/continue behavior regresses|branch-specific `ContinueTask` and task stop flow|Include targeted pause/continue tests in acceptance criteria; migrate `ContinueTask` exactly like other tasks.|
|Rust MSRV incompatibility|Native RPITIT in traits|Requires Rust 1.75+ for async fn/RPITIT in traits. This branch already uses edition 2024, so this should be acceptable; verify this branch’s published MSRV before implementation.|
|Benchmarks hide the win|Measurement|Use package-clean `codex-core` rebuilds with dependencies warm, not only touched-file incremental checks.|

## Expected implementation footprint

|Phase|Expected source footprint|Churn level|
|---|---:|---|
|ToolHandler native async|`core/src/tools/registry.rs` plus concrete handler files|Low/medium: mostly import/attribute removal and registry storage type change.|
|SessionTask native async|`core/src/tasks/mod.rs`, `core/src/state/turn.rs`, task impl files, test-only impls|Low/medium: same pattern, but lifecycle-sensitive because of pause/continue.|
|Dev profile|root `Cargo.toml`|Low.|
|Follow-up crate splitting|multiple crates/manifests|High; defer until measurements justify.|

## Recommended sequencing

1. Land measurement script/notes or at least capture manual baseline artifacts.
2. Implement Phase 1 (`ToolHandler`). Measure and test.
3. Implement Phase 2 (`SessionTask`). Measure and test with pause/continue focus.
4. Backport Phase 3 profile changes.
5. Only then decide whether system skills or macro/dependency work is worth the churn.

## Refresh: remaining slow `codex` release binary

The initial optimization work removed the largest known `codex-core` type-checking and monomorphization hotspot. The next optimization target is therefore different: `cargo build --release --bin codex` is dominated by the final optimized `codex-cli` binary build and link, not by the earlier high-fanout `#[async_trait]` boundaries.

### Repo-specific static diagnosis

`cli/src/main.rs` is a single multitool entrypoint. Building the `codex` binary links the interactive TUI together with many operational, daemon, debug, proxy, and integration subcommands. The direct `cli/Cargo.toml` dependency closure includes, among others:

- `codex-tui`
- `codex-core`
- `codex-app-server`
- `codex-app-server-protocol`
- `codex-app-server-test-client`
- `codex-codexd`
- `codex-cloud-tasks`
- `codex-exec`
- `codex-execpolicy`
- `codex-login`
- `codex-mcp-server`
- `codex-responses-api-proxy`
- `codex-rmcp-client`
- `codex-stdio-to-uds`

That layout is convenient for distribution and `arg0`-style dispatch, but it also means the release profile asks LLVM and the linker to optimize a very broad binary. The current root release profile is intentionally expensive:

```toml
[profile.release]
lto = "fat"
strip = "symbols"
split-debuginfo = "off"
codegen-units = 1
```

Cargo's default release profile is materially more compile-time friendly than this branch's production profile: default release uses `lto = false` and `codegen-units = 16`. This branch has deliberately chosen the opposite side of that tradeoff for shipped artifacts. The optimization plan should therefore add a developer/CI fast-release path and structural split options rather than silently weakening `[profile.release]`.

### Research delta since the first design note

|Finding|Practical implication for this branch|
|---|---|
|`cargo build --release` is exactly `cargo build --profile release`.|Add a separate custom profile for fast release-like local builds instead of changing what `--release` means.|
|`lto = "fat"` performs whole-graph LTO and can materially increase link time; `lto = "thin"` is explicitly described as substantially less time than fat LTO.|Use ThinLTO for a fast-release developer profile; keep FatLTO for production release until runtime/size data says otherwise.|
|`codegen-units = 1` maximizes cross-unit optimization opportunities but reduces backend parallelism.|Use more codegen units for fast-release developer builds; keep `1` for production if binary quality is still preferred.|
|Upstream branch `openai/codex` has added `split-debuginfo = "off"` to `[profile.release]`.|Backport this low-risk profile alignment even if expected impact is small with `debug = false`; it prevents accidental split-debug work if release debug settings change.|
|Upstream branch moved skill embedding out of `codex-core` into `codex-skills` / `codex-core-skills` style crates.|Treat this branch's `core/build.rs` + `include_dir` skill embedding as a measured follow-up split, especially if `codex-core` keeps rebuilding when only skill assets or adjacent files change.|
|LLD is a documented drop-in linker family, with a Mach-O `ld64.lld` path for macOS and potentially large wins on large programs.|Add opt-in local linker instructions; do not commit universal linker flags because this workspace targets macOS, Linux, and Windows.|
|`sccache` works as a `RUSTC_WRAPPER` for Rust and is useful for repeated clean dependency builds.|Recommend it for CI/developer environments, but do not make it mandatory. It will not remove the cost of a changed final binary link.|
|Cargo build scripts rerun conservatively unless `rerun-if-*` directives narrow invalidation.|Continue auditing build scripts, especially `core/build.rs`; directory-level `rerun-if-changed` scans should remain as narrow as possible.|

### Updated priority order

|Priority|Proposal|Expected impact|Risk/churn|
|---:|---|---|---|
|1|Add a `release-fast` custom profile for developer release-like builds.|High for `cargo build ... --profile release-fast`; avoids FatLTO and serial backend codegen.|Low; production `--release` unchanged.|
|2|Backport upstream `split-debuginfo = "off"` into `[profile.release]`.|Low to moderate; mostly upstream alignment and guardrail.|Very low.|
|3|Document opt-in LLD/sccache paths.|Medium in link-bound or repeated clean-build environments.|Low if opt-in only.|
|4|Split debug/test-client/schema-generation-only subcommands out of the shipped `codex` binary.|Potentially high for final binary codegen/link time.|Medium/high; distribution and CLI UX sensitive.|
|5|Extract embedded skills/assets out of `codex-core`, matching upstream branch direction.|Medium for package-clean `codex-core` rebuilds and build-script invalidation.|Medium; API and crate-boundary work.|
|6|Measure remaining `#[async_trait]`, derive macro, feature, and monomorphization hotspots.|Unknown until measured.|Low to high depending result.|

## Proposed additional optimizations

### Phase 5: add a fast release-like profile

Add a custom profile at the workspace root:

```toml
[profile.release-fast]
inherits = "release"
# Keep optimized codegen, but avoid the expensive production link profile.
lto = "thin"
codegen-units = 16
strip = "none"
split-debuginfo = "off"
```

Usage:

```bash
CODEX_SANDBOX_NETWORK_DISABLED=1 \
  cargo build -p codex-cli --bin codex --profile release-fast --timings
```

Comparison harness:

```bash
# Warm dependencies for each profile first.
CODEX_SANDBOX_NETWORK_DISABLED=1 cargo build -p codex-cli --bin codex --release >/dev/null
CODEX_SANDBOX_NETWORK_DISABLED=1 cargo build -p codex-cli --bin codex --profile release-fast >/dev/null

# Production release final-package rebuild.
cargo clean -p codex-cli --release >/dev/null
RUSTC_WRAPPER= CODEX_SANDBOX_NETWORK_DISABLED=1 \
  /usr/bin/time -p cargo build -p codex-cli --bin codex --release --timings \
  >/tmp/codex-release.stdout \
  2>/tmp/codex-release.stderr

# Fast release-like final-package rebuild.
cargo clean -p codex-cli --profile release-fast >/dev/null
RUSTC_WRAPPER= CODEX_SANDBOX_NETWORK_DISABLED=1 \
  /usr/bin/time -p cargo build -p codex-cli --bin codex --profile release-fast --timings \
  >/tmp/codex-release-fast.stdout \
  2>/tmp/codex-release-fast.stderr
```

Acceptance:

- `target/release-fast/codex --help` works.
- `target/release-fast/codex exec --help` works.
- Existing smoke tests that invoke the CLI can be pointed at the fast binary.
- Binary size/performance regressions are acceptable for this profile and are not used to judge the production release profile.
- The documentation clearly states that `--release` remains the production FatLTO profile.

Optional variant if ThinLTO remains too slow for edit-build loops:

```toml
[profile.release-nolto]
inherits = "release-fast"
lto = "off"
```

Use this only as a local smoke-test profile. It should not replace `release-fast` unless measurements show ThinLTO is still too expensive.

### Phase 6: align production release profile with upstream branch

Add the upstream-aligned split-debug setting:

```toml
[profile.release]
lto = "fat"
strip = "symbols"
codegen-units = 1
split-debuginfo = "off"
```

This is intentionally not a broad release-profile relaxation. It keeps this branch's existing production binary-quality policy while matching the upstream branch profile shape.

Acceptance:

```bash
CODEX_SANDBOX_NETWORK_DISABLED=1 cargo build -p codex-cli --bin codex --release
```

Then compare whether the build produces extra split debug/dSYM artifacts on macOS and whether wall time changes materially. If there is no measurable time improvement, keep the setting anyway for upstream profile alignment and future configuration guardrails.

### Phase 7: document opt-in faster linker and cache setup

Do not commit platform-wide linker flags into `.cargo/config.toml`. Add a local developer note instead, because the correct linker and flags differ across macOS, Linux, and Windows.

Recommended local experiment flow:

```bash
# Always measure the baseline first.
CODEX_SANDBOX_NETWORK_DISABLED=1 \
  cargo build -p codex-cli --bin codex --profile release-fast --timings

# Then test one linker configuration at a time in a local/uncommitted Cargo config
# or environment-specific build wrapper.
```

Example local Cargo config shapes to test, adjusted for the host triple:

```toml
# Linux example. Keep this local unless the CI image standardizes on LLD.
[target.x86_64-unknown-linux-gnu]
rustflags = ["-C", "link-arg=-fuse-ld=lld"]

# macOS example. Use the actual installed ld64.lld path if testing LLVM's Mach-O linker.
[target.aarch64-apple-darwin]
rustflags = ["-C", "link-arg=-fuse-ld=/path/to/ld64.lld"]
```

`sccache` setup should likewise be environment policy, not a required repo setting:

```bash
export RUSTC_WRAPPER=sccache
sccache --start-server || true
CODEX_SANDBOX_NETWORK_DISABLED=1 cargo build -p codex-cli --bin codex --profile release-fast
sccache --show-stats
```

Acceptance:

- Compare `--timings` and `/usr/bin/time -p` before/after.
- Check the produced binary launches and reports help text.
- Keep the configuration local unless the team standardizes the same linker in CI and on supported developer platforms.

### Phase 8: split heavyweight non-default tooling out of the final `codex` binary

The current `codex` binary includes hidden/debug and operational subcommands that are valuable but not necessarily required in every developer smoke-test binary. This is the most promising structural optimization after profile work.

Suggested split sequence:

1. **Move `codex-app-server-test-client` out of the default `codex` binary.**
   - Today the CLI crate depends on `codex-app-server-test-client` as a normal dependency.
   - Prefer a separate `codex-app-server-test-client` binary invocation for development/testing instead of linking it into the main `codex` binary.
   - Acceptance: existing app-server test-client workflows still have a binary, but `cargo build -p codex-cli --bin codex ...` no longer links the client.

2. **Move schema/export generation into a generator binary or feature.**
   - `codex-app-server-protocol` carries schema/export-oriented code and derive surface area.
   - If the shipped CLI only needs runtime protocol types, gate schema/export generation behind a feature used by a dedicated generator command.
   - Acceptance: generated schemas/TypeScript outputs are byte-stable under the generator, and the default `codex` binary does not compile generator-only dependencies.

3. **Evaluate helper binaries for app-server/proxy/cloud/codexd paths.**
   - `codex app-server`, `responses-api-proxy`, cloud task handling, and daemon paths may be better as sibling binaries if local interactive `codex` build time dominates developer loops.
   - This is a product/distribution decision. The multitool binary is useful; split only when the measured link-time win justifies the operational cost.

Measurement:

```bash
cargo tree -p codex-cli -e features > /tmp/codex-cli-features-before.txt
cargo tree -p codex-cli --duplicates > /tmp/codex-cli-duplicates-before.txt
CODEX_SANDBOX_NETWORK_DISABLED=1 \
  cargo build -p codex-cli --bin codex --profile release-fast --timings
```

After each split, recapture the same artifacts and compare target time, critical path, final binary size, and CLI smoke behavior.

### Phase 9: extract embedded skills/assets from `codex-core`

this branch still has `core/build.rs` and an `include_dir` dependency in `codex-core`. The build script recursively emits `cargo:rerun-if-changed` for `core/src/skills/assets/samples`. Upstream branch has moved this concern into separate skills-related crates.

Backport strategy:

1. Create a small `codex-skills` crate that owns embedded skills/assets and the related build script.
2. Optionally add a `codex-core-skills` adapter crate if the API boundary would otherwise make `codex-core` depend on asset layout details.
3. Remove `build = "build.rs"` and `include_dir` from `core/Cargo.toml` if the asset walker no longer belongs to `codex-core`.
4. Keep runtime behavior unchanged: embedded skill names, contents, and lookup semantics must remain stable.

Acceptance:

```bash
CODEX_SANDBOX_NETWORK_DISABLED=1 cargo check -p codex-core --lib
CODEX_SANDBOX_NETWORK_DISABLED=1 cargo test -p codex-core --test all skills --profile ci-test
cargo clean -p codex-core >/dev/null
CODEX_SANDBOX_NETWORK_DISABLED=1 cargo build -p codex-core --lib --timings
```

The expected win is not another 70% `codex-core` reduction; the goal is to stop asset/build-script concerns from invalidating or bloating the central core crate.

### Phase 10: measure remaining macro and monomorphization hotspots before refactoring

The earlier async-trait hotspots are fixed. Remaining `#[async_trait]` uses are lower-confidence targets unless measurement says otherwise. Do not perform a repo-wide mechanical rewrite without pass-level data.

Recommended evidence collection:

```bash
PATH="$HOME/.cargo/bin:$PATH" RUSTC_WRAPPER= \
  CODEX_SANDBOX_NETWORK_DISABLED=1 \
  cargo +nightly rustc -p codex-cli --bin codex --profile release-fast -- \
  -Z time-passes \
  -Z time-passes-format=json \
  >/tmp/codex-cli-time-passes.stdout \
  2>/tmp/codex-cli-time-passes.stderr

PATH="$HOME/.cargo/bin:$PATH" RUSTC_WRAPPER= \
  CODEX_SANDBOX_NETWORK_DISABLED=1 \
  cargo +nightly rustc -p codex-cli --bin codex --profile release-fast -- \
  -Zmacro-stats \
  >/tmp/codex-cli-macro-stats.stdout \
  2>/tmp/codex-cli-macro-stats.stderr

cargo llvm-lines -p codex-cli --bin codex --profile release-fast \
  > /tmp/codex-cli-llvm-lines.txt
```

Prioritize follow-up code-shape work only if the evidence identifies specific items, for example:

- a remaining object-safe `#[async_trait]` boundary with many implementers;
- a derive-heavy protocol/schema module that can be feature-gated;
- generic helper APIs producing large duplicated LLVM IR;
- duplicate dependency versions or unnecessary default features.

## Additional risk matrix entries

|Risk|Area|Mitigation|
|---|---|---|
|Fast-release profile becomes mistaken for production release|Profile policy|Keep `[profile.release]` unchanged and document that `--release` remains the shipped artifact profile.|
|ThinLTO or more codegen units hides runtime regressions|`release-fast`|Treat the profile as developer/CI smoke only unless benchmarked separately.|
|LLD behaves differently on one supported platform|Linker experiments|Keep linker flags opt-in/local until CI and platform smoke tests validate them.|
|`sccache` improves dependency rebuilds but not final binary link|Build cache expectations|Use `sccache --show-stats`; do not claim it fixes changed-final-crate release links.|
|Splitting subcommands changes distribution assumptions|CLI/multitool packaging|Split one low-risk debug/test-client path first; preserve user-visible commands or add wrapper dispatch.|
|Feature-gating protocol/schema derives breaks generated artifacts|App-server protocol|Add byte-stability tests for generated schema/TS outputs before gating dependencies.|
|Moving skills out of core changes embedded content semantics|Skills extraction|Snapshot embedded skill names/content and run skills-focused core tests.|

## Source/reference links

- Upstream PR #16630: https://github.com/openai/codex/pull/16630
- Upstream commit `3c7f013`: https://github.com/openai/codex/commit/3c7f013
- Upstream PR #16631: https://github.com/openai/codex/pull/16631
- Upstream commit `7a3eec6`: https://github.com/openai/codex/commit/7a3eec6
- Upstream branch `codex-rs/Cargo.toml`: https://raw.githubusercontent.com/openai/codex/HEAD/codex-rs/Cargo.toml
- Upstream branch `codex-core/Cargo.toml`: https://raw.githubusercontent.com/openai/codex/HEAD/codex-rs/core/Cargo.toml
- Upstream branch `codex-core-skills/Cargo.toml`: https://raw.githubusercontent.com/openai/codex/HEAD/codex-rs/core-skills/Cargo.toml
- Upstream branch `codex-skills/Cargo.toml`: https://raw.githubusercontent.com/openai/codex/HEAD/codex-rs/skills/Cargo.toml
- Rust blog on async fn/RPITIT in traits: https://blog.rust-lang.org/2023/12/21/async-fn-rpit-in-traits/
- Cargo profiles reference: https://doc.rust-lang.org/cargo/reference/profiles.html
- Cargo build timings reference: https://doc.rust-lang.org/cargo/reference/timings.html
- Cargo build scripts reference: https://doc.rust-lang.org/cargo/reference/build-scripts.html
- Cargo configuration reference: https://doc.rust-lang.org/cargo/reference/config.html
- Cargo build cache reference: https://doc.rust-lang.org/cargo/reference/build-cache.html
- rustc codegen options reference: https://doc.rust-lang.org/rustc/codegen-options/index.html
- `sccache` Rust documentation: https://github.com/mozilla/sccache/blob/HEAD/docs/Rust.md
- LLVM LLD documentation: https://lld.llvm.org/
- LLVM LLD Mach-O documentation: https://lld.llvm.org/MachO/index.html
- Rust Performance Book, compile times: https://nnethercote.github.io/perf-book/compile-times.html
- Faster Rust compile-time survey: https://corrode.dev/blog/tips-for-faster-rust-compile-times/
