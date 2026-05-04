# Local build cache

Use `scripts/cargo-local` for local Cargo loops that can create large debug
artifacts:

```bash
CODEX_SANDBOX_NETWORK_DISABLED=1 scripts/cargo-local check -p codex-cli --bin codex
CODEX_SANDBOX_NETWORK_DISABLED=1 scripts/cargo-local build -p codex-cli --bin codex --profile dev-small
CODEX_SANDBOX_NETWORK_DISABLED=1 scripts/cargo-local test -p codex-core --lib
```

The repo `just` recipes and `scripts/debug-codex.sh` call this wrapper.

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

For cleanup, prefer `cargo-sweep` when installed because it can remove stale
target artifacts without deleting every warm build product:

```bash
scripts/cargo-local sweep --dry-run --time 14
scripts/cargo-local sweep --time 14
```
