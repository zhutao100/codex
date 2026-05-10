# Local build cache

Use `scripts/cargo-local` for local Cargo loops that can create large debug
artifacts:

```bash
CODEX_SANDBOX_NETWORK_DISABLED=1 scripts/cargo-local check -p codex-cli --bin codex
CODEX_SANDBOX_NETWORK_DISABLED=1 scripts/cargo-local build -p codex-cli --bin codex --profile dev-small
CODEX_SANDBOX_NETWORK_DISABLED=1 scripts/cargo-local build -p codex-cli --bin codex --profile release-fast
CODEX_SANDBOX_NETWORK_DISABLED=1 scripts/cargo-local test -p codex-core --lib
```

Use `--profile release-fast` for release-like local smoke testing. Keep
`--release` for production packaging because the release profile intentionally
uses slower artifact-quality settings such as FatLTO.

The repo `just` recipes and `scripts/debug-codex.sh` call this wrapper.
The workspace VS Code rust-analyzer settings also call it for diagnostics,
build-script/proc-macro loading, and runnables.

Target-dir selection:

1. Existing `CARGO_TARGET_DIR`.
2. `CODEX_RS_BUILD_ROOT`, with target dirs stored under
   `$CODEX_RS_BUILD_ROOT/target/<workspace-name>-<workspace-path-hash>`.
3. The first writable `/Volumes/*/.codex-rs-build-root` marker directory.
4. The workspace `target/` directory.

To opt an external disk into automatic use:

```bash
external_disk=/Volumes/name-of-external-disk
mkdir -p "$external_disk/.codex-rs-build-root"
scripts/cargo-local --print-target-dir
```

Keep `sccache` configuration outside this repo. The wrapper does not start
long-lived cache processes on external disks, so a removable target volume can
be ejected after Cargo commands finish.

If rust-analyzer recreates `target/debug/`, check that VS Code opened the repo
root that contains `.vscode/settings.json`. The project settings override
rust-analyzer's Cargo commands with `./scripts/cargo-local`; if those settings
are not loaded, rust-analyzer falls back to direct `cargo` commands.

For cleanup, prefer `cargo-sweep` when installed because it can remove stale
target artifacts without deleting every warm build product:

```bash
scripts/cargo-local sweep --dry-run --time 14
scripts/cargo-local sweep --time 14
```
