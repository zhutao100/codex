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
- Put `$HOME/.cargo/bin` before Homebrew in `PATH` so both Cargo and rustc honor `rust-toolchain.toml`; selecting only the Cargo proxy can still leave Homebrew rustc in use.
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
- Keep `just-local test -p <crate>` scoped to the selected tests. Run `just-local bench-smoke` separately when benchmark validation is needed.
- During upstream rebases, port custom behavior to its current owner instead of restoring deleted crates or old APIs. Preserve upstream history envelopes, instruction provenance, atomic settings updates, and sanitized Git URLs.
- Model switches use upstream's incremental developer instructions and retain the original request instruction prefix; do not restore the retired session-wide base-instruction rewrite.
- Use `codex-prompts` rendering and reviewer `ToolPolicy` restrictions; do not restore retired personality-template metadata or session-identity-based tool dispatch.
- Preserve upstream's request history snapshots, async image resolution, and last usable server-usage boundary when updating token caches; do not restore the retired `prompt_history` helper.
- Formatting uses upstream's Git-indexed, batched `rustfmt` driver in `../scripts/format.py`; it no longer invokes Cargo and needs no wrapper override.
- See [customization maintenance](docs/rebase_customizations.md) for upstream replacement decisions and invariants to preserve.
- Keep model discovery's compile-time version override, `/models` query, and cache identity aligned; see [model client version](docs/model_client_version.md). Strip both prerelease and build metadata; do not substitute runtime package provenance for the compile-time contract.
