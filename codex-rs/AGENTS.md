When you see this instruction, you're in a customized fork repo branch.

# Changes convention

Changes in this branch are drop-in diff commits staying on top of the upstream branch.

The long-term maintenance pattern is: over time, keep calling `git pull origin main` to rebase the branch commits on top of the latest upstream code.

Thus, the design of the changes target to minimize the potentials of future rebase conflicts.

# Agents operation tips
- Use `scripts/cargo-local` for first-use wrapper installation and `cargo-local` instead of direct `cargo` for local builds and tests. See [local verification](docs/rebase_customizations.md#local-verification) for dependency preparation and cache maintenance.
- Use `just-local` instead of direct `just` in Rust projects whose recipes call
  `cargo` directly; it adds a per-invocation `cargo` shim that delegates to
  `cargo-local`.
- The shared wrapper prefers Rustup's Cargo and rustc so both honor `rust-toolchain.toml`.
- always use `CODEX_SANDBOX_NETWORK_DISABLED=1` for test runs to avoid tampering the host.
- With `CODEX_SANDBOX_NETWORK_DISABLED=1` on macOS, `cargo-local` uses localhost-only Seatbelt and applies `scripts/cargo-local-network-sandbox-skips.txt` for nested sandbox and shell tests. It reuses cached WebRTC artifacts when available and defaults to an 8 MiB test stack for deeply nested app-server tests.
- `cargo-local test` defaults to compact output and automatic dependency recovery. Use `CODEX_RS_CARGO_LOCAL_TEST_OUTPUT=raw` only for exact output; it does not recover missing dependencies.
- The `cargo build` commands can produce giant console output, even with
  `--quiet`. Redirect broad/noisy build output to tmp files and parse only
  necessary information.
- Keep `just-local test -p <crate>` scoped to the selected tests. Run `just-local bench-smoke` separately when benchmark validation is needed.
- During upstream rebases, port custom behavior to its current owner instead of restoring deleted crates or old APIs. Preserve upstream history envelopes, instruction provenance, atomic settings updates, and sanitized Git URLs.
- Model switches use upstream's incremental developer instructions and retain the original request instruction prefix; do not restore the retired session-wide base-instruction rewrite.
- Use `codex-prompts` rendering and reviewer `ToolPolicy` restrictions; do not restore retired personality-template metadata or session-identity-based tool dispatch.
- Preserve upstream's request history snapshots, async media resolution, and last usable server-usage boundary when updating token caches. Rebuild estimates when measured audio durations change; keep live rollout appends and realtime durability barriers.
- Formatting uses upstream's Git-indexed, batched `rustfmt` driver in `../scripts/format.py`; it no longer invokes Cargo and needs no wrapper override.
- See [customization maintenance](docs/rebase_customizations.md) for upstream replacement decisions and invariants to preserve.
- Keep model discovery's compile-time version override, `/models` query, and cache identity aligned; see [model client version](docs/model_client_version.md). Strip both prerelease and build metadata; do not substitute runtime package provenance for the compile-time contract.
