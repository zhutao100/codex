# `/usage` Upstream Audit

Status: Proposed

## Reference Commits

- `c884536d8 feat(tui): reland token activity command (#27925)`
  - Adds `/usage`, `/usage daily`, `/usage weekly`, and `/usage cumulative`.
  - Fetches token activity through app-server `account/usage/read`.
  - Adds async token-activity cards, chart rendering, palette selection, and ordering safeguards.
- `f8f5a6e78 feat(tui): add rate-limit reset redemption to /usage (#28154)`
  - Extends `/usage` into a menu that can consume usage-limit reset credits.
- `5c0fbf349 [codex] Fix usage-limit reset copy and state (#28793)`
  - Fixes reset-credit UI state and copy.

## Old Branch Inventory

Present:

- `tui/src/slash_command.rs` static slash-command list.
- `tui/src/chatwidget/slash_dispatch.rs` command dispatch.
- `tui/src/chatwidget/rate_limits.rs` direct backend fetch for ChatGPT rate limits.
- `tui/src/status/*` token and rate-limit formatting helpers.
- `codex-backend-client::Client::get_rate_limits()`.

Missing:

- App-server `ClientRequest::GetAccountTokenUsage`.
- `GetAccountTokenUsageResponse` protocol/schema types.
- Backend-client token-usage profile response models and endpoint method.
- TUI token-activity chart/card modules.
- Runtime auth-aware slash-command filtering.
- Reset-credit backend endpoints and `/usage` menu.

## Minimal Dependency Decision

Do not port app-server `account/usage/read` as a prerequisite for the old branch TUI. The old TUI already fetches account rate limits directly through `codex-backend-client`, so direct token-profile fetches are the smaller dependency surface.

Required prerequisite scope:

- Add only backend-client response types and `Client::get_token_usage_profile()`.

Deferred prerequisite scope:

- App-server request/response protocol additions.
- Generated TypeScript/schema fixture updates.
- Reset-credit consume APIs and menu state.

## Behavioral Compatibility Target

The minimal backport should match the user-visible token-activity command shape:

- `/usage` defaults to daily view.
- `/usage daily`, `/usage weekly`, and `/usage cumulative` render account activity.
- Unsupported view arguments produce `Usage: /usage [daily|weekly|cumulative]`.
- Signed-out/API-key-only sessions produce `Sign in with ChatGPT to use /usage.`

The output may be simpler than upstream but should preserve:

- Summary metrics.
- Last-12-month daily bucket display.
- Width-bounded rendering.
- Nonblocking fetch behavior.
