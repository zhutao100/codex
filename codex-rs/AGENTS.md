When you see this instruction, you're in a customized fork repo branch.

# Changes convention

Changes in this branch are drop-in diff commits staying on top of the upstream branch.

The long-term maintenance pattern is: over time, keep calling `git pull origin main` to rebase the branch commits on top of the latest upstream code.

Thus, the design of the changes target to minimize the potentials of future rebase conflicts.

# Agents operation tips

- Always use the shared `cargo-local` wrapper instead of direct `cargo` for local build/check/test/run commands. It uses an external target dir when an agent-created `/Volumes/*/.codex-rs-build-root` marker directory is mounted, and otherwise falls back to the workspace `target/`.
- Use `just-local` instead of direct `just` in Rust projects whose recipes call `cargo` directly; it adds a per-invocation `cargo` shim that delegates to `cargo-local`.
- always use `CODEX_SANDBOX_NETWORK_DISABLED=1` for test runs to avoid tampering the host.
- With `CODEX_SANDBOX_NETWORK_DISABLED=1` on macOS, `cargo-local` wraps Cargo in a Seatbelt profile that allows localhost sockets only and filters tests listed in `scripts/cargo-local-network-sandbox-skips.txt` because they exercise nested sandbox/shell behavior.
- `cargo-local test` defaults to agent output: it filters passing libtest progress, passing-test subprocess chatter, and legacy snapshot notices into a compact summary while preserving warnings and failures. Prefer `-- --show-output` for diagnostic output while retaining repository test exclusions; the shared wrapper's `CODEX_RS_CARGO_LOCAL_TEST_OUTPUT=raw` mode currently bypasses those exclusions.
- The `cargo build` commands can produce giant console output, even with `--quiet`. Redirect broad/noisy build output to tmp files and parse only necessary information.
- For mock-only HTTP/WebSocket tests, use the existing `skip_if_no_network_unless_localhost!` guard. Let the wrapper declare localhost capability; never forge its flag or count an early-return network test as executed transport coverage.
- See [streaming remote compaction](docs/features/remote_compaction_v2/README.md) before changing compaction, work notes, replay or client continuation. Keep triggers request-only, notes/snapshots exact, and installation coherent with task cancellation. Explicit unary fixtures must disable `RemoteCompactionV2`.
