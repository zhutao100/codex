mod compact;
mod continue_task;
mod ghost_snapshot;
mod post_turn_completion_review;
mod regular;
mod review;
mod undo;
mod user_shell;

use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use tokio::select;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use tokio_util::task::AbortOnDropHandle;
use tracing::Instrument;
use tracing::Span;
use tracing::trace;
use tracing::warn;

use crate::AuthManager;
use crate::features::Feature;
use crate::models_manager::manager::ModelsManager;
use crate::protocol::CodexErrorInfo;
use crate::protocol::ErrorEvent;
use crate::protocol::Event;
use crate::protocol::EventMsg;
use crate::protocol::SessionSource;
use crate::protocol::ThreadNameUpdatedEvent;
use crate::protocol::TurnAbortReason;
use crate::protocol::TurnAbortedEvent;
use crate::protocol::TurnCompleteEvent;
use crate::protocol::TurnContinuationSource;
use crate::protocol::TurnPauseReason;
use crate::protocol::TurnPausedEvent;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use crate::session_prefix::TURN_ABORTED_OPEN_TAG;
use crate::state::ActiveTurn;
use crate::state::PendingContinuation;
use crate::state::PendingContinuationTarget;
use crate::state::RunningTask;
use crate::state::TaskKind;
use codex_protocol::config_types::ModeKind;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::user_input::UserInput;

pub(crate) use compact::CompactTask;
pub(crate) use continue_task::ContinueTask;
pub(crate) use ghost_snapshot::GhostSnapshotTask;
pub(crate) use post_turn_completion_review::PostTurnCompletionReviewTask;
pub(crate) use regular::RegularTask;
pub(crate) use review::ReviewDelegateConfigParams;
pub(crate) use review::ReviewDelegateInstructionProfile;
pub(crate) use review::ReviewTask;
pub(crate) use review::configure_review_delegate_config;
pub(crate) use undo::UndoTask;
pub(crate) use user_shell::UserShellCommandMode;
pub(crate) use user_shell::UserShellCommandTask;
pub(crate) use user_shell::execute_user_shell_command;

const GRACEFULL_INTERRUPTION_TIMEOUT_MS: u64 = 100;
const TURN_ABORTED_INTERRUPTED_GUIDANCE: &str = "The user interrupted the previous turn on purpose. If any tools/commands were aborted, they may have partially executed; verify current state before retrying.";

fn user_text_messages_for_completed_turn_review(input: &[UserInput]) -> Vec<String> {
    input
        .iter()
        .filter_map(|item| match item {
            UserInput::Text { text, .. } if !text.trim().is_empty() => Some(text.clone()),
            UserInput::Text { .. }
            | UserInput::Image { .. }
            | UserInput::LocalImage { .. }
            | UserInput::Skill { .. }
            | UserInput::Mention { .. } => None,
            _ => None,
        })
        .collect()
}

#[derive(Clone, Debug)]
pub(crate) enum TaskStopReason {
    Abort(TurnAbortReason),
    Pause(TurnPauseReason),
}

/// Thin wrapper that exposes the parts of [`Session`] task runners need.
#[derive(Clone)]
pub(crate) struct SessionTaskContext {
    session: Arc<Session>,
}

impl SessionTaskContext {
    pub(crate) fn new(session: Arc<Session>) -> Self {
        Self { session }
    }

    pub(crate) fn clone_session(&self) -> Arc<Session> {
        Arc::clone(&self.session)
    }

    pub(crate) fn auth_manager(&self) -> Arc<AuthManager> {
        Arc::clone(&self.session.services.auth_manager)
    }

    pub(crate) fn models_manager(&self) -> Arc<ModelsManager> {
        Arc::clone(&self.session.services.models_manager)
    }
}

/// Async task that drives a [`Session`] turn.
///
/// Implementations encapsulate a specific Codex workflow (regular chat,
/// reviews, ghost snapshots, etc.). Each task instance is owned by a
/// [`Session`] and executed on a background Tokio task. The trait is
/// intentionally small: implementers identify themselves via
/// [`SessionTask::kind`], perform their work in [`SessionTask::run`], and may
/// release resources in [`SessionTask::abort`].
pub(crate) trait SessionTask: Send + Sync + 'static {
    /// Describes the type of work the task performs so the session can
    /// surface it in telemetry and UI.
    fn kind(&self) -> TaskKind;

    /// Executes the task until completion or cancellation.
    ///
    /// Implementations typically stream protocol events using `session` and
    /// `ctx`, returning an optional final agent message when finished. The
    /// provided `cancellation_token` is cancelled when the session requests an
    /// abort; implementers should watch for it and terminate quickly once it
    /// fires. Returning [`Some`] yields a final message that
    /// [`Session::on_task_finished`] will emit to the client.
    fn run(
        self: Arc<Self>,
        session: Arc<SessionTaskContext>,
        ctx: Arc<TurnContext>,
        input: Vec<UserInput>,
        cancellation_token: CancellationToken,
    ) -> impl std::future::Future<Output = Option<String>> + Send;

    /// Gives the task a chance to perform cleanup after an abort.
    ///
    /// The default implementation is a no-op; override this if additional
    /// teardown or notifications are required once
    /// [`Session::abort_all_tasks`] cancels the task.
    fn abort(
        &self,
        session: Arc<SessionTaskContext>,
        ctx: Arc<TurnContext>,
        reason: TaskStopReason,
    ) -> impl std::future::Future<Output = ()> + Send {
        async move {
            let _ = (session, ctx, reason);
        }
    }
}

pub(crate) trait AnySessionTask: Send + Sync + 'static {
    fn kind(&self) -> TaskKind;

    fn run(
        self: Arc<Self>,
        session: Arc<SessionTaskContext>,
        ctx: Arc<TurnContext>,
        input: Vec<UserInput>,
        cancellation_token: CancellationToken,
    ) -> BoxFuture<'static, Option<String>>;

    fn abort<'a>(
        &'a self,
        session: Arc<SessionTaskContext>,
        ctx: Arc<TurnContext>,
        reason: TaskStopReason,
    ) -> BoxFuture<'a, ()>;
}

impl<T> AnySessionTask for T
where
    T: SessionTask,
{
    fn kind(&self) -> TaskKind {
        SessionTask::kind(self)
    }

    fn run(
        self: Arc<Self>,
        session: Arc<SessionTaskContext>,
        ctx: Arc<TurnContext>,
        input: Vec<UserInput>,
        cancellation_token: CancellationToken,
    ) -> BoxFuture<'static, Option<String>> {
        Box::pin(SessionTask::run(
            self,
            session,
            ctx,
            input,
            cancellation_token,
        ))
    }

    fn abort<'a>(
        &'a self,
        session: Arc<SessionTaskContext>,
        ctx: Arc<TurnContext>,
        reason: TaskStopReason,
    ) -> BoxFuture<'a, ()> {
        Box::pin(SessionTask::abort(self, session, ctx, reason))
    }
}

impl Session {
    pub async fn spawn_task<T: SessionTask>(
        self: &Arc<Self>,
        turn_context: Arc<TurnContext>,
        input: Vec<UserInput>,
        task: T,
    ) {
        self.abort_all_tasks(TurnAbortReason::Replaced).await;

        let task: Arc<dyn AnySessionTask> = Arc::new(task);
        let task_kind = task.kind();
        if task_kind != TaskKind::Regular {
            self.seed_initial_context_if_needed(turn_context.as_ref())
                .await;
        }
        let completed_turn_user_messages = if task_kind == TaskKind::Regular {
            user_text_messages_for_completed_turn_review(&input)
        } else {
            Vec::new()
        };

        let cancellation_token = CancellationToken::new();
        let done = Arc::new(Notify::new());
        let (registered_tx, registered_rx) = tokio::sync::oneshot::channel::<()>();

        let done_clone = Arc::clone(&done);
        let handle = {
            let session_ctx = Arc::new(SessionTaskContext::new(Arc::clone(self)));
            let ctx = Arc::clone(&turn_context);
            let task_for_run = Arc::clone(&task);
            let task_cancellation_token = cancellation_token.child_token();
            let session_span = Span::current();
            tokio::spawn(
                async move {
                    let _ = registered_rx.await;
                    let ctx_for_finish = Arc::clone(&ctx);
                    let last_agent_message = task_for_run
                        .run(
                            Arc::clone(&session_ctx),
                            ctx,
                            input,
                            task_cancellation_token.child_token(),
                        )
                        .await;
                    session_ctx.clone_session().flush_rollout().await;
                    if !task_cancellation_token.is_cancelled() {
                        // Emit completion uniformly from spawn site so all tasks share the same lifecycle.
                        let sess = session_ctx.clone_session();
                        sess.on_task_finished(ctx_for_finish, last_agent_message, task_kind)
                            .await;
                    }
                    done_clone.notify_waiters();
                }
                .instrument(session_span),
            )
        };

        let timer = turn_context
            .otel_manager
            .start_timer("codex.turn.e2e_duration_ms", &[])
            .ok();

        let running_task = RunningTask {
            done,
            handle: Arc::new(AbortOnDropHandle::new(handle)),
            kind: task_kind,
            task,
            cancellation_token,
            turn_context: Arc::clone(&turn_context),
            completed_turn_user_messages,
            _timer: timer,
        };
        self.register_new_active_task(running_task).await;
        let _ = registered_tx.send(());
    }

    pub async fn abort_all_tasks(self: &Arc<Self>, reason: TurnAbortReason) {
        self.stop_all_tasks(TaskStopReason::Abort(reason)).await;
    }

    pub async fn pause_all_tasks(self: &Arc<Self>, reason: TurnPauseReason) {
        self.stop_all_tasks(TaskStopReason::Pause(reason)).await;
    }

    pub(crate) async fn pause_current_task_from_self(
        self: &Arc<Self>,
        turn_context: &Arc<TurnContext>,
        reason: TurnPauseReason,
        model: Option<String>,
    ) {
        let sub_id = turn_context.sub_id.clone();
        let turn_state = {
            let active = self.active_turn.lock().await;
            let Some(active_turn) = active.as_ref() else {
                return;
            };
            if !active_turn.tasks.contains_key(&sub_id) {
                return;
            }
            Arc::clone(&active_turn.turn_state)
        };
        turn_state.lock().await.clear_pending();
        self.set_pending_continuation(Some(PendingContinuation {
            source: TurnContinuationSource::Paused,
            continued_from_turn_id: Some(sub_id.clone()),
            model,
            pause_reason: Some(reason),
            target: PendingContinuationTarget::Regular,
        }))
        .await;
    }

    async fn stop_all_tasks(self: &Arc<Self>, reason: TaskStopReason) {
        for task in self.take_all_running_tasks().await {
            self.handle_task_abort(task, reason.clone()).await;
        }
        self.close_unified_exec_processes().await;
    }

    pub async fn on_task_finished(
        self: &Arc<Self>,
        turn_context: Arc<TurnContext>,
        last_agent_message: Option<String>,
        task_kind: TaskKind,
    ) {
        let mut active = self.active_turn.lock().await;
        let mut pending_input = Vec::<ResponseInputItem>::new();
        let mut should_close_processes = false;
        let mut completed_turn_user_messages = Vec::new();
        if let Some(at) = active.as_mut()
            && let Some(task) = at.remove_task(&turn_context.sub_id)
        {
            completed_turn_user_messages = task.completed_turn_user_messages;
            if at.tasks.is_empty() {
                let mut ts = at.turn_state.lock().await;
                pending_input = ts.take_pending_input();
                should_close_processes = true;
            }
        }
        if should_close_processes {
            *active = None;
        }
        drop(active);
        let paused_current_turn_reason = self
            .pending_pause_reason_for_turn(turn_context.sub_id.as_str())
            .await;
        if !pending_input.is_empty() && paused_current_turn_reason.is_none() {
            let pending_response_items = pending_input
                .into_iter()
                .map(ResponseItem::from)
                .collect::<Vec<_>>();
            self.record_conversation_items(turn_context.as_ref(), &pending_response_items)
                .await;
        }
        if should_close_processes {
            self.close_unified_exec_processes().await;
        }
        if let Some(reason) = paused_current_turn_reason {
            let sub_id = turn_context.sub_id.clone();
            self.send_event_raw(Event {
                id: sub_id.clone(),
                msg: EventMsg::TurnPaused(TurnPausedEvent {
                    turn_id: sub_id,
                    reason,
                }),
            })
            .await;
            return;
        }
        self.clear_pending_continuation().await;
        if task_kind == TaskKind::Regular {
            self.capture_completed_regular_turn_for_review(
                turn_context.as_ref(),
                completed_turn_user_messages,
                last_agent_message.as_deref(),
            )
            .await;
        }
        let event = EventMsg::TurnComplete(TurnCompleteEvent {
            last_agent_message: last_agent_message.clone(),
        });
        self.send_event(turn_context.as_ref(), event).await;
        if task_kind == TaskKind::PostTurnCompletionReview {
            self.continue_after_post_turn_completion_review(turn_context)
                .await;
            return;
        }
        self.maybe_auto_rename_thread(
            Arc::clone(&turn_context),
            last_agent_message.clone(),
            task_kind,
        )
        .await;
        self.maybe_auto_post_turn_completion_review(turn_context, last_agent_message, task_kind)
            .await;
    }

    async fn maybe_auto_rename_thread(
        self: &Arc<Self>,
        turn_context: Arc<TurnContext>,
        last_agent_message: Option<String>,
        task_kind: TaskKind,
    ) {
        if !turn_context.features.enabled(Feature::AutoRenameThread) {
            return;
        }

        if task_kind != TaskKind::Regular
            || turn_context.collaboration_mode.mode != ModeKind::Default
        {
            return;
        }

        if !matches!(
            turn_context.session_source,
            SessionSource::Cli | SessionSource::VSCode
        ) {
            return;
        }

        let Some(message) = last_agent_message.as_ref() else {
            return;
        };
        if message.trim().is_empty() {
            return;
        }

        if !self.prepare_auto_rename().await {
            return;
        }

        let session = Arc::clone(self);
        let turn_context = Arc::clone(&turn_context);
        tokio::spawn(async move {
            let thread_name_result =
                crate::thread_name::generate_thread_name(session.as_ref(), turn_context.as_ref())
                    .await;
            let thread_name = match thread_name_result {
                Ok(thread_name) => thread_name,
                Err(err) => {
                    session
                        .send_event(
                            turn_context.as_ref(),
                            EventMsg::Error(ErrorEvent {
                                message: format!("Auto-rename failed: {err}"),
                                codex_error_info: Some(CodexErrorInfo::Other),
                            }),
                        )
                        .await;
                    return;
                }
            };

            let Some(thread_name) = thread_name else {
                session
                    .send_event(
                        turn_context.as_ref(),
                        EventMsg::Error(ErrorEvent {
                            message: "Auto-rename failed: empty thread name.".to_string(),
                            codex_error_info: Some(CodexErrorInfo::Other),
                        }),
                    )
                    .await;
                return;
            };

            if let Err(err) = session.set_thread_name(thread_name.clone()).await {
                session
                    .send_event(
                        turn_context.as_ref(),
                        EventMsg::Error(ErrorEvent {
                            message: format!("Auto-rename failed: {err}"),
                            codex_error_info: Some(CodexErrorInfo::Other),
                        }),
                    )
                    .await;
                return;
            }

            session
                .send_event(
                    turn_context.as_ref(),
                    EventMsg::ThreadNameUpdated(ThreadNameUpdatedEvent {
                        thread_id: session.conversation_id,
                        thread_name: Some(thread_name),
                    }),
                )
                .await;
        });
    }

    async fn maybe_auto_post_turn_completion_review(
        self: &Arc<Self>,
        turn_context: Arc<TurnContext>,
        last_agent_message: Option<String>,
        task_kind: TaskKind,
    ) {
        if task_kind != TaskKind::Regular
            || !turn_context
                .features
                .enabled(Feature::AutoPostTurnCompletionReview)
            || matches!(turn_context.session_source, SessionSource::SubAgent(_))
        {
            return;
        }

        let Some(message) = last_agent_message.as_ref() else {
            return;
        };
        if message.trim().is_empty() || self.active_turn.lock().await.is_some() {
            return;
        }

        if self
            .last_completed_regular_turn_for_review()
            .await
            .is_none()
        {
            return;
        }

        self.submit_internal_op(
            self.next_internal_sub_id_with_prefix("post-turn-review"),
            crate::protocol::Op::ReviewCompletedTurn,
        )
        .await;
    }

    async fn continue_after_post_turn_completion_review(
        self: &Arc<Self>,
        _completed_review_context: Arc<TurnContext>,
    ) {
        let Some(checkpoint) = self
            .take_pending_post_turn_completion_review_continuation()
            .await
        else {
            return;
        };

        self.set_pending_continuation(Some(checkpoint)).await;
        self.submit_internal_op(
            self.next_internal_sub_id_with_prefix("post-turn-review-continuation"),
            crate::protocol::Op::Continue,
        )
        .await;
    }

    async fn register_new_active_task(&self, task: RunningTask) {
        let mut active = self.active_turn.lock().await;
        let mut turn = ActiveTurn::default();
        turn.add_task(task);
        *active = Some(turn);
    }

    async fn take_all_running_tasks(&self) -> Vec<RunningTask> {
        let mut active = self.active_turn.lock().await;
        match active.take() {
            Some(mut at) => {
                at.clear_pending().await;

                at.drain_tasks()
            }
            None => Vec::new(),
        }
    }

    async fn close_unified_exec_processes(&self) {
        self.services
            .unified_exec_manager
            .terminate_all_processes()
            .await;
    }

    async fn handle_task_abort(self: &Arc<Self>, task: RunningTask, reason: TaskStopReason) {
        let sub_id = task.turn_context.sub_id.clone();
        if task.cancellation_token.is_cancelled() {
            return;
        }

        trace!(task_kind = ?task.kind, sub_id, "aborting running task");
        task.cancellation_token.cancel();
        let session_task = task.task;

        select! {
            _ = task.done.notified() => {
            },
            _ = tokio::time::sleep(Duration::from_millis(GRACEFULL_INTERRUPTION_TIMEOUT_MS)) => {
                warn!("task {sub_id} didn't complete gracefully after {}ms", GRACEFULL_INTERRUPTION_TIMEOUT_MS);
            }
        }

        task.handle.abort();

        let session_ctx = Arc::new(SessionTaskContext::new(Arc::clone(self)));
        session_task
            .abort(session_ctx, Arc::clone(&task.turn_context), reason.clone())
            .await;

        match reason {
            TaskStopReason::Abort(TurnAbortReason::Interrupted) => {
                self.set_pending_continuation(Some(PendingContinuation {
                    source: TurnContinuationSource::Interrupted,
                    continued_from_turn_id: Some(sub_id.clone()),
                    model: Some(task.turn_context.model_info.slug.clone()),
                    pause_reason: None,
                    target: PendingContinuationTarget::Regular,
                }))
                .await;

                let marker = ResponseItem::Message {
                    id: None,
                    role: "user".to_string(),
                    content: vec![ContentItem::InputText {
                        text: format!(
                            "{TURN_ABORTED_OPEN_TAG}\n{TURN_ABORTED_INTERRUPTED_GUIDANCE}\n</turn_aborted>"
                        ),
                    }],
                    end_turn: None,
                    phase: None,
                };
                self.record_into_history(std::slice::from_ref(&marker), task.turn_context.as_ref())
                    .await;
                self.persist_rollout_items(&[RolloutItem::ResponseItem(marker)])
                    .await;
                // Ensure the marker is durably visible before emitting TurnAborted: some clients
                // synchronously re-read the rollout on receipt of the abort event.
                self.flush_rollout().await;

                let event = EventMsg::TurnAborted(TurnAbortedEvent {
                    reason: TurnAbortReason::Interrupted,
                });
                self.send_event(task.turn_context.as_ref(), event).await;
            }
            TaskStopReason::Abort(reason) => {
                self.clear_pending_continuation().await;
                let event = EventMsg::TurnAborted(TurnAbortedEvent { reason });
                self.send_event(task.turn_context.as_ref(), event).await;
            }
            TaskStopReason::Pause(reason) => {
                let target = match task.kind {
                    TaskKind::Regular => Some(PendingContinuationTarget::Regular),
                    TaskKind::PostTurnCompletionReview => {
                        Some(PendingContinuationTarget::PostTurnCompletionReview)
                    }
                    TaskKind::Review | TaskKind::Compact | TaskKind::UserShell => None,
                };
                let Some(target) = target else {
                    self.clear_pending_continuation().await;
                    self.send_event(
                        task.turn_context.as_ref(),
                        EventMsg::Error(ErrorEvent {
                            message: "Pause is not available for this task.".to_string(),
                            codex_error_info: Some(CodexErrorInfo::BadRequest),
                        }),
                    )
                    .await;
                    return;
                };
                self.set_pending_continuation(Some(PendingContinuation {
                    source: TurnContinuationSource::Paused,
                    continued_from_turn_id: Some(sub_id.clone()),
                    model: Some(task.turn_context.model_info.slug.clone()),
                    pause_reason: Some(reason.clone()),
                    target,
                }))
                .await;
                self.send_event_raw_flushed(Event {
                    id: sub_id.clone(),
                    msg: EventMsg::TurnPaused(TurnPausedEvent {
                        turn_id: sub_id,
                        reason,
                    }),
                })
                .await;
            }
        }
    }
}

#[cfg(test)]
mod tests {}
