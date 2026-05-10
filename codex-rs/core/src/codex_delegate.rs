use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;

use async_channel::Receiver;
use async_channel::Sender;
use codex_async_utils::OrCancelExt;
use codex_protocol::ThreadId;
use codex_protocol::protocol::ApplyPatchApprovalRequestEvent;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ExecApprovalRequestEvent;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::RequestUserInputEvent;
use codex_protocol::protocol::RuntimeContextActivatedEvent;
use codex_protocol::protocol::RuntimeContextDeactivatedEvent;
use codex_protocol::protocol::RuntimeContextScope;
use codex_protocol::protocol::RuntimeContextSnapshot;
use codex_protocol::protocol::RuntimeContextUpdatedEvent;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::protocol::Submission;
use codex_protocol::protocol::ThreadNameUpdatedEvent;
use codex_protocol::protocol::TokenCountEvent;
use codex_protocol::protocol::TurnStartedEvent;
use codex_protocol::request_user_input::RequestUserInputArgs;
use codex_protocol::request_user_input::RequestUserInputResponse;
use codex_protocol::user_input::UserInput;
use std::time::Duration;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

use crate::AuthManager;
use crate::codex_thread::ThreadConfigSnapshot;
use crate::config::Config;
use crate::error::CodexErr;
use crate::models_manager::manager::ModelsManager;
use crate::session::Codex;
use crate::session::CodexSpawnOk;
use crate::session::SUBMISSION_CHANNEL_CAPACITY;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use codex_protocol::protocol::InitialHistory;

pub(crate) fn apply_delegate_model_provider(
    config: &mut Config,
    provider_id: &str,
) -> Result<(), CodexErr> {
    config.apply_model_provider_id(provider_id)
}

#[derive(Clone, Debug, Default)]
pub(crate) struct DelegateRuntimeContextParams {
    pub(crate) task_kind: Option<String>,
    pub(crate) parent_turn_id: Option<String>,
    pub(crate) agents_summary: Option<String>,
}

/// Start an interactive sub-Codex thread and return IO channels.
///
/// The returned `events_rx` yields non-approval events emitted by the sub-agent.
/// Approval requests are handled via `parent_session` and are not surfaced.
/// The returned `ops_tx` allows the caller to submit additional `Op`s to the sub-agent.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_codex_thread_interactive(
    config: Config,
    auth_manager: Arc<AuthManager>,
    models_manager: Arc<ModelsManager>,
    parent_session: Arc<Session>,
    parent_ctx: Arc<TurnContext>,
    cancel_token: CancellationToken,
    subagent_source: SubAgentSource,
    runtime_context_params: DelegateRuntimeContextParams,
    initial_history: Option<InitialHistory>,
) -> Result<Codex, CodexErr> {
    let (tx_sub, rx_sub) = async_channel::bounded(SUBMISSION_CHANNEL_CAPACITY);
    let (tx_ops, rx_ops) = async_channel::bounded(SUBMISSION_CHANNEL_CAPACITY);

    let CodexSpawnOk { codex, .. } = Box::pin(Codex::spawn(
        config,
        auth_manager,
        models_manager,
        Arc::clone(&parent_session.services.skills_manager),
        Arc::clone(&parent_session.services.file_watcher),
        initial_history.unwrap_or(InitialHistory::New),
        SessionSource::SubAgent(subagent_source.clone()),
        parent_session.services.agent_control.clone(),
        Vec::new(),
    ))
    .or_cancel(&cancel_token)
    .await??;
    let runtime_context = DelegateRuntimeContext::new(
        codex.thread_config_snapshot().await,
        codex.session.conversation_id,
        parent_session.conversation_id,
        runtime_context_params,
    );
    let codex = Arc::new(codex);

    // Use a child token so parent cancel cascades but we can scope it to this task
    let cancel_token_events = cancel_token.child_token();
    let cancel_token_ops = cancel_token.child_token();

    // Forward events from the sub-agent to the consumer, filtering approvals and
    // routing them to the parent session for decisions.
    let parent_session_clone = Arc::clone(&parent_session);
    let parent_ctx_clone = Arc::clone(&parent_ctx);
    let codex_for_events = Arc::clone(&codex);
    tokio::spawn(async move {
        forward_events(
            codex_for_events,
            tx_sub,
            parent_session_clone,
            parent_ctx_clone,
            runtime_context,
            cancel_token_events,
        )
        .await;
    });

    // Forward ops from the caller to the sub-agent.
    let codex_for_ops = Arc::clone(&codex);
    tokio::spawn(async move {
        forward_ops(codex_for_ops, rx_ops, cancel_token_ops).await;
    });

    Ok(Codex {
        next_id: AtomicU64::new(0),
        tx_sub: tx_ops,
        rx_event: rx_sub,
        agent_status: codex.agent_status.clone(),
        session: Arc::clone(&codex.session),
    })
}

/// Convenience wrapper for one-time use with an initial prompt.
///
/// Internally calls the interactive variant, then immediately submits the provided input.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_codex_thread_one_shot(
    config: Config,
    auth_manager: Arc<AuthManager>,
    models_manager: Arc<ModelsManager>,
    input: Vec<UserInput>,
    parent_session: Arc<Session>,
    parent_ctx: Arc<TurnContext>,
    cancel_token: CancellationToken,
    subagent_source: SubAgentSource,
    runtime_context_params: DelegateRuntimeContextParams,
    initial_history: Option<InitialHistory>,
) -> Result<Codex, CodexErr> {
    // Use a child token so we can stop the delegate after completion without
    // requiring the caller to cancel the parent token.
    let child_cancel = cancel_token.child_token();
    let io = run_codex_thread_interactive(
        config,
        auth_manager,
        models_manager,
        parent_session,
        parent_ctx,
        child_cancel.clone(),
        subagent_source,
        runtime_context_params,
        initial_history,
    )
    .await?;

    // Send the initial input to kick off the one-shot turn.
    io.submit(Op::UserInput {
        items: input,
        final_output_json_schema: None,
    })
    .await?;

    // Bridge events so we can observe completion and shut down automatically.
    let (tx_bridge, rx_bridge) = async_channel::bounded(SUBMISSION_CHANNEL_CAPACITY);
    let ops_tx = io.tx_sub.clone();
    let agent_status = io.agent_status.clone();
    let session = Arc::clone(&io.session);
    let io_for_bridge = io;
    tokio::spawn(async move {
        while let Ok(event) = io_for_bridge.next_event().await {
            let should_shutdown = matches!(
                event.msg,
                EventMsg::TurnComplete(_) | EventMsg::TurnAborted(_) | EventMsg::TurnPaused(_)
            );
            let _ = tx_bridge.send(event).await;
            if should_shutdown {
                let _ = ops_tx
                    .send(Submission {
                        id: "shutdown".to_string(),
                        op: Op::Shutdown {},
                    })
                    .await;
                child_cancel.cancel();
                break;
            }
        }
    });

    // For one-shot usage, return a closed `tx_sub` so callers cannot submit
    // additional ops after the initial request. Create a channel and drop the
    // receiver to close it immediately.
    let (tx_closed, rx_closed) = async_channel::bounded(SUBMISSION_CHANNEL_CAPACITY);
    drop(rx_closed);

    Ok(Codex {
        next_id: AtomicU64::new(0),
        rx_event: rx_bridge,
        tx_sub: tx_closed,
        agent_status,
        session,
    })
}

async fn forward_events(
    codex: Arc<Codex>,
    tx_sub: Sender<Event>,
    parent_session: Arc<Session>,
    parent_ctx: Arc<TurnContext>,
    mut runtime_context: DelegateRuntimeContext,
    cancel_token: CancellationToken,
) {
    let cancelled = cancel_token.cancelled();
    tokio::pin!(cancelled);

    loop {
        tokio::select! {
            _ = &mut cancelled => {
                shutdown_delegate(&codex).await;
                break;
            }
            event = codex.next_event() => {
                let Event { id, msg } = match event {
                    Ok(event) => event,
                    Err(_) => break,
                };
                match msg {
                    // ignore all legacy delta events
                    EventMsg::AgentMessageDelta(_) | EventMsg::AgentReasoningDelta(_) => {}
                    EventMsg::SessionConfigured(event) => {
                        runtime_context.apply_session_configured(event);
                        if !send_runtime_context_update(
                            &mut runtime_context,
                            &tx_sub,
                            id,
                            &cancel_token,
                        )
                        .await
                        {
                            shutdown_delegate(&codex).await;
                            break;
                        }
                    }
                    EventMsg::TokenCount(event) => {
                        runtime_context.apply_token_count(event);
                        if !send_runtime_context_update(
                            &mut runtime_context,
                            &tx_sub,
                            id,
                            &cancel_token,
                        )
                        .await
                        {
                            shutdown_delegate(&codex).await;
                            break;
                        }
                    }
                    EventMsg::ThreadNameUpdated(event) => {
                        runtime_context.apply_thread_name_updated(event);
                        if !send_runtime_context_update(
                            &mut runtime_context,
                            &tx_sub,
                            id,
                            &cancel_token,
                        )
                        .await
                        {
                            shutdown_delegate(&codex).await;
                            break;
                        }
                    }
                    EventMsg::TurnStarted(event) => {
                        runtime_context.apply_turn_started(&event);
                        if !send_runtime_context_update(
                            &mut runtime_context,
                            &tx_sub,
                            id.clone(),
                            &cancel_token,
                        )
                        .await
                            || !send_to_consumer(
                                &tx_sub,
                                Event {
                                    id,
                                    msg: EventMsg::TurnStarted(event),
                                },
                                &cancel_token,
                            )
                            .await
                        {
                            shutdown_delegate(&codex).await;
                            break;
                        }
                    }
                    EventMsg::TurnComplete(_) | EventMsg::TurnAborted(_) | EventMsg::TurnPaused(_) => {
                        let terminal = Event { id, msg };
                        if !send_runtime_context_deactivation(
                            &mut runtime_context,
                            &tx_sub,
                            terminal.id.clone(),
                            &cancel_token,
                        )
                        .await
                            || !send_to_consumer(&tx_sub, terminal, &cancel_token).await
                        {
                            shutdown_delegate(&codex).await;
                            break;
                        }
                    }
                    EventMsg::ExecApprovalRequest(event) => {
                        // Initiate approval via parent session; do not surface to consumer.
                        handle_exec_approval(
                            &codex,
                            id,
                            &parent_session,
                            &parent_ctx,
                            event,
                            &cancel_token,
                        )
                        .await;
                    }
                    EventMsg::ApplyPatchApprovalRequest(event) => {
                        handle_patch_approval(
                            &codex,
                            id,
                            &parent_session,
                            &parent_ctx,
                            event,
                            &cancel_token,
                        )
                        .await;
                    }
                    EventMsg::RequestUserInput(event) => {
                        handle_request_user_input(
                            &codex,
                            id,
                            &parent_session,
                            &parent_ctx,
                            event,
                            &cancel_token,
                        )
                        .await;
                    }
                    msg => {
                        if !ensure_runtime_context_activated(
                            &mut runtime_context,
                            &tx_sub,
                            id.clone(),
                            &cancel_token,
                        )
                        .await
                            || !send_to_consumer(&tx_sub, Event { id, msg }, &cancel_token).await
                        {
                            shutdown_delegate(&codex).await;
                            break;
                        }
                    }
                }
            }
        }
    }

    let _ = send_runtime_context_deactivation(
        &mut runtime_context,
        &tx_sub,
        "runtime-context-ended".to_string(),
        &cancel_token,
    )
    .await;
}

struct DelegateRuntimeContext {
    snapshot: RuntimeContextSnapshot,
    activated: bool,
    finished: bool,
}

impl DelegateRuntimeContext {
    fn new(
        thread_config: ThreadConfigSnapshot,
        delegate_session_id: ThreadId,
        parent_session_id: ThreadId,
        params: DelegateRuntimeContextParams,
    ) -> Self {
        let scope_id = format!("delegate:{parent_session_id}:{delegate_session_id}");
        Self {
            snapshot: RuntimeContextSnapshot {
                scope_id,
                scope: RuntimeContextScope::Delegate,
                task_kind: params.task_kind,
                session_source: thread_config.session_source,
                session_id: delegate_session_id,
                parent_session_id: Some(parent_session_id),
                parent_turn_id: params.parent_turn_id,
                thread_name: None,
                rollout_path: None,
                cwd: thread_config.cwd,
                model: thread_config.model,
                model_provider_id: thread_config.model_provider_id,
                approval_policy: thread_config.approval_policy,
                sandbox_policy: thread_config.sandbox_policy,
                reasoning_effort: thread_config.reasoning_effort,
                service_tier: thread_config.service_tier,
                model_context_window: None,
                agents_summary: params.agents_summary,
                token_info: None,
            },
            activated: false,
            finished: false,
        }
    }

    fn apply_session_configured(
        &mut self,
        event: codex_protocol::protocol::SessionConfiguredEvent,
    ) {
        self.snapshot.session_id = event.session_id;
        self.snapshot.thread_name = event.thread_name;
        self.snapshot.model = event.model;
        self.snapshot.model_provider_id = event.model_provider_id;
        self.snapshot.approval_policy = event.approval_policy;
        self.snapshot.sandbox_policy = event.sandbox_policy;
        self.snapshot.cwd = event.cwd;
        self.snapshot.reasoning_effort = event.reasoning_effort;
        self.snapshot.service_tier = event.service_tier;
        self.snapshot.rollout_path = event.rollout_path;
    }

    fn apply_token_count(&mut self, event: TokenCountEvent) {
        if let Some(info) = event.info {
            self.snapshot.model_context_window = info.model_context_window;
            self.snapshot.token_info = Some(info);
        }
    }

    fn apply_thread_name_updated(&mut self, event: ThreadNameUpdatedEvent) {
        if event.thread_id == self.snapshot.session_id {
            self.snapshot.thread_name = event.thread_name;
        }
    }

    fn apply_turn_started(&mut self, event: &TurnStartedEvent) {
        self.snapshot.model_context_window = event.model_context_window;
    }
}

async fn ensure_runtime_context_activated(
    runtime_context: &mut DelegateRuntimeContext,
    tx_sub: &Sender<Event>,
    id: String,
    cancel_token: &CancellationToken,
) -> bool {
    if runtime_context.finished {
        return true;
    }

    if runtime_context.activated {
        return true;
    }

    runtime_context.activated = true;
    send_to_consumer(
        tx_sub,
        Event {
            id,
            msg: EventMsg::RuntimeContextActivated(RuntimeContextActivatedEvent {
                snapshot: runtime_context.snapshot.clone(),
            }),
        },
        cancel_token,
    )
    .await
}

async fn send_runtime_context_update(
    runtime_context: &mut DelegateRuntimeContext,
    tx_sub: &Sender<Event>,
    id: String,
    cancel_token: &CancellationToken,
) -> bool {
    if runtime_context.finished {
        return true;
    }

    if !runtime_context.activated {
        return ensure_runtime_context_activated(runtime_context, tx_sub, id, cancel_token).await;
    }

    send_to_consumer(
        tx_sub,
        Event {
            id,
            msg: EventMsg::RuntimeContextUpdated(RuntimeContextUpdatedEvent {
                snapshot: runtime_context.snapshot.clone(),
            }),
        },
        cancel_token,
    )
    .await
}

async fn send_runtime_context_deactivation(
    runtime_context: &mut DelegateRuntimeContext,
    tx_sub: &Sender<Event>,
    id: String,
    cancel_token: &CancellationToken,
) -> bool {
    if !runtime_context.activated || runtime_context.finished {
        return true;
    }

    runtime_context.finished = true;
    runtime_context.activated = false;
    send_to_consumer(
        tx_sub,
        Event {
            id,
            msg: EventMsg::RuntimeContextDeactivated(RuntimeContextDeactivatedEvent {
                scope_id: runtime_context.snapshot.scope_id.clone(),
                session_id: runtime_context.snapshot.session_id,
            }),
        },
        cancel_token,
    )
    .await
}

async fn send_to_consumer(
    tx_sub: &Sender<Event>,
    event: Event,
    cancel_token: &CancellationToken,
) -> bool {
    matches!(tx_sub.send(event).or_cancel(cancel_token).await, Ok(Ok(())))
}

/// Ask the delegate to stop and drain its events so background sends do not hit a closed channel.
async fn shutdown_delegate(codex: &Codex) {
    let _ = codex.submit(Op::Interrupt).await;
    let _ = codex.submit(Op::Shutdown {}).await;

    let _ = timeout(Duration::from_millis(500), async {
        while let Ok(event) = codex.next_event().await {
            if matches!(
                event.msg,
                EventMsg::TurnAborted(_) | EventMsg::TurnPaused(_) | EventMsg::TurnComplete(_)
            ) {
                break;
            }
        }
    })
    .await;
}

/// Forward ops from a caller to a sub-agent, respecting cancellation.
async fn forward_ops(
    codex: Arc<Codex>,
    rx_ops: Receiver<Submission>,
    cancel_token_ops: CancellationToken,
) {
    loop {
        let submission = match rx_ops.recv().or_cancel(&cancel_token_ops).await {
            Ok(Ok(submission)) => submission,
            Ok(Err(_)) | Err(_) => break,
        };
        let _ = codex.submit_with_id(submission).await;
    }
}

/// Handle an ExecApprovalRequest by consulting the parent session and replying.
async fn handle_exec_approval(
    codex: &Codex,
    id: String,
    parent_session: &Session,
    parent_ctx: &TurnContext,
    event: ExecApprovalRequestEvent,
    cancel_token: &CancellationToken,
) {
    let ExecApprovalRequestEvent {
        call_id,
        command,
        cwd,
        reason,
        proposed_execpolicy_amendment,
        ..
    } = event;
    // Race approval with cancellation and timeout to avoid hangs.
    let approval_fut = parent_session.request_command_approval(
        parent_ctx,
        call_id,
        command,
        cwd,
        reason,
        proposed_execpolicy_amendment,
    );
    let decision = await_approval_with_cancel(
        approval_fut,
        parent_session,
        &parent_ctx.sub_id,
        cancel_token,
    )
    .await;

    let _ = codex.submit(Op::ExecApproval { id, decision }).await;
}

/// Handle an ApplyPatchApprovalRequest by consulting the parent session and replying.
async fn handle_patch_approval(
    codex: &Codex,
    id: String,
    parent_session: &Session,
    parent_ctx: &TurnContext,
    event: ApplyPatchApprovalRequestEvent,
    cancel_token: &CancellationToken,
) {
    let ApplyPatchApprovalRequestEvent {
        call_id,
        changes,
        reason,
        grant_root,
        ..
    } = event;
    let decision_rx = parent_session
        .request_patch_approval(parent_ctx, call_id, changes, reason, grant_root)
        .await;
    let decision = await_approval_with_cancel(
        async move { decision_rx.await.unwrap_or_default() },
        parent_session,
        &parent_ctx.sub_id,
        cancel_token,
    )
    .await;
    let _ = codex.submit(Op::PatchApproval { id, decision }).await;
}

async fn handle_request_user_input(
    codex: &Codex,
    id: String,
    parent_session: &Session,
    parent_ctx: &TurnContext,
    event: RequestUserInputEvent,
    cancel_token: &CancellationToken,
) {
    let args = RequestUserInputArgs {
        questions: event.questions,
    };
    let response_fut =
        parent_session.request_user_input(parent_ctx, parent_ctx.sub_id.clone(), args);
    let response = await_user_input_with_cancel(
        response_fut,
        parent_session,
        &parent_ctx.sub_id,
        cancel_token,
    )
    .await;
    let _ = codex.submit(Op::UserInputAnswer { id, response }).await;
}

async fn await_user_input_with_cancel<F>(
    fut: F,
    parent_session: &Session,
    sub_id: &str,
    cancel_token: &CancellationToken,
) -> RequestUserInputResponse
where
    F: core::future::Future<Output = Option<RequestUserInputResponse>>,
{
    tokio::select! {
        biased;
        _ = cancel_token.cancelled() => {
            let empty = RequestUserInputResponse {
                answers: HashMap::new(),
            };
            parent_session
                .notify_user_input_response(sub_id, empty.clone())
                .await;
            empty
        }
        response = fut => response.unwrap_or_else(|| RequestUserInputResponse {
            answers: HashMap::new(),
        }),
    }
}

/// Await an approval decision, aborting on cancellation.
async fn await_approval_with_cancel<F>(
    fut: F,
    parent_session: &Session,
    sub_id: &str,
    cancel_token: &CancellationToken,
) -> codex_protocol::protocol::ReviewDecision
where
    F: core::future::Future<Output = codex_protocol::protocol::ReviewDecision>,
{
    tokio::select! {
        biased;
        _ = cancel_token.cancelled() => {
            parent_session
                .notify_approval(sub_id, codex_protocol::protocol::ReviewDecision::Abort)
                .await;
            codex_protocol::protocol::ReviewDecision::Abort
        }
        decision = fut => {
            decision
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_channel::bounded;
    use codex_protocol::models::ResponseItem;
    use codex_protocol::protocol::AgentStatus;
    use codex_protocol::protocol::RawResponseItemEvent;
    use codex_protocol::protocol::TurnAbortReason;
    use codex_protocol::protocol::TurnAbortedEvent;
    use pretty_assertions::assert_eq;
    use tokio::sync::watch;

    #[tokio::test]
    async fn forward_ops_preserves_submission_id() {
        let (tx_child, rx_child) = bounded(SUBMISSION_CHANNEL_CAPACITY);
        let (tx_events, rx_events) = bounded(SUBMISSION_CHANNEL_CAPACITY);
        let (_agent_status_tx, agent_status) = watch::channel(AgentStatus::PendingInit);
        let (session, _ctx, _rx_evt) =
            crate::session::tests::make_session_and_context_with_rx().await;
        let codex = Arc::new(Codex {
            next_id: AtomicU64::new(0),
            tx_sub: tx_child,
            rx_event: rx_events,
            agent_status,
            session,
        });
        let (tx_ops, rx_ops) = bounded(SUBMISSION_CHANNEL_CAPACITY);
        let cancel = CancellationToken::new();
        let forward = tokio::spawn(forward_ops(codex, rx_ops, cancel));

        tx_ops
            .send(Submission {
                id: "caller-submission".to_string(),
                op: Op::Interrupt,
            })
            .await
            .unwrap();
        drop(tx_ops);
        drop(tx_events);

        timeout(std::time::Duration::from_millis(1000), forward)
            .await
            .expect("forward_ops hung")
            .expect("forward_ops join error");

        let forwarded = rx_child.recv().await.expect("forwarded submission");
        assert_eq!("caller-submission", forwarded.id);
        assert_eq!(Op::Interrupt, forwarded.op);
    }

    #[tokio::test]
    async fn forward_events_cancelled_while_send_blocked_shuts_down_delegate() {
        let (tx_events, rx_events) = bounded(1);
        let (tx_sub, rx_sub) = bounded(SUBMISSION_CHANNEL_CAPACITY);
        let (_agent_status_tx, agent_status) = watch::channel(AgentStatus::PendingInit);
        let (session, ctx, _rx_evt) =
            crate::session::tests::make_session_and_context_with_rx().await;
        let codex = Arc::new(Codex {
            next_id: AtomicU64::new(0),
            tx_sub,
            rx_event: rx_events,
            agent_status,
            session: Arc::clone(&session),
        });

        let (tx_out, rx_out) = bounded(1);
        tx_out
            .send(Event {
                id: "full".to_string(),
                msg: EventMsg::TurnAborted(TurnAbortedEvent {
                    reason: TurnAbortReason::Interrupted,
                }),
            })
            .await
            .unwrap();

        let cancel = CancellationToken::new();
        let forward = tokio::spawn(forward_events(
            Arc::clone(&codex),
            tx_out.clone(),
            session,
            ctx,
            test_runtime_context(),
            cancel.clone(),
        ));

        tx_events
            .send(Event {
                id: "evt".to_string(),
                msg: EventMsg::RawResponseItem(RawResponseItemEvent {
                    item: ResponseItem::CustomToolCall {
                        id: None,
                        status: None,
                        call_id: "call-1".to_string(),
                        name: "tool".to_string(),
                        input: "{}".to_string(),
                    },
                }),
            })
            .await
            .unwrap();

        drop(tx_events);
        cancel.cancel();
        timeout(std::time::Duration::from_millis(1000), forward)
            .await
            .expect("forward_events hung")
            .expect("forward_events join error");

        let received = rx_out.recv().await.expect("prefilled event missing");
        assert_eq!("full", received.id);
        let mut ops = Vec::new();
        while let Ok(sub) = rx_sub.try_recv() {
            ops.push(sub.op);
        }
        assert!(
            ops.iter().any(|op| matches!(op, Op::Interrupt)),
            "expected Interrupt op after cancellation"
        );
        assert!(
            ops.iter().any(|op| matches!(op, Op::Shutdown)),
            "expected Shutdown op after cancellation"
        );
    }

    #[tokio::test]
    async fn forward_events_converts_delegate_status_events_to_runtime_context() {
        let (tx_events, rx_events) = bounded(SUBMISSION_CHANNEL_CAPACITY);
        let (tx_sub, _rx_sub) = bounded(SUBMISSION_CHANNEL_CAPACITY);
        let (_agent_status_tx, agent_status) = watch::channel(AgentStatus::PendingInit);
        let (session, ctx, _rx_evt) =
            crate::session::tests::make_session_and_context_with_rx().await;
        let codex = Arc::new(Codex {
            next_id: AtomicU64::new(0),
            tx_sub,
            rx_event: rx_events,
            agent_status,
            session: Arc::clone(&session),
        });
        let runtime_context = test_runtime_context();
        let delegate_session_id = runtime_context.snapshot.session_id;

        let (tx_out, rx_out) = bounded(SUBMISSION_CHANNEL_CAPACITY);
        let cancel = CancellationToken::new();
        let forward = tokio::spawn(forward_events(
            Arc::clone(&codex),
            tx_out,
            session,
            ctx,
            runtime_context,
            cancel,
        ));

        tx_events
            .send(Event {
                id: "configured".to_string(),
                msg: EventMsg::SessionConfigured(
                    codex_protocol::protocol::SessionConfiguredEvent {
                        session_id: delegate_session_id,
                        forked_from_id: None,
                        thread_name: Some("Delegate review".to_string()),
                        model: "deepseek-v4-pro".to_string(),
                        model_provider_id: "deepseek".to_string(),
                        approval_policy: codex_protocol::protocol::AskForApproval::Never,
                        sandbox_policy: codex_protocol::protocol::SandboxPolicy::ReadOnly,
                        cwd: std::path::PathBuf::from("/tmp/delegate"),
                        reasoning_effort: None,
                        service_tier: None,
                        history_log_id: 0,
                        history_entry_count: 0,
                        initial_messages: None,
                        rollout_path: None,
                    },
                ),
            })
            .await
            .unwrap();
        tx_events
            .send(Event {
                id: "tokens".to_string(),
                msg: EventMsg::TokenCount(TokenCountEvent {
                    info: Some(codex_protocol::protocol::TokenUsageInfo {
                        total_token_usage: codex_protocol::protocol::TokenUsage {
                            total_tokens: 42,
                            ..Default::default()
                        },
                        last_token_usage: codex_protocol::protocol::TokenUsage {
                            total_tokens: 42,
                            ..Default::default()
                        },
                        model_context_window: Some(1_048_576),
                    }),
                    rate_limits: None,
                }),
            })
            .await
            .unwrap();
        tx_events
            .send(Event {
                id: "done".to_string(),
                msg: EventMsg::TurnComplete(codex_protocol::protocol::TurnCompleteEvent {
                    last_agent_message: None,
                }),
            })
            .await
            .unwrap();
        drop(tx_events);

        let activated = match rx_out.recv().await.expect("activation event").msg {
            EventMsg::RuntimeContextActivated(activated) => activated,
            other => panic!("expected runtime context activation, got {other:?}"),
        };
        assert_eq!(activated.snapshot.session_id, delegate_session_id);
        assert_eq!(activated.snapshot.model, "deepseek-v4-pro");
        assert_eq!(activated.snapshot.model_provider_id, "deepseek");
        assert_eq!(activated.snapshot.task_kind, Some("review".to_string()));

        let updated = match rx_out.recv().await.expect("token update event").msg {
            EventMsg::RuntimeContextUpdated(updated) => updated,
            other => panic!("expected runtime context update, got {other:?}"),
        };
        assert_eq!(
            updated
                .snapshot
                .token_info
                .as_ref()
                .unwrap()
                .model_context_window,
            Some(1_048_576)
        );

        let deactivated = match rx_out.recv().await.expect("deactivation event").msg {
            EventMsg::RuntimeContextDeactivated(deactivated) => deactivated,
            other => panic!("expected runtime context deactivation, got {other:?}"),
        };
        assert_eq!(deactivated.session_id, delegate_session_id);

        let terminal = rx_out.recv().await.expect("terminal event");
        assert!(matches!(terminal.msg, EventMsg::TurnComplete(_)));

        timeout(std::time::Duration::from_millis(1000), forward)
            .await
            .expect("forward_events hung")
            .expect("forward_events join error");
    }

    fn test_runtime_context() -> DelegateRuntimeContext {
        let delegate_session_id = ThreadId::new();
        let parent_session_id = ThreadId::new();
        DelegateRuntimeContext::new(
            ThreadConfigSnapshot {
                model: "delegate-model".to_string(),
                model_provider_id: "delegate-provider".to_string(),
                approval_policy: codex_protocol::protocol::AskForApproval::Never,
                sandbox_policy: codex_protocol::protocol::SandboxPolicy::ReadOnly,
                cwd: std::path::PathBuf::from("/tmp/delegate"),
                reasoning_effort: None,
                service_tier: None,
                personality: None,
                session_source: SessionSource::SubAgent(SubAgentSource::Review),
            },
            delegate_session_id,
            parent_session_id,
            DelegateRuntimeContextParams {
                task_kind: Some("review".to_string()),
                parent_turn_id: Some("parent-turn".to_string()),
                agents_summary: Some("AGENTS.review.md, AGENTS.md".to_string()),
            },
        )
    }
}
