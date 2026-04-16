# Context

## Codex TUI diff rendering glitch

Understand the Codex TUI diff rendering color issue from the recent user reports,
- https://github.com/openai/codex/issues/15416
- https://github.com/openai/codex/issues/12904
- https://github.com/openai/codex/issues/12912

Per user feedbacks, the glitch started from the release tag `rust-v0.105.0`.

### Codex TUI transparent / configurable diff background proposals

Regarding the issue, some users have made desired proposals
- https://github.com/openai/codex/issues/14661
- https://github.com/openai/codex/issues/12749

### Merged PR

There's a relevant PR merged into the canonical `openai/codex` repo, since release tag `rust-v0.107.0`. 
- PR: `https://github.com/openai/codex/pull/13037`
- Merged commit: `https://github.com/openai/codex/commit/c3c7587`

However, looks this PR does not resolve the color rendering glitch for Codex TUI running in MacOS terminal app.

### Open PR from forked repo

There's a wild PR "add configurable diff backgrounds" living in a forked `ignatremizov/codex` repo, 
- PR: `https://github.com/ignatremizov/codex/pull/1/`
- Direct `.patch` file URL: `https://patch-diff.githubusercontent.com/raw/ignatremizov/codex/pull/1.patch`
