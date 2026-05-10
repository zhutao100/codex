# Cargo Build Optimization

## Status

Consolidated proposal. Partially implemented in this branch.

Implemented inputs already present in this branch:

- native async `ToolHandler` with a private object-safe `AnyToolHandler` adapter;
- native async `SessionTask` with a private object-safe `AnySessionTask` adapter;
- local dev profiles in the workspace root `Cargo.toml`;
- production release `split-debuginfo = "off"`;
- a `release-fast` profile for release-like developer builds.
- a repeatable measurement script, `measure.sh`, that uses the repo-local Cargo
  wrapper and writes absolute-path-heavy captures outside the repository.

Remaining scope is the larger `cargo build --release --bin codex` latency problem caused by the final `codex` binary's dependency closure and production release profile.

## Target Base

This proposal targets this project's customized branch shape. The upstream project is used only as a reference for design direction and low-risk optimization patterns.

## Purpose

The earlier compile-time optimization work reduced `codex-core` rebuild cost. The release build pain point is broader: `cargo build --release --bin codex` must compile and link the `codex-cli` binary, whose entrypoint directly includes interactive TUI, non-interactive exec/review, MCP server, app-server tooling, codexd management, cloud tasks, sandbox helpers, hidden proxy helpers, and feature tooling.

This directory separates the two problem classes:

1. compiler hot spots inside high-fanout crates, especially `codex-core`;
2. final binary dependency-closure and link-time cost for the multipurpose `codex` executable.

## Documents

- `problem_statement.md`: consolidated problem statements, source validation, and corrected interpretation of the observed build output.
- `design_proposal.md`: proposed optimization strategy, including already-landed items and remaining work.
- `implementation_plan.md`: sequencing, commands, acceptance criteria, and rollback notes.
