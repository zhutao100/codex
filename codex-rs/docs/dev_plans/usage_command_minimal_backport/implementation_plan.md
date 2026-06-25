# `/usage` Minimal Backport Implementation Plan

Status: Proposed

## Phase 1: Planning

- Inspect upstream from `c884536d8` and follow-up `/usage` commits.
- Compare the old branch architecture:
  - TUI already owns direct ChatGPT backend rate-limit fetches through `codex-backend-client`.
  - App-server v2 has `account/rateLimits/read`, but does not have `account/usage/read`.
  - The old TUI does not require app-server request routing for local slash-command output.
- Commit this plan before implementation.

Validation:

- `git diff --check -- docs/dev_plans/usage_command_minimal_backport`

## Phase 2: Minimal Feature Port

Backend client:

- Add hand-rolled `TokenUsageProfile`, `TokenUsageProfileStats`, and `TokenUsageProfileDailyBucket` response types.
- Add `Client::get_token_usage_profile()` using:
  - Codex API style: `/api/codex/profiles/me`
  - ChatGPT backend style: `/wham/profiles/me`
- Add path tests for the new endpoint.

TUI command:

- Add `SlashCommand::Usage`.
- Parse optional view arguments: empty/day/daily, week/weekly, cumulative.
- Require ChatGPT/Codex backend auth and show a typed-command error when unavailable.
- Start an asynchronous fetch and keep a pending token activity card above the composer until it can be committed without interrupting active output.
- Render loaded cards with summary metrics and daily/weekly/cumulative activity views.

Out of scope for this minimal pass:

- `/usage` menu with reset-credit redemption.
- App-server `account/usage/read` protocol/schema fixtures.
- Dynamic hiding of `/usage` from completion based on runtime auth state.
- Upstream terminal-palette and resize-reflow infrastructure.

Validation:

- `just fmt`
- `CODEX_SANDBOX_NETWORK_DISABLED=1 scripts/cargo-local test -p codex-backend-client`
- `CODEX_SANDBOX_NETWORK_DISABLED=1 scripts/cargo-local test -p codex-tui usage`
- `CODEX_SANDBOX_NETWORK_DISABLED=1 scripts/cargo-local test -p codex-tui token_activity`
- `just fix -p codex-backend-client`
- `just fix -p codex-tui`
- `cargo insta pending-snapshots -p codex-tui` if snapshot tests generate `*.snap.new`

## Phase 3: Completion

- Update this plan status to Completed.
- Record any intentional limitations that remain.
- Commit implementation with a Conventional Commit message.
