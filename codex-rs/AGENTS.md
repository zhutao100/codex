When you see this instruction, you're in a customized fork repo branch.

# Changes convention

Changes in this branch are drop-in diff commits staying on top of the upstream branch. 

The long-term maintenance pattern is: over time, keep calling `git pull origin main` to rebase the branch commits on top of the latest upstream code.

Thus, the design of the changes target to minimize the potentials of future rebase conflicts.

# Agents operation tips
- Prefer `scripts/cargo-local` over direct `cargo` for local build/check/test/run
  commands. It uses an external target dir when an agent-created
  `/Volumes/*/.codex-rs-build-root` marker directory is mounted, and otherwise
  falls back to the workspace `target/`.
- always use `CODEX_SANDBOX_NETWORK_DISABLED=1` for test runs to avoid tampering the host.
- The `cargo build` `cargo test` commands produce giant console output, even with `--quiet` flag. Always redirect the output to tmp files and only parse necessary information.
