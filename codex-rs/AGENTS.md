# Working in Codex

## Start here

Read [project principles](docs/principles.md) before changing the agent loop, context construction, persistence or transport. Use the [documentation index](docs/README.md) to load only the feature contracts relevant to the task. Check for more specific `AGENTS.md` instructions in the area being edited.

## Project invariants

- The harness owns task scheduling, tool execution, approvals, context and recovery. Keep model requests, UI events and durable host metadata distinct.
- Preserve ordered history, tool-call/result pairing and opaque reasoning, compaction and [sampled-output replay](docs/features/responses-output-replay.md) data. Keep [request prefixes](docs/features/responses-websocket.md#activation-and-wire-contract) and tool order stable; check [capture and compaction prefixes](docs/features/session-work-notes.md#fit-and-prefix-reuse) against actual request items alongside [review budgets](docs/features/post_turn_completion_review/budget_accounting.md) and transport continuation.
- Accepted input, consumed input and a completed answer have different owners and lifetimes. Changes to pause, steering, compaction or review must preserve those distinctions through cancellation and replay.
- Local history and durable checkpoints must support recovery when an API connection disappears. A WebSocket continuation ID is not a session checkpoint; see the [context harness contract](docs/features/context-harness.md) for local evidence retained outside history.
- Use checked-in implementation and tests to establish behavior. Check [feature controls](docs/features/feature-controls.md) before restoring a retired flag; a newer registry or tool description does not establish support here. Refresh or rebase upstream only when requested.

## Development

Use `cargo-local` for local Cargo build/check/test/run commands and `just-local` for recipes that invoke Cargo. The repository shim requires a separately installed shared wrapper; see [build and test guidance](docs/development.md) for setup, profiles and scoped checks.

Set `CODEX_SANDBOX_NETWORK_DISABLED=1` for tests. Use the existing localhost-aware guard for mock transport tests and let the wrapper declare that capability. Do not count a skipped network test as transport coverage. Capture noisy output outside the repository and report the relevant result and limitations.

Match the check to the contract: recorder/replay for durable history, captured requests/events for transport, disposable child processes for sandbox policy (including [ordinary and packaged Linux launchers](linux-sandbox/README.md)), and live evaluation for model behavior. Documentation-only edits need link, format and source-consistency checks, not a workspace rebuild.

## Documentation maintenance

- Keep this file a short gateway. Put feature rules, decisions and test entry points in the relevant doc and link them from an index; replace superseded guidance instead of appending another rule.
- Store durable, shareable knowledge: contracts, reasons for non-obvious choices, limitations and actionable unfinished work. Omit directory inventories, branch/worktree comparisons, machine paths, run logs and facts easily read from code. A targeted source link is useful when it explains where to verify a contract.
- Update affected docs with behavior changes. Separate implemented behavior from proposals; use source/schema links instead of manually duplicating complete field or default lists.
- Keep active plans current with status, decisions and remaining work. When finished, retain the useful contract in `docs/features/` and remove the plan; delete abandoned material. Update incoming links when moving or deleting a document.
- Never auto-wrap Markdown prose or pad tables. Keep each paragraph/list item on one source line, use compact GFM tables, and leave blank lines around headings and lists.
