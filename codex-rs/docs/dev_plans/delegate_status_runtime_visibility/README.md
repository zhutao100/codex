# Delegate Status and Runtime Visibility

## Status

Proposed.

## Target Base

This proposal targets this project's customized `v0.98` branch shape.

## Purpose

The post-turn completion review delegate can run with an effective model, provider, sandbox, instruction stack, and context window that differ from the parent session. The delegate workflow already runs and records review lifecycle output, but the live status surfaces still describe the parent session. This directory proposes the protocol and UI/runtime-state changes needed to make `/status`, the bottom status line, and `codexd` subscribers report the active delegate accurately without treating the delegate as a full parent-session switch.

## Documents

- `problem_statement.md`: inspected behavior, code-path evidence, and root causes.
- `design_proposal.md`: recommended active runtime context design, alternatives, and non-goals.
- `codexd_protocol_notes.md`: focused protocol and state-machine changes for `codexd` and the TUI producer bridge.
- `implementation_plan.md`: phased implementation surfaces, tests, and acceptance criteria.
