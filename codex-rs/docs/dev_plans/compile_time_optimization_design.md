# Compile-time optimization proposal

For `codex-rs` v0.98 shape.

Upstream reference changes:
- PR #16630 / main commit `3c7f013`: native async `ToolHandler`.
- PR #16631 / main commit `7a3eec6`: native async `SessionTask`.

## Problem statement

`codex-core` in the v0.98 branch still uses `#[async_trait]` at two high-fanout internal async trait boundaries:

1. `core/src/tools/registry.rs`: `ToolHandler` is object-safe and stored as `Arc<dyn ToolHandler>`. Its `is_mutating` and `handle` methods are async trait methods expanded by `async_trait`.
2. `core/src/tasks/mod.rs`: `SessionTask` is object-safe and stored as `Arc<dyn SessionTask>`. Its `run` and `abort` methods are async trait methods expanded by `async_trait`.

The concrete tool handlers and session tasks are numerous enough that the macro-generated futures and lifetime glue become a broad compile-time tax. The v0.98 branch also carries a customized `ContinueTask` for `/pause` and `/continue`, so any session-task refactor must preserve the pause/continue lifecycle rather than blindly copying latest upstream.

Upstream v0.119 addressed the same class of issue by moving the object-safe boundary from the implementation trait to a small internal adapter trait. Concrete impls use native return-position `impl Future` in traits (RPITIT) with explicit `Send` bounds; only the registry/session storage boundary boxes the future. The expected result for v0.98 is the same shape of compile-time improvement: materially less trait obligation evaluation, borrow-checking, monomorphization graph walk, and generated async-trait glue in `codex-core` package-clean rebuilds.

## Upstream reference analysis

| Upstream change | Mechanism | Reported impact | Why it matters for v0.98 |
|---|---|---:|---|
| PR #16630 / `3c7f013` | Removed `#[async_trait]` from concrete `ToolHandler` impls. `ToolHandler` methods now return native `impl Future + Send`; an internal `AnyToolHandler` boxes at the registry boundary. | `rustc total` for package-clean `codex-core` rebuild dropped from 187.15s to 68.98s, a 63.1% reduction. | v0.98 has the older object-safe `Arc<dyn ToolHandler>` design and many concrete handlers under `core/src/tools/handlers/`, so the same hotspot exists. |
| PR #16631 / `7a3eec6` | Removed `#[async_trait]` from concrete `SessionTask` impls. `SessionTask` methods now return native `impl Future + Send`; an internal `AnySessionTask` boxes at the running-task storage boundary. | On top of #16630, package-clean `codex-core` rebuild `rustc total` dropped from 67.21s to 35.08s, a 47.8% reduction. | v0.98 has the older `Arc<dyn SessionTask>` design plus a branch-specific `ContinueTask`; the same boundary can be adapted with one extra task impl. |

Important upstream lesson: the useful benchmark is a package-clean `codex-core` rebuild with dependencies warm, not a warm touched-file incremental check. #16631 explicitly found the touched-file check almost flat while package-clean rebuilds showed the real win.

## v0.98 code inspection summary

### `ToolHandler` hotspot

Current v0.98 shape in `core/src/tools/registry.rs`:

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

Representative implementers inspected in v0.98:

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

v0.98 has a simpler `ToolOutput` enum than latest upstream. It does not need the latest `ToolHandler::Output` associated type unless the branch also wants to backport later typed output machinery. The minimal v0.98 adaptation can keep `ToolOutput` as the single output type.

### `SessionTask` hotspot

Current v0.98 shape in `core/src/tasks/mod.rs`:

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

Concrete v0.98 session-task impls using `#[async_trait]`:

- `core/src/tasks/compact.rs`
- `core/src/tasks/continue_task.rs`
- `core/src/tasks/ghost_snapshot.rs`
- `core/src/tasks/regular.rs`
- `core/src/tasks/review.rs`
- `core/src/tasks/undo.rs`
- `core/src/tasks/user_shell.rs`
- test-only impls in `core/src/codex_tests.rs`, if present in the branch checkout.

The branch-specific `ContinueTask` must be migrated with the rest. Its existence does not change the adapter design; it only expands the implementation list and the pause/continue verification matrix.

### Build/profile observations

- Root `Cargo.toml` already optimizes release artifact size with:
  - `[profile.release] lto = "fat"`
  - `[profile.release] strip = "symbols"`
  - `[profile.release] codegen-units = 1`
- Root `Cargo.toml` does **not** have the latest upstream local-dev profile optimization:
  - `[profile.dev] debug = 1`
  - `[profile.dev-small]` inheriting `dev` with `debug = 0` and `strip = true`
- v0.98 has `core/build.rs`, which recursively emits `cargo:rerun-if-changed` for `core/src/skills/assets/samples`. Latest upstream no longer has `core/build.rs`; skills were moved into separate crates. This is lower priority than the async-trait hotspots but worth keeping in the follow-up queue.
- `core/tests/all.rs` aggregates the `core/tests/suite/*` modules into one integration-test binary, so the core test suite has already avoided the common “many integration test binaries” compile-time pitfall.

## Applicable Rust compile-time optimization research

### Measurement first

Use multiple complementary views because each answers a different question:

| Tool/command | Purpose | Use in this project |
|---|---|---|
| `cargo build --timings` | Crate-level critical path, parallelism, slow units, duplicate crate versions/features. | Run at workspace and `codex-core` focus levels to prove whether `codex-core` is the long pole. |
| `cargo +nightly rustc -p codex-core --lib -- -Z time-passes -Z time-passes-format=json` | rustc pass-level wall time. | Match the upstream #16630/#16631 measurement style. |
| `cargo +nightly rustc -p codex-core --lib -- -Z self-profile=...` + `measureme summarize` | Query-level and artifact-size attribution. | Confirm reductions in `evaluate_obligation`, `mir_borrowck`, and monomorphization rather than only process wall time. |
| `cargo +nightly rustc -- -Zmacro-stats` | Procedural/declarative macro expansion cost. | Check whether `async_trait`, `serde`, `schemars`, `rmcp`, `clap`, and other macros remain compile-time bottlenecks after the native async refactor. |
| `cargo llvm-lines` | Monomorphized LLVM IR line/copy counts. | Find generic helpers worth type-erasing or converting to non-generic inner functions. |
| `CARGO_LOG=cargo::core::compiler::fingerprint=info cargo build -vv` | Rebuild cause diagnosis. | Use if incremental rebuilds are unexpectedly invalidated by build scripts, environment variables, generated files, or feature drift. |

### Code-shape optimizations

| Pattern | Applicability to v0.98 | Recommendation |
|---|---|---|
| Remove broad `#[async_trait]` use on internal traits | Directly applicable to `ToolHandler` and `SessionTask`. | Primary implementation target. Use native RPITIT plus private object-safe adapters. |
| Move boxing/type-erasure to the actual storage boundary | Directly applicable: `ToolRegistry` and `RunningTask` are the storage boundaries. | Preserve dynamic dispatch at those boundaries; avoid forcing every concrete impl to hand-write boxed futures. |
| Reduce proc-macro fanout | Likely applicable after measuring. `codex-core` depends on macro-heavy crates (`clap`, `serde`, `schemars`, `rmcp`, `thiserror`, etc.). | Do not preemptively rewrite. Measure with `-Zmacro-stats`; feature-gate or split only proven hotspots. |
| Split large crates / isolate optional subsystems | Applicable but higher churn. Latest upstream has split many subsystems out of `core`. | Treat as future upstream-alignment work after low-churn async-trait backports. Candidate areas: system skills embedding, MCP-heavy code, shell/escalation runtimes. |
| Disable unused dependency features | Applicable, but risky without `cargo tree -e features` evidence. | Audit in a follow-up. Avoid speculative feature pruning that changes runtime behavior. |
| Use non-generic inner functions in hot generic APIs | Unknown until `cargo llvm-lines`/self-profile. | Apply opportunistically only to measured monomorphization hotspots. |

### Cargo/profile optimizations

| Setting/tool | Compile-time effect | v0.98 recommendation |
|---|---|---|
| `[profile.dev] debug = 1` | Reduces debug-info generation versus full dev debug info while preserving useful line tables/backtraces. | Low-risk local-dev improvement; backport from latest upstream. |
| `[profile.dev-small] debug = 0`, `strip = true` | Fast/small local debug artifacts for scenarios that do not need debugger-friendly output. | Optional convenience profile; do not make it the default. |
| `codegen-units` | More units can reduce compile time by increasing backend parallelism, but can reduce runtime performance/size. | Leave release `codegen-units = 1` because v0.98 explicitly optimizes shipped binaries. Do not use release profile to evaluate edit-build speed. |
| `lto = "fat"` | Improves release runtime/size at substantial link-time cost. | Keep for release artifacts, but add documentation that developer loops should use `cargo check`, default dev profile, or `dev-small`. |
| `incremental` | Improves local rebuilds for workspace members; default dev already enables it. | No explicit change required. CI may keep incremental disabled unless cache strategy says otherwise. |
| `sccache` | Can improve repeated clean builds/CI by caching rustc outputs. | Recommended as environment/CI configuration, not committed as a hard repo default. |
| Faster linkers (`lld`/`mold`) | Can reduce link time, mostly relevant for binaries/release. | Optional developer docs; avoid checked-in universal linker flags because Codex targets macOS/Linux/Windows and already has target-specific Windows flags. |

## Proposed design for v0.98

### Phase 0: establish baseline

Run all measurements from a clean but dependency-warm state:

```bash
cargo check -p codex-core --lib >/dev/null
cargo clean -p codex-core >/dev/null
/usr/bin/time -p cargo +nightly rustc -p codex-core --lib -- \
  -Z time-passes \
  -Z time-passes-format=json >/tmp/codex-core-time-passes-baseline.jsonl

cargo clean -p codex-core >/dev/null
cargo +nightly build -p codex-core --lib \
  -Z unstable-options \
  --timings=json >/tmp/codex-core-timings-baseline.jsonl
```

Also capture:

```bash
cargo tree -p codex-core -e features > /tmp/codex-core-features-baseline.txt
cargo +nightly rustc -p codex-core --lib -- -Zmacro-stats > /tmp/codex-core-macro-stats-baseline.txt
```

Acceptance: baseline artifacts exist and clearly identify whether `codex-core` is the current long pole.

### Phase 1: native async `ToolHandler`

Minimal v0.98 API sketch:

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
- Keep `ToolOutput` as-is. Do not backport latest upstream’s associated output type unless a later feature requires it.

Verification after Phase 1:

```bash
cargo check -p codex-core --lib
cargo test -p codex-core --test all --profile ci-test
cargo clean -p codex-core >/dev/null
/usr/bin/time -p cargo +nightly rustc -p codex-core --lib -- \
  -Z time-passes \
  -Z time-passes-format=json >/tmp/codex-core-time-passes-toolhandler.jsonl
```

Acceptance:

- Tool dispatch behavior unchanged for function, custom/freeform, local shell, and MCP payloads.
- Mutating-tool gate still waits before mutating handlers execute.
- Package-clean `codex-core` timing improves materially; if not, inspect self-profile before proceeding.

### Phase 2: native async `SessionTask`

Minimal v0.98 API sketch:

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
- Preserve v0.98 pause/continue semantics:
  - `pause_all_tasks` and `abort_all_tasks` still flow through `stop_all_tasks`.
  - `handle_task_abort` must call `task.abort(...)` through `AnySessionTask` before or during the same lifecycle points as today.
  - `ContinueTask` must remain a normal `SessionTask`; do not special-case it.
  - Existing `PendingContinuation`, `TurnPausedEvent`, `TurnContinuationSource`, and `/continue` tests should remain unchanged except for imports/attributes.
- Do not add latest upstream’s `span_name` unless needed separately. It is not required for the compile-time optimization.

Verification after Phase 2:

```bash
cargo check -p codex-core --lib
cargo test -p codex-core --test all --profile ci-test
cargo clean -p codex-core >/dev/null
/usr/bin/time -p cargo +nightly rustc -p codex-core --lib -- \
  -Z time-passes \
  -Z time-passes-format=json >/tmp/codex-core-time-passes-sessiontask.jsonl
```

Pause/continue targeted checks:

```bash
cargo test -p codex-core --test all pause --profile ci-test
cargo test -p codex-core --test all continue --profile ci-test
cargo test -p codex-core --test all abort_tasks --profile ci-test
```

Acceptance:

- `/pause` still cancels/parks active turn work according to v0.98 behavior.
- `/continue` still resumes from the branch’s pending-continuation state.
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

Do not change `[profile.release]` by default. The branch already trades release compile/link time for shipped artifact size with fat LTO and `codegen-units = 1`. If release build time becomes a separate problem, create a separate `profile.release-fast` or build-script path rather than weakening the production release profile.

Verification:

```bash
cargo check -p codex-core --lib
cargo build -p codex-cli --profile dev-small
```

Acceptance:

- Default dev builds retain usable line-level diagnostics.
- Developers who only need smoke-test binaries can explicitly use `--profile dev-small`.

### Phase 4: measured follow-ups only

Treat these as follow-up tickets, not part of the minimal backport:

1. **System skills embedding/build script**
   - Current v0.98 embeds `core/src/skills/assets/samples` through `include_dir` and uses `core/build.rs` to track all sample files.
   - Latest upstream moved skills-related code into separate crates and no longer has `core/build.rs`.
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

| Risk | Area | Mitigation |
|---|---|---|
| Native async trait methods are not object-safe | Tool/session trait storage | Keep private `AnyToolHandler` and `AnySessionTask` adapter traits as the only `dyn` boundaries. |
| Returned futures fail `Send` bounds | Concrete handlers/tasks | Compile errors identify non-`Send` values held across `await`; narrow scopes or move non-`Send` work before awaits. |
| Lifetime mismatch in adapter | `handle_any`, `is_mutating`, `abort` | Use explicit adapter lifetimes mirroring upstream: borrowed calls return `BoxFuture<'a, ...>`, task `run(self: Arc<Self>, ...)` returns `BoxFuture<'static, ...>`. |
| Pause/continue behavior regresses | v0.98-specific `ContinueTask` and task stop flow | Include targeted pause/continue tests in acceptance criteria; migrate `ContinueTask` exactly like other tasks. |
| Rust MSRV incompatibility | Native RPITIT in traits | Requires Rust 1.75+ for async fn/RPITIT in traits. v0.98 already uses edition 2024, so this should be acceptable; verify the branch’s published MSRV before implementation. |
| Benchmarks hide the win | Measurement | Use package-clean `codex-core` rebuilds with dependencies warm, not only touched-file incremental checks. |

## Expected implementation footprint

| Phase | Expected source footprint | Churn level |
|---|---:|---|
| ToolHandler native async | `core/src/tools/registry.rs` plus concrete handler files | Low/medium: mostly import/attribute removal and registry storage type change. |
| SessionTask native async | `core/src/tasks/mod.rs`, `core/src/state/turn.rs`, task impl files, test-only impls | Low/medium: same pattern, but lifecycle-sensitive because of pause/continue. |
| Dev profile | root `Cargo.toml` | Low. |
| Follow-up crate splitting | multiple crates/manifests | High; defer until measurements justify. |

## Recommended sequencing

1. Land measurement script/notes or at least capture manual baseline artifacts.
2. Implement Phase 1 (`ToolHandler`). Measure and test.
3. Implement Phase 2 (`SessionTask`). Measure and test with pause/continue focus.
4. Backport Phase 3 profile changes.
5. Only then decide whether system skills or macro/dependency work is worth the churn.

## Source/reference links

- Upstream PR #16630: https://github.com/openai/codex/pull/16630
- Upstream main commit `3c7f013`: https://github.com/openai/codex/commit/3c7f013
- Upstream PR #16631: https://github.com/openai/codex/pull/16631
- Upstream main commit `7a3eec6`: https://github.com/openai/codex/commit/7a3eec6
- Rust blog on async fn/RPITIT in traits: https://blog.rust-lang.org/2023/12/21/async-fn-rpit-in-traits/
- Cargo profiles reference: https://doc.rust-lang.org/cargo/reference/profiles.html
- Cargo build timings reference: https://doc.rust-lang.org/cargo/reference/timings.html
- Rust Performance Book, compile times: https://nnethercote.github.io/perf-book/compile-times.html
- Faster Rust compile-time survey: https://corrode.dev/blog/tips-for-faster-rust-compile-times/
