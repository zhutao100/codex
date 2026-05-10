use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::session::session::Session;
use crate::session::turn::continue_turn;
use crate::session::turn_context::TurnContext;
use crate::state::PendingContinuation;
use crate::state::TaskKind;
use crate::tasks::SessionTask;
use crate::tasks::SessionTaskContext;
use codex_protocol::user_input::UserInput;

pub(crate) struct ContinueTask {
    checkpoint: PendingContinuation,
}

impl ContinueTask {
    pub(crate) fn new(checkpoint: PendingContinuation) -> Self {
        Self { checkpoint }
    }
}

impl SessionTask for ContinueTask {
    fn kind(&self) -> TaskKind {
        TaskKind::Regular
    }

    async fn run(
        self: Arc<Self>,
        session: Arc<SessionTaskContext>,
        ctx: Arc<TurnContext>,
        _input: Vec<UserInput>,
        cancellation_token: CancellationToken,
    ) -> Option<String> {
        let sess: Arc<Session> = session.clone_session();
        continue_turn(
            sess,
            ctx,
            self.checkpoint.clone(),
            cancellation_token.child_token(),
        )
        .await
    }
}
