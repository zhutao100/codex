use std::sync::Arc;

use super::SessionTask;
use super::SessionTaskContext;
use crate::compact::CompactionContext;
use crate::compact::CompactionImplementation;
use crate::compact::InitialContextInjection;
use crate::compact::compaction_implementation;
use crate::compact_remote_v2::CompactionReason;
use crate::protocol::EventMsg;
use crate::protocol::TurnStartedEvent;
use crate::session::turn;
use crate::session::turn_context::TurnContext;
use crate::state::TaskKind;
use codex_protocol::user_input::UserInput;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Copy, Default)]
pub(crate) struct CompactTask;

impl SessionTask for CompactTask {
    fn kind(&self) -> TaskKind {
        TaskKind::Compact
    }

    async fn run(
        self: Arc<Self>,
        session: Arc<SessionTaskContext>,
        ctx: Arc<TurnContext>,
        input: Vec<UserInput>,
        cancellation_token: CancellationToken,
    ) -> Option<String> {
        let session = session.clone_session();
        let (checkpoint_source, ctx) =
            match turn::manual_checkpoint_contexts(&session, &ctx, &cancellation_token).await {
                Ok(retry) => retry,
                Err(error) => {
                    session
                        .send_event(&ctx, EventMsg::Error(error.to_error_event(None)))
                        .await;
                    return None;
                }
            };
        if let Some(source) = checkpoint_source {
            session
                .send_event(
                    &ctx,
                    EventMsg::TurnStarted(TurnStartedEvent {
                        model_context_window: ctx.model_context_window(),
                        collaboration_mode_kind: ctx.collaboration_mode.mode,
                    }),
                )
                .await;
            if let Err(error) = turn::run_pre_turn_compact(
                &session,
                &source,
                &ctx,
                InitialContextInjection::BeforeLastUserMessage,
                &cancellation_token,
                None,
            )
            .await
            {
                tracing::info!("Recovering manual checkpoint failed: {error:#}");
            }
            return None;
        }
        // Only a standalone compact may seed its context. A retry above
        // installs the saved destination at the checkpoint's own placement.
        if let Err(error) = session.seed_initial_context_if_needed(&ctx).await {
            session
                .send_event(&ctx, EventMsg::Error(error.to_error_event(None)))
                .await;
            return None;
        }
        let mut context = CompactionContext::new(&ctx);
        context.source_fit = turn::compaction_source_fit(&session, &ctx).await;
        context.review_frames = match session
            .compaction_review_frames(&ctx, &cancellation_token)
            .await
        {
            Ok(frame) => frame,
            Err(error) => {
                session
                    .send_event(&ctx, EventMsg::Error(error.to_error_event(None)))
                    .await;
                return None;
            }
        };
        let implementation = compaction_implementation(&ctx);
        ctx.otel_manager.counter(
            "codex.task.compact",
            1,
            &[("type", implementation.metric_label())],
        );
        match implementation {
            CompactionImplementation::RemoteV2 => {
                let _ = crate::compact_remote_v2::run_remote_compact_task(
                    &session,
                    &context,
                    None,
                    None,
                    InitialContextInjection::DoNotInject,
                    CompactionReason::UserRequested,
                    &cancellation_token,
                )
                .await;
            }
            CompactionImplementation::Remote => {
                let _ = crate::compact_remote::run_remote_compact_task(session, &context).await;
            }
            CompactionImplementation::Local => {
                let _ = crate::compact::run_compact_task(session, &context, input).await;
            }
        }

        None
    }
}
