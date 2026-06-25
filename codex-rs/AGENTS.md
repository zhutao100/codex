When you see this instruction, you're in a customized fork repo branch.

# Changes convention

Changes in this branch are drop-in diff commits staying on top of the upstream branch.

The long-term maintenance pattern is: over time, keep calling `git pull origin main` to rebase the branch commits on top of the latest upstream code.

Thus, the design of the changes target to minimize the potentials of future rebase conflicts.

# Agents operation tips
- Always use the shared `cargo-local` wrapper instead of direct `cargo` for
  local build/check/test/run commands. It uses an external target dir when an agent-created
  `/Volumes/*/.codex-rs-build-root` marker directory is mounted, and otherwise
  falls back to the workspace `target/`.
- Use `just-local` instead of direct `just` in Rust projects whose recipes call
  `cargo` directly; it adds a per-invocation `cargo` shim that delegates to
  `cargo-local`.
- always use `CODEX_SANDBOX_NETWORK_DISABLED=1` for test runs to avoid tampering the host.
- With `CODEX_SANDBOX_NETWORK_DISABLED=1` on macOS, `cargo-local`
  wraps Cargo in a Seatbelt profile that allows localhost sockets only and
  filters tests listed in `scripts/cargo-local-network-sandbox-skips.txt`
  because they exercise nested sandbox/shell behavior.
  It also reuses cached WebRTC/V8 native artifacts from local target dirs when
  available and sets an 8 MiB test stack by default, so broad test runs do not
  download build-script artifacts or overflow app-server test worker stacks.
- `cargo-local test` defaults to agent output: it filters passing libtest
  progress into a compact summary while preserving warnings and failures. Use
  `CODEX_RS_CARGO_LOCAL_TEST_OUTPUT=raw` only when exact Cargo/libtest output is
  needed.
- The `cargo build` commands can produce giant console output, even with
  `--quiet`. Redirect broad/noisy build output to tmp files and parse only
  necessary information.
