# Problem Statement

## Status

Proposed.

## Target Base

This proposal targets this project's customized `v0.98` branch shape.

## Context

The review delegate workflow starts in `core/src/codex_delegate.rs`. A delegate is spawned as a separate `Codex` session with `SessionSource::SubAgent(SubAgentSource::Review)` and independent session state. Post-turn completion review constructs its delegate config in `core/src/tasks/post_turn_completion_review.rs` by calling `configure_review_delegate_config(...)` from `core/src/tasks/review.rs`.

The delegate config intentionally differs from the parent session:

- It may use `review_model` instead of the parent model.
- It may use `review_model_provider` instead of the parent provider.
- It applies review-specific instruction profiles such as `AGENTS.post-turn-review.md`, `AGENTS.review.md`, and `AGENTS.md` fallback ordering.
- It forces `approval_policy = never` and a read-only sandbox for the post-turn review delegate.
- It disables collaboration mode and web search for the review delegate.
- Its effective `ModelInfo` can come from `model_overlay`, including custom context-window and auto-compaction metadata.

The observed behavior is therefore internally plausible: the delegate runs correctly, but the live status surfaces describe the parent session because those surfaces are still keyed by parent-session state.

## User-visible symptoms

During `/review-completed-turn`, with `review_model = "deepseek-v4-pro"`, `review_model_provider = "deepseek"`, and a `model_overlay` entry for that slug:

- `/status` shows the parent session model, provider, sandbox, `AGENTS.md` summary, session id, and context window.
- The bottom status indicator/status line shows the parent model and context status.
- `codexd` downstream applications do not reliably show the delegate workflow as an active delegate turn, and when they do see a turn they can inherit the parent model/provider metadata.

The functional review path still works because delegate content events are forwarded, parsed, and recorded, and because enter/result/exit events are written through the parent session. That does not imply the UI has a correct live status context for the delegate.

## Root causes

### 1. The delegate is a separate session, but the parent UI has no delegate session context

`run_codex_thread_interactive(...)` calls `Codex::spawn(...)` with `SessionSource::SubAgent(SubAgentSource::Review)`. This creates a separate session, event stream, status object, model info, context window, rollout path, and thread id.

The returned `Codex` wrapper exposes only the delegate IO channels and status handle to the caller. There is no explicit parent-visible runtime context object that says: "a delegate is now active; here is its session id, task kind, model, provider, sandbox, instruction summary, cwd, context window, and parent linkage."

### 2. Delegate `SessionConfigured` and `TokenCount` are explicitly filtered out

The delegate session emits a normal `SessionConfiguredEvent` in `core/src/codex.rs` and normal `TokenCount` events through the regular session pipeline. However, `core/src/codex_delegate.rs::forward_events(...)` drops both event kinds before they reach the post-turn review event processor:

- `EventMsg::SessionConfigured(_)` is ignored.
- `EventMsg::TokenCount(_)` is ignored.
- `EventMsg::ThreadNameUpdated(_)` is ignored.

This explains the mismatch precisely. The content/progress stream can reach the parent UI, while the events that describe the delegate's effective session and token context are withheld.

Forwarding raw delegate `SessionConfigured` as-is would be dangerous because `tui/src/chatwidget.rs::on_session_configured(...)` treats it as a primary session switch: it replaces the thread id, rollout path, cwd, header model, current collaboration mode, history metadata, and session-info cell. The missing abstraction is not "forward raw `SessionConfigured`"; it is "forward a delegate-scoped runtime context."

### 3. `/status` is rendered from parent `ChatWidget` config/state

`ChatWidget::add_status_output(...)` builds the status card from `self.config`, `self.thread_id`, `self.thread_name`, `self.token_info`, `self.model_display_name()`, and `self.effective_reasoning_effort()`. `status/card.rs` then derives model provider, approval policy, sandbox policy, directory, and `Agents.md` summary from that parent `Config`.

The status card has no way to render an active delegate config because it accepts a `Config`, not a status-context snapshot. It also has only one token-info slot, currently representing the parent session.

### 4. The bottom status indicator uses parent model selection when a delegate turn starts

`ChatWidget::on_task_started(...)` does not receive or inspect the `TurnStartedEvent` payload. If `running_turn_model` is empty, it assigns `self.current_model()` and `self.effective_reasoning_effort()`, both derived from the parent UI collaboration mode.

Even if the delegate `TurnStartedEvent` is forwarded, the current payload is insufficient for model/provider display. `TurnStartedEvent` only contains `model_context_window` and `collaboration_mode_kind`; it does not include model slug, display name, provider id, sandbox, approval, task kind, parent thread, or delegate session id.

### 5. The status line context-window logic reads parent token/config state

`status_line_context_window_size()` uses `self.token_info.model_context_window` or `self.config.model_context_window`. Since delegate `TokenCount` events are filtered, `self.token_info` remains the parent token state. If the delegate model context window comes from `model_overlay`, the status line has no active delegate token event or delegate model info from which to derive the custom window.

### 6. `codexd` is process/runtime scoped and its active-turn schema is too thin

`codexd/src/protocol.rs::ActiveTurnSnapshot` currently contains `threadId`, `turnId`, optional `status`, optional `startedAt`, optional `model`, and optional `latestLabel`. It lacks provider, sandbox, approval, context window, context usage, task kind, parent/delegate relation, per-turn session source, cwd override, and instruction-stack summary.

`codexd/src/daemon.rs` updates `activeTurns` only by interpreting `turn/started` and `turn/completed`. It keys the daemon-side active turn map by bare `turnId`. That is fragile for nested sessions because parent and delegate sessions can independently generate the same small turn ids.

The TUI `MenuBarBridge` has the same shape issue. It stores `current_model` and `current_model_provider` only from visible `SessionConfigured` events, and delegate `SessionConfigured` is filtered. It also deduplicates by bare `turn_id`, so a delegate turn with the same id as an existing parent turn can be suppressed instead of appearing as a distinct active turn.

### 7. Event forwarding is optimized for content rendering, not runtime-state monitoring

`process_post_turn_completion_review_events(...)` forwards most delegate events to the parent via `send_event_transient(...)`, while suppressing messages that would duplicate the rendered final review output. This works for live content and progress but does not establish a durable or queryable active-runtime state.

The current design therefore has two separate paths:

- Content path: delegate output/progress can stream into the parent UI.
- Runtime-state path: parent `/status`, bottom status, status line, and `codexd` still read parent session state.

The fix should connect the second path without regressing the first.

## Upstream status

The upstream project was inspected after this proposal was drafted. It does not remove the root cause for inline delegate visibility:

- `core/src/codex_delegate.rs::forward_events(...)` still drops delegate `SessionConfigured` and `TokenCount` events. This branch also drops delegate `ThreadNameUpdated`; upstream no longer has that core event shape, but its app-server thread-name notification is still not a delegate runtime-context path.
- `tui/src/chatwidget.rs::add_status_output(...)` and `tui/src/status/card.rs` still render `/status` from the parent `ChatWidget` state and parent `Config`.
- `tui/src/chatwidget.rs::on_task_started()` still has no delegate runtime payload from which to display the active model/provider.
- `status_line_context_window_size()` still derives its context window from parent token/config state.
- The upstream project has no `codexd/` module, so `codexd`-specific visibility remains this project's responsibility.

The upstream project does provide useful components to reuse: app-server v2 thread/turn lifecycle notifications, `thread/tokenUsage/updated`, `thread/name/updated`, `SessionSource::SubAgent(...)`, richer `SubAgentSource` values, and detached review delivery for app-server clients. These are compatibility and naming inputs, not a replacement for an explicit active runtime context.

## Design requirements

- Preserve the parent session as the primary conversation; do not treat a delegate `SessionConfigured` as a top-level session switch.
- Provide a delegate-scoped status context while the delegate is active.
- Keep raw delegate content/progress forwarding behavior intact.
- Support custom review providers and custom model-overlay metadata.
- Make `codexd` active turns globally distinguishable by thread/session plus turn id, not by bare turn id.
- Allow downstream apps to distinguish primary turns from delegate turns and to display the delegate's effective runtime information.
- Return status surfaces to the parent session after the delegate exits.
