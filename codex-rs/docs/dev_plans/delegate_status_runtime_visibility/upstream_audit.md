# Upstream Audit

## Status

Inspected against the upstream project.

## Summary

The upstream project has not implemented a complete inline delegate runtime-status fix. It still runs review work in a separate `Codex` sub-agent session, while `core/src/codex_delegate.rs::forward_events(...)` filters delegate `SessionConfigured` and `TokenCount` events before they can update parent-visible status surfaces. This branch additionally filters delegate `ThreadNameUpdated`; upstream moved thread-name delivery into app-server notifications, but that does not create a parent-visible delegate runtime context.

The upstream project does include reusable components that this project should align with instead of inventing incompatible runtime vocabulary:

- app-server v2 thread/turn lifecycle notifications.
- thread-scoped token-usage notifications.
- `SessionSource::SubAgent(...)` and richer `SubAgentSource` metadata.
- detached review delivery for app-server clients.
- analytics-side `ThreadConfigSnapshot` capture for sub-agent session start.

The conclusion is therefore mixed: the upstream project does not remove the need for this proposal, but it provides protocol shapes and naming conventions that should be reused where possible.

## Upstream evidence

| Area | Upstream project status | Implication for this project |
| --- | --- | --- |
| Delegate runtime boundary | `core/src/codex_delegate.rs::run_codex_thread_interactive(...)` accepts a `SubAgentSource`, spawns the delegate with `SessionSource::SubAgent(subagent_source.clone())`, and records a sub-agent analytics event from `codex.thread_config_snapshot().await`. | Reuse the parameterized `SubAgentSource` pattern and consider the thread-config snapshot as a seed for a renderable runtime snapshot. Do not rely on the analytics emission as the UI/status solution. |
| Delegate event forwarding | `core/src/codex_delegate.rs::forward_events(...)` still drops `EventMsg::SessionConfigured(_)` and `EventMsg::TokenCount(_)`. This branch also drops `EventMsg::ThreadNameUpdated(_)`; upstream no longer exposes that as a core delegate event. | The same root cause remains for inline delegates. This project still needs an explicit delegate runtime-context event path for session, token, and name/title state. |
| Review delegate setup | `core/src/tasks/review.rs` still builds a review sub-agent config, applies `review_model` where configured, forces review-oriented restrictions such as `approval_policy = never`, and invokes `run_codex_thread_one_shot(..., SubAgentSource::Review, ...)`. | Upstream confirms the review delegate is a real sub-session with runtime settings that can differ from the parent. Status surfaces must not infer review state from the parent `Config`. |
| TUI `/status` card | `tui/src/chatwidget.rs::add_status_output(...)` still reads `self.config`, `self.thread_id`, `self.thread_name`, `self.token_info`, `self.current_model()`, and `self.model_display_name()`. `tui/src/status/card.rs` still renders provider, approval, sandbox, and cwd from `&Config`. | The upstream TUI status card is still parent-session-centric. This project should refactor status rendering around a status-subject snapshot. |
| Bottom/status-line context | `tui/src/chatwidget.rs::on_task_started()` receives no runtime payload, and `status_line_context_window_size()` still derives context size from `self.token_info` or `self.config.model_context_window`. | A forwarded delegate `TurnStarted` alone is insufficient. Delegate model/provider/context-window data must be carried by an explicit active runtime context. |
| App-server v2 protocol | The upstream v2 protocol module defines `ThreadStartedNotification`, `ThreadStatusChangedNotification`, `TurnStartedNotification`, `TurnCompletedNotification`, `ThreadNameUpdatedNotification`, `ThreadTokenUsageUpdatedNotification`, `ThreadTokenUsage`, `Thread`, `Turn`, and `SessionSource` under `app-server-protocol/src/protocol/v2/`. | Use these names and shapes as the compatibility baseline for `codexd` notifications and snapshots where they overlap. |
| Token usage bridge | `app-server/src/bespoke_event_handling.rs` converts top-level `TokenCount` into `thread/tokenUsage/updated`, keyed by `thread_id` and `turn_id`. | Reuse this thread-scoped token-usage concept for delegate context updates. It still requires core to convert or forward delegate token events instead of dropping them. |
| Detached review | `app-server/src/request_processors/turn_processor.rs` supports `review/start` with detached delivery, creating a separate review thread and emitting `thread/started`. The TUI path still requests inline delivery. | Detached review is a useful downstream-client option, but it is not a complete fix for inline `/review` or `/review-completed-turn` status. |
| `codexd` | No upstream `codexd/` module is present. | `codexd` remains this project's responsibility, but its protocol should align with upstream app-server v2 concepts. |

## Reuse decisions

### 1. Keep `RuntimeContextSnapshot`, but align its source fields with upstream

`RuntimeContextSnapshot` should continue to be a dedicated parent-visible delegate-status object. The source and task fields should reuse upstream terminology:

- `sessionSource = "subAgent"` or equivalent typed representation.
- `subAgentSource = "review"`, `"threadSpawn"`, `"compact"`, or other upstream-compatible values.
- `taskKind = "post_turn_completion_review"` for this project's post-turn review overlay on top of `SubAgentSource::Review`.

This avoids treating `post_turn_completion_review` as a replacement for the lower-level session source.

### 2. Mirror app-server v2 notification names where possible

For `codexd`, prefer upstream-compatible names and payload concepts for overlapping events:

- `thread/started` for a new observable session/thread.
- `turn/started` and `turn/completed` for turn lifecycle.
- `thread/tokenUsage/updated` for token and context-window changes.
- `thread/name/updated` for thread title/name changes.

A `turn/contextUpdated` or `runtime/contextUpdated` notification is still reasonable for fields not covered by app-server v2, such as sandbox, approval policy, instruction summary, and parent/delegate linkage. It should not duplicate token usage if `thread/tokenUsage/updated` can carry it.

### 3. Use composite turn identity everywhere outside a single session

Upstream app-server v2 notifications always carry `thread_id` when reporting turn lifecycle or token usage. This project should preserve the existing `turnKey = "${threadId}:${turnId}"` proposal for `codexd` and avoid bare `turnId` de-duplication.

### 4. Treat detached review as complementary, not a substitute

Detached review can help external app-server clients observe review work as a separate thread. It does not solve the TUI inline delegate problem because:

- the TUI still uses inline delivery for review start.
- `/review-completed-turn` is a this-project workflow, not an upstream detached-review workflow.
- the nested delegate still filters its own `SessionConfigured` and `TokenCount` events, and this branch also filters its core `ThreadNameUpdated` event.
- the parent TUI still needs to show a temporary active delegate context without replacing the parent session.

### 5. Port the safer upstream delegate API shape

This project's `run_codex_thread_interactive(...)` currently hardcodes `SessionSource::SubAgent(SubAgentSource::Review)`. Upstream passes `SubAgentSource` into both interactive and one-shot helpers. The implementation should adopt that shape before adding more delegate types or runtime-context labels.

## Upstream status conclusion

The upstream project still has the inline delegate status/runtime visibility gap. The useful upstream work is not a direct fix, but a set of protocol and API conventions to align with. The proposal remains necessary; the implementation should reuse upstream app-server v2 lifecycle/token naming, `SessionSource::SubAgent(...)` classification, and composite thread/turn identity semantics.
