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
        let snapshot = {
            let state = self.state.lock().await;
            if !state.session_configuration.tracks_completion_review() {
                return true;
            }
            state.completion_review.clone()
        };
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
        self.check_completion_review_persistence(result).await
    }

    async fn check_completion_review_persistence(&self, result: std::io::Result<()>) -> bool {
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
        main_task_id: Option<&str>,
    ) -> Option<u64> {
        self.accept_completion_review_input_with_client_id(input, new_task, None, main_task_id)
            .await
    }

    pub(crate) async fn accept_completion_review_input_with_client_id(
        &self,
        input: &[UserInput],
        new_task: bool,
        client_id: Option<String>,
        main_task_id: Option<&str>,
    ) -> Option<u64> {
        let _guard = self.completion_review_installation.lock().await;
        let revision = {
            let mut state = self.state.lock().await;
            if !state.session_configuration.tracks_completion_review() {
                return None;
            }
            let revision =
                state
                    .completion_review
                    .accept_input(new_task, input.to_vec(), client_id);
            if let Some(main_task_id) = main_task_id {
                state.completion_review.main_task_id = Some(main_task_id.to_string());
            }
            revision
        };
        self.persist_completion_review_locked().await;
        Some(revision)
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
            let needs_installation = input.installation != ReviewInputInstallation::Recorded;
            if needs_installation {
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
                // stops after the body but before checkpointing installation.
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
                if consume {
                    let recorder = session.services.rollout.lock().await.clone();
                    let result = match recorder {
                        Some(recorder) => recorder.flush().await,
                        None => Ok(()),
                    };
                    if !session.check_completion_review_persistence(result).await {
                        return false;
                    }
                }
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
            }
            if consume {
                session
                    .state
                    .lock()
                    .await
                    .completion_review
                    .consume_input(revision);
            }
            if needs_installation || consume {
                session.persist_completion_review_locked().await
            } else {
                true
            }
        })
        .await
        .unwrap_or(false)
    }

    pub(crate) async fn start_completion_review_main_task(&self, turn_id: &str) -> bool {
        let _guard = self.completion_review_installation.lock().await;
        {
            let mut state = self.state.lock().await;
            if !state.session_configuration.tracks_completion_review() {
                return true;
            }
            state.completion_review.main_task_id = Some(turn_id.to_string());
        }
        self.persist_completion_review_locked().await
    }

    pub(crate) async fn retarget_pending_regular_continuation_model(&self, model: &str) {
        let _guard = self.completion_review_installation.lock().await;
        let mut state = self.state.lock().await;
        let Some(checkpoint) = state
            .pending_continuation
            .as_mut()
            .filter(|checkpoint| checkpoint.target == PendingContinuationTarget::Regular)
        else {
            return;
        };
        checkpoint.model = Some(model.to_string());
        if !state.completion_review.replacement_answer_required {
            return;
        }
        state.completion_review.recovery_model = Some(model.to_string());
        if let Some(handoff) = state.completion_review.handoff.as_mut().filter(|handoff| {
            matches!(
                handoff.disposition,
                ReviewHandoffDisposition::Started { .. }
            )
        }) {
            model.clone_into(&mut handoff.model);
        }
        drop(state);
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
        let state = self.state.lock().await;
        if !state.session_configuration.tracks_completion_review() {
            return None;
        }
        state.completion_review.eligible_episode().cloned()
    }

    pub(crate) async fn capture_completed_regular_turn_for_review(
        &self,
        turn_context: &TurnContext,
        final_agent_message: Option<&str>,
    ) {
        let Some(text) = final_agent_message.filter(|text| !text.trim().is_empty()) else {
            return;
        };
        let reserve_automatic = turn_context
            .features
            .enabled(Feature::AutoPostTurnCompletionReview)
            && !matches!(turn_context.session_source, SessionSource::SubAgent(_))
            && self.active_turn.lock().await.is_none();
        let _guard = self.completion_review_installation.lock().await;
        let mut state = self.state.lock().await;
        if !state.session_configuration.tracks_completion_review()
            || state.completion_review.main_task_id.as_deref() != Some(&turn_context.sub_id)
        {
            return;
        }
        state.completion_review.capture(
            turn_context.sub_id.clone(),
            turn_context.cwd.clone(),
            text.to_string(),
        );
        if reserve_automatic {
            let _ = state.completion_review.begin_review(true);
        }
        drop(state);
        self.persist_completion_review_locked().await;
    }

    #[cfg(test)]
    pub(crate) async fn begin_completion_review(&self, automatic: bool) -> Option<ReviewTicket> {
        self.begin_completion_review_for_candidate(automatic)
            .await
            .map(|(_, ticket)| ticket)
    }

    pub(crate) async fn reserve_manual_completion_review(
        &self,
    ) -> Option<(CompletedTurnForReview, ReviewTicket)> {
        let _lifecycle = self.task_lifecycle.lock().await;
        if self.active_turn.lock().await.is_some() {
            return None;
        }
        self.begin_completion_review_for_candidate(false).await
    }

    async fn begin_completion_review_for_candidate(
        &self,
        automatic: bool,
    ) -> Option<(CompletedTurnForReview, ReviewTicket)> {
        let _guard = self.completion_review_installation.lock().await;
        let mut state = self.state.lock().await;
        if !state.session_configuration.tracks_completion_review() {
            return None;
        }
        let review = &mut state.completion_review;
        let ticket = review.begin_review(automatic)?;
        let completed_turn = review.eligible_episode()?.clone();
        drop(state);
        self.persist_completion_review_locked()
            .await
            .then_some((completed_turn, ticket))
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
        review.recovery_model = checkpoint.model.clone();
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
    ) -> Result<Option<String>, CodexErr> {
        let state = self.state.lock().await;
        let review = &state.completion_review;
        if !review.replacement_answer_required {
            return Ok(None);
        }
        let Some(handoff) = review.handoff.as_ref() else {
            return Ok(Some(format!(
                "{}\nConsumed user request context (host snapshot):\n{}",
                crate::tasks::post_turn_completion_review::REPLACEMENT_ANSWER_CONTRACT,
                serde_json::to_string(&review.request_frame).map_err(|error| {
                    CodexErr::Fatal(format!(
                        "Could not serialize review follow-up scope: {error}"
                    ))
                })?
            )));
        };
        if !matches!(
            handoff.disposition,
            ReviewHandoffDisposition::Started { .. }
        ) || !review.ticket_is_current(&handoff.ticket)
        {
            return Err(CodexErr::Fatal(
                "Review follow-up snapshots are unavailable. Repair persistence and resume from the last durable rollout, or supply a new complete request. No incomplete follow-up request was sent."
                    .into(),
            ));
        }
        let episode = review
            .eligible_episode()
            .ok_or_else(|| CodexErr::Fatal("Review follow-up candidate is unavailable.".into()))?;
        let advisory: ResponseItem = DeveloperInstructions::new(handoff.advisory.clone()).into();
        let instructions = if state.history.raw_items().contains(&advisory) {
            crate::tasks::post_turn_completion_review::REPLACEMENT_ANSWER_CONTRACT
        } else {
            &handoff.advisory
        };
        Ok(Some(format!(
            "{}\n{}",
            instructions,
            crate::tasks::post_turn_completion_review::render_completed_turn_context(
                episode,
                include_reference
            )
        )))
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
        let started_main = state
            .completion_review
            .handoff
            .as_ref()
            .is_some_and(|handoff| {
                matches!(
                    handoff.disposition,
                    ReviewHandoffDisposition::Started { .. }
                )
            });
        // A started handoff is resumable main work, not queued reviewer work.
        if !started_main {
            state.completion_review.clear_operations();
        }
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
        state.completion_review = CompletionReviewState::for_unknown_history();
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
                // Lost provenance also invalidates older candidates in this generation.
                if !state.request_frame.exact {
                    return None;
                }
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
