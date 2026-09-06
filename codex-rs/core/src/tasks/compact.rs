use std::sync::Arc;

use super::SessionTask;
use super::SessionTaskContext;
use crate::compact::CompactionImplementation;
use crate::compact::InitialContextInjection;
use crate::compact::compaction_implementation;
use crate::compact_remote_v2::CompactionReason;
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
        let implementation = compaction_implementation(&session, &ctx.provider);
        session.services.otel_manager.counter(
            "codex.task.compact",
            1,
            &[("type", implementation.metric_label())],
        );
        match implementation {
            CompactionImplementation::RemoteV2 => {
                let _ = crate::compact_remote_v2::run_remote_compact_task(
                    &session,
                    &ctx,
                    None,
                    None,
                    InitialContextInjection::DoNotInject,
                    CompactionReason::UserRequested,
                    &cancellation_token,
                )
                .await;
            }
            CompactionImplementation::Remote => {
                let _ = crate::compact_remote::run_remote_compact_task(session, ctx).await;
            }
            CompactionImplementation::Local => {
                let _ = crate::compact::run_compact_task(session, ctx, input).await;
            }
        }

        None
    }
}
