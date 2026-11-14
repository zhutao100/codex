# Account token activity

## Behavior

|Command|Result|
|---|---|
|`/usage` or `/usage daily`|Daily account token activity|
|`/usage weekly`|Weekly aggregation|
|`/usage cumulative`|Cumulative aggregation|
|Unsupported argument|`Usage: /usage [daily\|weekly\|cumulative]`|

ChatGPT authentication is required; API-key sessions receive `Sign in with ChatGPT to use /usage.` `/status` remains the rate-limit and credits surface.

## Rendering and ordering contract

The direct-core TUI fetches `/api/codex/profiles/me` or `/wham/profiles/me` through [the backend client](../../backend-client/src/client.rs). The loading card is transient UI content above the composer; it must not rewrite the model conversation.

- Commit a settled card to visible history only when active cells, answer/plan streams and queued exec/MCP/approval/input events cannot be reordered. Drain ready core events and respect explicit app-event barriers.
- Task indicators alone are not insertion barriers. Queue terminal error, interruption and pause history before retrying insertion.
- Include loading and blocked settled cards in the transcript overlay's live tail. Keep the composer visible in a short viewport and fit summary fields to graph width.
- Use process-wide request IDs so a late response for a replaced widget cannot complete a newer request.

[Widget state](../../tui/src/chatwidget/tokens.rs) owns transient/settled cards; [app integration](../../tui/src/app.rs) owns queued-history ordering. Preserve this division when changing presentation.

## Scope and tests

The feature does not add an app-server `account/usage/read` RPC or reset-credit redemption. Do not assume those surfaces exist because another client supports them.

Use the backend-client URL/response tests, [token-card tests](../../tui/src/chatwidget/tokens_tests.rs), chatwidget tests and `token_activity_app_event_commits_after_queued_active_thread_history` in the app tests. Localhost fixture success covers the deterministic authenticated response path, not a live account service call. See [scoped validation](../development.md#scoped-validation).
