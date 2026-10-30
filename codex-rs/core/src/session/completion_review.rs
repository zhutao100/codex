use super::*;
use crate::completion_review::AUTOMATIC_REVIEW_LIMIT;
use crate::completion_review::CompletionReviewState;
use crate::completion_review::InternalReviewOperation;
use crate::completion_review::ReviewHandoff;
use crate::completion_review::ReviewHandoffDisposition;
use crate::completion_review::ReviewInputInstallation;
use crate::completion_review::ReviewTicket;

impl Session {
    pub(super) async fn completion_review_transaction<T, F, Fut>(
        self: &Arc<Self>,
        install: F,
    ) -> Result<T, tokio::task::JoinError>
    where
        T: Send + 'static,
        F: FnOnce(Arc<Self>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = T> + Send + 'static,
    {
        let guard = Arc::clone(&self.completion_review_installation)
            .lock_owned()
            .await;
        let session = Arc::clone(self);
        // Task-stop joins this guard even if the caller is cancelled.
        tokio::spawn(async move {
            let _guard = guard;
            install(session).await
        })
        .await
    }

    async fn persist_completion_review_locked(&self) -> bool {
        let snapshot = self.state.lock().await.completion_review.clone();
        let recorder = self.services.rollout.lock().await.clone();
        let result = async {
            let item = RolloutItem::PostTurnCompletionReviewState(
                serde_json::to_value(snapshot).map_err(std::io::Error::other)?,
            );
            if let Some(recorder) = recorder {
                recorder.record_items(&[item]).await?;
                recorder.flush().await?;
            }
            Ok::<_, std::io::Error>(())
        }
        .await;
        if let Err(error) = result {
            self.state
                .lock()
                .await
                .completion_review
                .request_frame
                .exact = false;
            warn!("failed to persist completion review state: {error}");
            self.send_event_raw(Event {
                id: self.next_internal_sub_id_with_prefix("completion-review-unavailable"),
                msg: EventMsg::Warning(WarningEvent {
                    message: "Completion review state could not be persisted. Automatic review and continuation are unavailable for this request.".into(),
                }),
            }).await;
            return false;
        }
        true
    }

    pub(crate) async fn accept_completion_review_input(
        &self,
        input: &[UserInput],
        new_task: bool,
    ) -> u64 {
        self.accept_completion_review_input_with_client_id(input, new_task, None)
            .await
    }

    pub(crate) async fn accept_completion_review_input_with_client_id(
        &self,
        input: &[UserInput],
        new_task: bool,
        client_id: Option<String>,
    ) -> u64 {
        let _guard = self.completion_review_installation.lock().await;
        let revision = self.state.lock().await.completion_review.accept_input(
            new_task,
            input.to_vec(),
            client_id,
        );
        self.persist_completion_review_locked().await;
        revision
    }

    pub(crate) async fn install_completion_review_input(
        self: &Arc<Self>,
        context: &Arc<TurnContext>,
        revision: u64,
        consume: bool,
    ) -> bool {
        let context = Arc::clone(context);
        self.completion_review_transaction(move |session| async move {
            let input = session
                .state
                .lock()
                .await
                .completion_review
                .pending_inputs
                .iter()
                .find(|input| input.revision == revision)
                .cloned();
            let Some(input) = input else {
                return true;
            };
            if input.installation != ReviewInputInstallation::Recorded {
                if let Some(input) = session
                    .state
                    .lock()
                    .await
                    .completion_review
                    .pending_inputs
                    .iter_mut()
                    .find(|input| input.revision == revision)
                {
                    input.installation = ReviewInputInstallation::Installing;
                }
                // This marker disambiguates identical messages if the process
                // stops after writing the body but before its Recorded snapshot.
                if !session.persist_completion_review_locked().await {
                    return false;
                }
                session
                    .record_user_prompt_and_emit_turn_item(
                        &context,
                        &input.content,
                        input.response_item,
                        input.client_id,
                    )
                    .await;
                if let Some(input) = session
                    .state
                    .lock()
                    .await
                    .completion_review
                    .pending_inputs
                    .iter_mut()
                    .find(|input| input.revision == revision)
                {
                    input.installation = ReviewInputInstallation::Recorded;
                }
                if !session.persist_completion_review_locked().await {
                    return false;
                }
            }
            if consume {
                session
                    .state
                    .lock()
                    .await
                    .completion_review
                    .consume_input(revision);
                return session.persist_completion_review_locked().await;
            }
            true
        })
        .await
        .unwrap_or(false)
    }

    pub(crate) async fn start_completion_review_main_task(&self, turn_id: &str) {
        let _guard = self.completion_review_installation.lock().await;
        self.state.lock().await.completion_review.main_task_id = Some(turn_id.to_string());
        self.persist_completion_review_locked().await;
    }

    pub(crate) async fn completion_review_main_task_is_current(&self, turn_id: &str) -> bool {
        self.state
            .lock()
            .await
            .completion_review
            .main_task_id
            .as_deref()
            == Some(turn_id)
    }

    pub(crate) async fn initial_completion_review_input_revision(
        &self,
        turn_id: &str,
    ) -> Option<u64> {
        self.active_turn.lock().await.as_ref().and_then(|active| {
            active
                .tasks
                .get(turn_id)
                .and_then(|task| task.review_input_revision)
        })
    }

    pub(crate) async fn install_pending_completion_review_input(
        self: &Arc<Self>,
        context: &Arc<TurnContext>,
        input: TurnInput,
        consume: bool,
    ) -> bool {
        match input {
            TurnInput::UserInput {
                review_revision: Some(revision),
                ..
            } => {
                self.install_completion_review_input(context, revision, consume)
                    .await
            }
            input => {
                let context = Arc::clone(context);
                self.completion_review_transaction(move |session| async move {
                    session.record_pending_input(&context, input).await;
                    session.persist_completion_review_locked().await
                })
                .await
                .unwrap_or(false)
            }
        }
    }

    pub(crate) async fn retain_completion_review_input(
        self: &Arc<Self>,
        context: &Arc<TurnContext>,
        input: TurnInput,
    ) {
        self.install_pending_completion_review_input(context, input, false)
            .await;
    }

    pub(crate) async fn completion_review_input_recovery_revisions(&self) -> Vec<u64> {
        let active = self.active_turn.lock().await;
        let mut live_revisions = Vec::new();
        if let Some(active) = active.as_ref() {
            live_revisions.extend(
                active
                    .tasks
                    .values()
                    .filter_map(|task| task.review_input_revision),
            );
            live_revisions.extend(
                active
                    .turn_state
                    .lock()
                    .await
                    .pending_completion_review_revisions(),
            );
        }
        self.state
            .lock()
            .await
            .completion_review
            .pending_inputs
            .iter()
            .filter(|input| !live_revisions.contains(&input.revision))
            .map(|input| input.revision)
            .collect()
    }

    pub(crate) async fn completed_turn_for_review(&self) -> Option<CompletedTurnForReview> {
        self.state
            .lock()
            .await
            .completion_review
            .eligible_episode()
            .cloned()
    }

    pub(crate) async fn capture_completed_regular_turn_for_review(
        &self,
        turn_context: &TurnContext,
        final_agent_message: Option<&str>,
    ) {
        let Some(text) = final_agent_message.filter(|text| !text.trim().is_empty()) else {
            return;
        };
        let _guard = self.completion_review_installation.lock().await;
        if self
            .state
            .lock()
            .await
            .completion_review
            .main_task_id
            .as_deref()
            != Some(&turn_context.sub_id)
        {
            return;
        }
        self.state.lock().await.completion_review.capture(
            turn_context.sub_id.clone(),
            turn_context.cwd.clone(),
            text.to_string(),
        );
        self.persist_completion_review_locked().await;
    }

    pub(crate) async fn begin_completion_review(&self, automatic: bool) -> Option<ReviewTicket> {
        self.begin_completion_review_for_candidate(automatic, None)
            .await
    }

    pub(crate) async fn begin_completion_review_for_candidate(
        &self,
        automatic: bool,
        expected_completion: Option<&str>,
    ) -> Option<ReviewTicket> {
        let _guard = self.completion_review_installation.lock().await;
        if let Some(expected) = expected_completion
            && self
                .state
                .lock()
                .await
                .completion_review
                .eligible_episode()?
                .turn_id
                != expected
        {
            return None;
        }
        let ticket = self
            .state
            .lock()
            .await
            .completion_review
            .begin_review(automatic)?;
        self.persist_completion_review_locked()
            .await
            .then_some(ticket)
    }

    pub(crate) async fn completion_review_ticket_is_current(&self, ticket: &ReviewTicket) -> bool {
        self.state
            .lock()
            .await
            .completion_review
            .ticket_is_current(ticket)
    }

    pub(crate) async fn current_completion_review_ticket(&self) -> Option<ReviewTicket> {
        let state = self.state.lock().await;
        state
            .completion_review
            .attempt
            .clone()
            .filter(|ticket| state.completion_review.ticket_is_current(ticket))
    }

    pub(crate) async fn finish_completion_review(&self, ticket: &ReviewTicket) {
        let _guard = self.completion_review_installation.lock().await;
        let mut state = self.state.lock().await;
        if !state.completion_review.ticket_is_current(ticket) {
            return;
        }
        state.completion_review.clear_operations();
        drop(state);
        self.persist_completion_review_locked().await;
    }

    pub(crate) async fn mark_completion_review_clarification_unavailable(&self, turn_id: &str) {
        let _guard = self.completion_review_installation.lock().await;
        let mut state = self.state.lock().await;
        let review = &mut state.completion_review;
        if review.main_task_id.as_deref() != Some(turn_id) {
            return;
        }
        review.request_frame.exact = false;
        review.clear_operations();
        drop(state);
        self.persist_completion_review_locked().await;
        self.send_event_raw(Event {
            id: turn_id.to_string(),
            msg: EventMsg::Warning(WarningEvent {
                message: "Full completion review is unavailable: human clarification answers from request_user_input are not represented in the frozen request snapshot. The answer will not be declared fully reviewed.".into(),
            }),
        }).await;
    }

    pub(crate) async fn prepare_completion_review_handoff(
        self: &Arc<Self>,
        ticket: &ReviewTicket,
        advisory: String,
        model: String,
    ) -> bool {
        let ticket = ticket.clone();
        self.completion_review_transaction(move |session| async move {
            session
                .prepare_completion_review_handoff_locked(&ticket, advisory, model)
                .await
        })
        .await
        .unwrap_or(false)
    }

    async fn prepare_completion_review_handoff_locked(
        &self,
        ticket: &ReviewTicket,
        advisory: String,
        model: String,
    ) -> bool {
        let mut state = self.state.lock().await;
        let cwd_matches = state
            .completion_review
            .eligible_episode()
            .is_some_and(|episode| episode.cwd == state.session_configuration.cwd);
        let review = &mut state.completion_review;
        if !cwd_matches
            || !review.ticket_is_current(ticket)
            || review.handoff.is_some()
            || (ticket.automatic
                && review.episode.as_ref().is_some_and(|episode| {
                    episode.automatic_reviews_started >= AUTOMATIC_REVIEW_LIMIT
                }))
        {
            return false;
        }
        review.handoff = Some(ReviewHandoff {
            ticket: ticket.clone(),
            advisory,
            model,
            disposition: ReviewHandoffDisposition::Prepared,
            advisory_recorded: false,
        });
        drop(state);
        self.persist_completion_review_locked().await
    }

    pub(crate) async fn claim_completion_review_handoff(
        &self,
        ticket: &ReviewTicket,
        turn_id: &str,
    ) -> Option<PendingContinuation> {
        let _guard = self.completion_review_installation.lock().await;
        let mut state = self.state.lock().await;
        let cwd_matches = state
            .completion_review
            .eligible_episode()
            .is_some_and(|episode| episode.cwd == state.session_configuration.cwd);
        let review = &mut state.completion_review;
        if !cwd_matches || !review.ticket_is_current(ticket) {
            return None;
        }
        let handoff = review.handoff.as_mut()?;
        if handoff.ticket != *ticket || handoff.disposition != ReviewHandoffDisposition::Prepared {
            return None;
        }
        handoff.disposition = ReviewHandoffDisposition::Started {
            turn_id: turn_id.to_string(),
        };
        let checkpoint = PendingContinuation {
            source: TurnContinuationSource::PostTurnCompletionReview,
            continued_from_turn_id: Some(ticket.candidate_completion_id.clone()),
            model: Some(handoff.model.clone()),
            pause_reason: None,
            target: PendingContinuationTarget::Regular,
        };
        review.replacement_answer_required = true;
        drop(state);
        self.persist_completion_review_locked()
            .await
            .then_some(checkpoint)
    }

    pub(crate) async fn queue_completion_review_operation(
        &self,
        operation: InternalReviewOperation,
    ) {
        let op = match &operation {
            InternalReviewOperation::Review(_) => Op::ReviewCompletedTurn,
            InternalReviewOperation::Continue(_) => Op::Continue,
        };
        let id = format!("completion-review-{}", uuid::Uuid::new_v4());
        self.state
            .lock()
            .await
            .internal_review_operations
            .insert(id.clone(), operation);
        if self
            .tx_sub
            .try_send(Submission { id: id.clone(), op })
            .is_err()
        {
            let operation = self
                .state
                .lock()
                .await
                .internal_review_operations
                .remove(&id);
            if let Some(InternalReviewOperation::Review(ticket)) = operation
                && self.completion_review_ticket_is_current(&ticket).await
            {
                self.set_pending_continuation(Some(PendingContinuation {
                    source: TurnContinuationSource::Interrupted,
                    continued_from_turn_id: Some(ticket.candidate_completion_id),
                    model: None,
                    pause_reason: None,
                    target: PendingContinuationTarget::PostTurnCompletionReview,
                }))
                .await;
            }
            self.send_event_raw(Event {
                id,
                msg: EventMsg::Warning(WarningEvent {
                    message: "Completion review work could not be queued. Recover it with /continue or resume; no clean verdict is implied.".into(),
                }),
            }).await;
        }
    }

    pub(crate) async fn take_completion_review_operation(
        &self,
        id: &str,
    ) -> Option<InternalReviewOperation> {
        self.state
            .lock()
            .await
            .internal_review_operations
            .remove(id)
    }

    pub(crate) async fn queue_prepared_completion_review_handoff(&self) {
        let ticket = {
            let state = self.state.lock().await;
            state
                .completion_review
                .handoff
                .as_ref()
                .filter(|handoff| {
                    handoff.disposition == ReviewHandoffDisposition::Prepared
                        && state.completion_review.ticket_is_current(&handoff.ticket)
                })
                .map(|handoff| handoff.ticket.clone())
        };
        if let Some(ticket) = ticket {
            self.queue_completion_review_operation(InternalReviewOperation::Continue(ticket))
                .await;
        }
    }

    pub(crate) async fn completion_review_main_instructions(
        &self,
        include_reference: bool,
    ) -> Option<String> {
        let state = self.state.lock().await;
        let review = &state.completion_review;
        if !review.replacement_answer_required {
            return None;
        }
        let Some(handoff) = review.handoff.as_ref() else {
            return Some(format!(
                "{}\nConsumed user request context (host snapshot):\n{}",
                crate::tasks::post_turn_completion_review::REPLACEMENT_ANSWER_CONTRACT,
                serde_json::to_string(&review.request_frame).ok()?
            ));
        };
        if !matches!(
            handoff.disposition,
            ReviewHandoffDisposition::Started { .. }
        ) || !review.ticket_is_current(&handoff.ticket)
        {
            return None;
        }
        let episode = review.eligible_episode()?;
        let advisory: ResponseItem = DeveloperInstructions::new(handoff.advisory.clone()).into();
        let instructions = if state.history.raw_items().contains(&advisory) {
            crate::tasks::post_turn_completion_review::REPLACEMENT_ANSWER_CONTRACT
        } else {
            &handoff.advisory
        };
        Some(format!(
            "{}\n{}",
            instructions,
            crate::tasks::post_turn_completion_review::render_completed_turn_context(
                episode,
                include_reference
            )
        ))
    }

    pub(crate) async fn record_completion_review_advisory(
        self: &Arc<Self>,
        context: &Arc<TurnContext>,
    ) {
        let context = Arc::clone(context);
        let _ = self
            .completion_review_transaction(move |session| async move {
                session
                    .record_completion_review_advisory_locked(&context)
                    .await;
            })
            .await;
    }

    async fn record_completion_review_advisory_locked(&self, context: &TurnContext) {
        let handoff = self.state.lock().await.completion_review.handoff.clone();
        let Some(handoff) = handoff.filter(|handoff| {
            !handoff.advisory_recorded
                && matches!(
                    handoff.disposition,
                    ReviewHandoffDisposition::Started { .. }
                )
        }) else {
            return;
        };
        // ResponseItem IDs are not serialized. Compare the exact host-owned
        // developer item when recovering a write made before its state flag.
        let item: ResponseItem = DeveloperInstructions::new(handoff.advisory).into();
        let exists = self.clone_history().await.raw_items().contains(&item);
        if !exists {
            self.record_conversation_items(context, &[item]).await;
        }
        if let Some(handoff) = self.state.lock().await.completion_review.handoff.as_mut() {
            handoff.advisory_recorded = true;
        }
        self.persist_completion_review_locked().await;
    }

    pub(crate) async fn stop_completion_review_operations(&self) {
        let _guard = self.completion_review_installation.lock().await;
        let mut state = self.state.lock().await;
        state.completion_review.clear_operations();
        if state.pending_continuation.as_ref().is_some_and(|pending| {
            pending.target == PendingContinuationTarget::PostTurnCompletionReview
        }) {
            state.pending_continuation = None;
        }
        drop(state);
        self.persist_completion_review_locked().await;
    }

    pub(crate) async fn pause_completion_review_operations(&self, reviewer: bool) {
        let _guard = self.completion_review_installation.lock().await;
        if reviewer {
            self.state.lock().await.completion_review.handoff = None;
        }
        self.persist_completion_review_locked().await;
    }

    pub(crate) async fn recover_completion_review(&self) {
        self.queue_prepared_completion_review_handoff().await;
    }

    pub(crate) async fn invalidate_completion_review_history(&self) {
        let _guard = self.completion_review_installation.lock().await;
        let mut state = self.state.lock().await;
        let exact = !state
            .history
            .raw_items()
            .iter()
            .any(is_user_turn_boundary_response_item);
        state.completion_review = CompletionReviewState::default();
        state.completion_review.request_frame.exact = exact;
        drop(state);
        self.persist_completion_review_locked().await;
    }

    pub(crate) async fn persist_completion_review(&self) {
        let _guard = self.completion_review_installation.lock().await;
        self.persist_completion_review_locked().await;
    }
}

pub(super) fn completion_review_for_fork(items: &[RolloutItem]) -> Option<CompletionReviewState> {
    let mut generation = None;
    for item in items.iter().rev() {
        match item {
            RolloutItem::PostTurnCompletionReviewState(value) => {
                let state = CompletionReviewState::from_rollout(value)?;
                if generation
                    .as_ref()
                    .is_some_and(|generation| generation != &state.history_generation)
                {
                    return None;
                }
                generation = Some(state.history_generation.clone());
                if state.eligible_episode().is_some() {
                    return Some(state);
                }
            }
            RolloutItem::ResponseItem(item)
                if is_user_turn_boundary_response_item(item)
                    && !super::turn::is_review_rollout_user_message(item) =>
            {
                return None;
            }
            RolloutItem::EventMsg(EventMsg::ThreadRolledBack(_)) => return None,
            _ => {}
        }
    }
    None
}
