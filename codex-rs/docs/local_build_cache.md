# Local build cache

Use `cargo-local` for local Cargo loops that can create large debug
artifacts:

```bash
CODEX_SANDBOX_NETWORK_DISABLED=1 cargo-local check -p codex-cli --bin codex
CODEX_SANDBOX_NETWORK_DISABLED=1 cargo-local build -p codex-cli --bin codex --profile dev-small
CODEX_SANDBOX_NETWORK_DISABLED=1 cargo-local test -p codex-core --lib
```

The repo `just` recipes and `scripts/debug-codex.sh` call this wrapper.
The workspace VS Code rust-analyzer settings also call it for diagnostics,
build-script/proc-macro loading, and runnables.

`cargo-local test` defaults to agent-oriented output. It still runs
Cargo with quiet short diagnostics, then filters successful libtest progress
blocks into one compact summary while preserving warnings, errors, failure
details, and sanitized local paths. Set
`CODEX_RS_CARGO_LOCAL_TEST_OUTPUT=raw` when exact Cargo/libtest output is needed.

When `CODEX_SANDBOX_NETWORK_DISABLED=1` wraps Cargo on macOS, the wrapper reuses
already-downloaded native artifacts for WebRTC and V8 from sibling local target
dirs by setting `LK_CUSTOM_WEBRTC` and `RUSTY_V8_ARCHIVE` when those variables
are not already set. This keeps broad test runs network-closed while avoiding
build-script downloads for artifacts that are already present locally.

Network-disabled test runs also default `RUST_MIN_STACK` to 8 MiB when the
caller has not set it, matching the stack needs of the app-server protocol tests
on macOS debug builds.

Target-dir selection:

1. Existing `CARGO_TARGET_DIR`.
2. `CARGO_LOCAL_BUILD_ROOT` or `CODEX_RS_BUILD_ROOT`, with target dirs stored
   under `<build-root>/target/<workspace-name>-<workspace-path-hash>`.
3. The first writable `/Volumes/*/.rust-build-root` or
   `/Volumes/*/.codex-rs-build-root` marker directory.
4. The workspace `target/` directory.

To opt an external disk into automatic use:

```bash
external_disk=/Volumes/name-of-external-disk
mkdir -p "$external_disk/.codex-rs-build-root"
cargo-local --print-target-dir
```

Keep `sccache` configuration outside this repo. The wrapper does not start
long-lived cache processes on external disks, so a removable target volume can
be ejected after Cargo commands finish.

If rust-analyzer recreates `target/debug/`, check that VS Code opened the repo
root that contains `.vscode/settings.json`. The project settings override
rust-analyzer's Cargo commands with `cargo-local`; if those settings are not
loaded, rust-analyzer falls back to direct `cargo` commands.

For cleanup, prefer `cargo-sweep` when installed because it can remove stale
target artifacts without deleting every warm build product:

```bash
cargo-local sweep --dry-run --time 14
cargo-local sweep --time 14
```
