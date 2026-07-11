# `/usage` Backport

Status: Supported on `custom-0.98.0`

## Behavior

- `/usage` and `/usage daily` show daily account token activity.
- `/usage weekly` and `/usage cumulative` switch chart aggregation.
- Unsupported arguments report `Usage: /usage [daily|weekly|cumulative]`.
- Sessions without ChatGPT authentication report `Sign in with ChatGPT to use /usage.`
- `/status` remains the rate-limit and credits surface.

## Design

Reference: upstream `c884536d8 feat(tui): reland token activity command (#27925)`.

- Fetch `/api/codex/profiles/me` or `/wham/profiles/me` directly through `codex-backend-client`.
- Do not port app-server `account/usage/read`; the older TUI already owns direct backend account reads.
- Render the loading card above the composer without mutating transcript history.
- Commit a settled card directly into history when no active cell or answer/plan stream can be reordered.
- Treat agent and MCP task indicators as presentation state, not insertion barriers.
- Drain ready core events before committing settled output and hold their app-event history behind explicit barriers.
- Treat deferred exec, MCP, approval, and input events as insertion barriers until their queue is flushed.
- Queue terminal error, interruption, and pause history before retrying settled output insertion.
- Include loading and blocked-settled cards in the transcript overlay live tail.
- Allocate transient activity as flexible content so a short viewport preserves the composer.
- Use process-wide request IDs so responses from replaced widgets cannot complete a newer request.
- Pack summary fields to the visible graph width.

## Deferred Scope

- Reset-credit redemption from `f8f5a6e78` and `5c0fbf349`.
- App-server protocol and generated schema support for `account/usage/read`.
- Runtime auth-aware slash-command completion filtering.
- Newer terminal palette and resize-reflow infrastructure.

## Verification

- `CODEX_SANDBOX_NETWORK_DISABLED=1 cargo-local test -p codex-backend-client`
- `CODEX_SANDBOX_NETWORK_DISABLED=1 cargo-local test -p codex-tui`
- Authenticated `/usage` success path uses a localhost-backed profile response.
- App-level lifecycle coverage commits `TokenActivityLoaded` after queued core history and before later events.
- `CODEX_SANDBOX_NETWORK_DISABLED=1 just-local fix -p codex-backend-client`
- `CODEX_SANDBOX_NETWORK_DISABLED=1 just-local fix -p codex-tui`
