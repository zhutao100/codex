# Implementation Plan

## Status

Proposed.

## Upstream reuse decisions

Before implementing the new runtime context layer, fold in the useful upstream project changes:

- Change `core/src/codex_delegate.rs::run_codex_thread_interactive(...)` and `run_codex_thread_one_shot(...)` to accept a `SubAgentSource`, matching the upstream helper shape, instead of hardcoding `SubAgentSource::Review`.
- Capture delegate runtime metadata close to the upstream `thread_config_snapshot()` call site, but emit it as a parent-visible runtime context event rather than only as analytics.
- Use upstream app-server v2 names and field shapes where they overlap with `codexd`: `turn/started`, `turn/completed`, `thread/tokenUsage/updated`, `thread/name/updated`, and `ThreadTokenUsage { total, last, modelContextWindow }`.
- Keep `post_turn_completion_review` as a task kind layered over `SubAgentSource::Review`; do not replace the low-level session source with the task label.
- Treat upstream detached review as an optional downstream-client workflow. It is not an acceptance criterion for inline TUI status correctness.

## Phase 1: Add runtime context data structures

Add protocol-safe data structures for active runtime status. Suggested names:

- `RuntimeContextSnapshot`.
- `RuntimeContextActivatedEvent`.
- `RuntimeContextUpdatedEvent`.
- `RuntimeContextDeactivatedEvent`.

The first implementation can specialize names to `DelegateSessionStarted`, `DelegateSessionUpdated`, and `DelegateSessionEnded` if that keeps the diff smaller. The important property is that these events are distinct from raw `SessionConfigured` and cannot accidentally reset the parent session.

Implementation surfaces:

- `protocol/src/protocol.rs`: event variants and serializable snapshot structs.
- `core/src/codex_delegate.rs`: construct and forward delegate runtime context lifecycle events; adopt the upstream `SubAgentSource` parameterized helper shape.
- `core/src/tasks/review.rs`: expose enough of the configured delegate profile to label task kind and instruction profile.
- `core/src/tasks/post_turn_completion_review.rs`: pass `post_turn_completion_review` task kind and parent reviewed turn id when available.

Capture these fields at minimum:

- scope id.
- scope kind.
- task kind.
- delegate session id.
- parent session id.
- parent turn id when applicable.
- model.
- model provider id.
- approval policy.
- sandbox policy.
- cwd.
- reasoning effort.
- service tier.
- model context window.
- agents summary or a status-renderable instruction-source summary.

## Phase 2: Stop dropping delegate status data without polluting parent state

Do not forward raw delegate `SessionConfigured` into the parent UI path. Instead:

- Convert delegate `SessionConfigured` into `RuntimeContextActivated`.
- Convert delegate `ThreadNameUpdated` into `RuntimeContextUpdated`.
- Convert delegate `TokenCount` into `RuntimeContextUpdated { token_info }` and, for `codexd`/app-server-compatible consumers, `thread/tokenUsage/updated`-shaped data.
- Preserve approval routing to the parent session.
- Preserve current content/progress event forwarding.
- Emit `RuntimeContextDeactivated` when the delegate turn ends or is shut down.

Add regression tests around `codex_delegate.rs::forward_events(...)` to assert that delegate `SessionConfigured` is not silently lost and is not forwarded as a primary `SessionConfigured`.

## Phase 3: Refactor TUI status rendering around status subjects

Add a `StatusSubjectSnapshot` or similar UI-level type. Build one primary subject from current parent state and one delegate subject from active runtime context.

Implementation surfaces:

- `tui/src/chatwidget.rs`: store active delegate context stack or single active delegate context.
- `tui/src/chatwidget.rs`: handle runtime context activated/updated/deactivated events.
- `tui/src/chatwidget.rs`: make `/status`, status-line model/context items, and bottom active model read from `active_status_subject()`.
- `tui/src/status/card.rs`: render from a status snapshot rather than a full `Config`.
- `tui/src/status_indicator_widget.rs`: no large change expected; it should receive the already-correct active model/reasoning values.

Expected behavior:

- During a post-turn review delegate, `/status` shows the delegate model/provider/sandbox/context and labels the scope as a delegate or post-turn review.
- The primary session header stays on the parent session unless an explicit nested badge is later added.
- After delegate completion, `/status` and status line return to parent session values.

## Phase 4: Extend `codexd` active turn state

Implementation surfaces:

- `codexd/src/protocol.rs`: optional active turn fields and optional context-update params.
- `codexd/src/daemon.rs`: key active turns by `turnKey`, not bare `turnId`; fold context updates into snapshots.
- `codexd/src/producer.rs`: no major API change required if generic notifications carry richer params.
- `tui/src/menubar_bridge.rs`: publish composite-key active turns and include delegate runtime context fields when present.
- `codexd/README.md`: document the richer active turn shape and compatibility behavior.

Add daemon tests for two simultaneous turns with the same `turnId` but different `threadId`. Both must appear in the same runtime snapshot and must complete independently.

## Phase 5: Tests

Core tests:

- A review delegate configured with `review_model` and `review_model_provider` emits an active runtime context using the delegate model/provider.
- A post-turn completion review delegate emits read-only sandbox and `approval = never` in the active runtime context.
- Delegate `TokenCount` becomes a delegate-scoped context update with the delegate model context window.
- Delegate completion emits a deactivation event even when the delegate is aborted or paused.

TUI tests:

- Given parent `SessionConfigured` with model A and delegate runtime context with model B, `/status` renders model B during the delegate and model A after deactivation.
- The bottom status indicator shows model B while delegate scope is active.
- Status-line context window uses the delegate context window while delegate scope is active.
- Parent session header and parent thread id are not overwritten by delegate context activation.

`codexd` tests:

- `turn/started` with `threadId = parent`, `turn.id = 0` and `threadId = delegate`, `turn.id = 0` creates two active turns.
- Delegate active turn snapshot includes `scope = delegate`, `sessionSource = subAgent`, `subAgentSource = review`, `taskKind = post_turn_completion_review`, model, provider, sandbox, and parent linkage when supplied.
- `turn/stateUpdated`, `runtime/contextUpdated`, or `thread/tokenUsage/updated` changes the active delegate turn context without requiring a new `turn/started`.
- Legacy clients that read only `threadId`, `turnId`, `status`, `model`, and `latestLabel` still receive valid values.

Manual acceptance scenario:

- Configure `review_model = "deepseek-v4-pro"` and `review_model_provider = "deepseek"`.
- Configure a `model_overlay.models` entry for `deepseek-v4-pro` with `context_window = 1048576` and `auto_compact_token_limit = 960000`.
- Complete a normal parent turn.
- Run `/review-completed-turn`.
- While the delegate is running, verify `/status` shows the DeepSeek review delegate, provider `deepseek`, read-only sandbox, no approval, delegate context window, and post-turn review scope.
- Verify the bottom status line uses the delegate model/context while the delegate runs.
- Verify `codexd` snapshot contains an active delegate turn with model/provider and parent linkage.
- After the delegate exits, verify all status surfaces return to the parent session.

## Rollout order

1. Land the runtime context event types and core delegate lifecycle translation.
2. Wire TUI active status subjects for `/status`, bottom indicator, and status line.
3. Add `codexd` optional fields and composite turn keys.
4. Add docs and tests for downstream consumers.
5. Re-run the manual DeepSeek delegate scenario and compare `/status`, footer, status line, and `codexd` snapshots before and after the change.

## Risks and mitigations

- Risk: raw delegate session events accidentally replace the parent session. Mitigation: never route raw delegate `SessionConfigured` to `on_session_configured`; use dedicated runtime context events.
- Risk: status code starts depending on full `Config` clones for delegates. Mitigation: introduce a compact renderable snapshot and keep `Config` out of status rendering after snapshot construction.
- Risk: `codexd` consumers break on new fields. Mitigation: make all new fields optional and preserve legacy fields.
- Risk: nested or concurrent delegates make a single active context ambiguous. Mitigation: use a stack or keyed map of active runtime contexts, with a deterministic active-subject policy such as most recently activated non-deactivated delegate.
