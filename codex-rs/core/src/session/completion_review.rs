use super::*;
use crate::completion_review::AutoCompactionDecision;
use crate::completion_review::CompletionReviewReservationError;
use crate::completion_review::CompletionReviewState;
use crate::completion_review::DeferredCaptureSource;
use crate::completion_review::InputContextProjection;
use crate::completion_review::InternalReviewOperation;
use crate::completion_review::ReviewHandoff;
use crate::completion_review::ReviewHandoffDisposition;
use crate::completion_review::ReviewInputInstallation;
use crate::completion_review::ReviewPromptFrame;
use crate::completion_review::ReviewPromptFrames;
use crate::completion_review::ReviewSampling;
use crate::completion_review::ReviewTicket;
use crate::completion_review::WorkNotesCheckpoint;

fn review_input_budget(context: &TurnContext) -> i64 {
    context
        .model_context_window()
        .unwrap_or_default()
        .saturating_sub(8_192)
}

fn review_work_tokens(state: &SessionState, context: &TurnContext) -> i64 {
    state
        .history_tokens_from_usage(&context.model_info.slug)
        .unwrap_or_else(|| local_work_tokens(state, context))
}

fn local_work_tokens(state: &SessionState, context: &TurnContext) -> i64 {
    state
        .history
        .estimate_token_count_with_base_instructions(&state.base_instructions_for_turn(context))
        .unwrap_or_default()
}

fn helper_work_tokens(state: &SessionState, source: &TurnContext) -> i64 {
    state
        .helper_tokens_from_usage(&source.model_info.slug)
        .unwrap_or_else(|| local_work_tokens(state, source))
}

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
                .invalidate_request_provenance();
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
        main_task: Option<&TurnContext>,
    ) -> Option<u64> {
        self.accept_completion_review_input_with_client_id(input, new_task, None, main_task)
            .await
    }

    pub(crate) async fn accept_completion_review_input_with_client_id(
        &self,
        input: &[UserInput],
        new_task: bool,
        client_id: Option<String>,
        main_task: Option<&TurnContext>,
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
            if let Some(context) = main_task {
                state
                    .completion_review
                    .set_main_task(&context.sub_id, &context.model_info.slug);
                state.completion_review.recovery_settings =
                    Some(RegularTaskSettings::from_turn(context));
            }
            revision
        };
        self.persist_completion_review_locked().await;
        Some(revision)
    }

    /// Persist the choice before a helper receipt can replace the measured work
    /// baseline. The installation marker, not a response or validation error, closes it.
    pub(super) async fn checkpoint_auto_compaction(
        &self,
        source: &TurnContext,
        placement: crate::compact::InitialContextInjection,
        source_token_adjustment: i64,
        legacy_attempts: u8,
    ) -> CodexResult<AutoCompactionDecision> {
        let _guard = self.completion_review_installation.lock().await;
        let (decision, previous) = {
            let mut state = self.state.lock().await;
            let slot = &mut state.completion_review.pending_compaction;
            if let Some(saved) = slot.as_ref()
                && saved.notes_remaining.is_some()
            {
                return Ok(saved.clone());
            }
            let mut decision = slot.clone().unwrap_or_else(|| AutoCompactionDecision {
                source: RegularTaskSettings::from_turn(source),
                placement,
                source_token_adjustment,
                notes_remaining: None,
            });
            decision.notes_remaining = Some(if decision.source.captures_notes() {
                crate::compact::WORK_NOTES_CAPTURE_MAX_ATTEMPTS.saturating_sub(legacy_attempts)
            } else {
                0
            });
            let previous = slot.replace(decision.clone());
            (decision, previous)
        };
        if !self.persist_completion_review_locked().await {
            self.state.lock().await.completion_review.pending_compaction = previous;
            return Err(CodexErr::Fatal(
                "Could not persist the automatic compaction decision".into(),
            ));
        }
        Ok(decision)
    }

    pub(super) async fn advance_work_notes(
        &self,
        checkpoint: WorkNotesCheckpoint,
    ) -> CodexResult<()> {
        let _guard = self.completion_review_installation.lock().await;
        let previous = {
            let mut state = self.state.lock().await;
            let Some(decision) = &mut state.completion_review.pending_compaction else {
                return Ok(());
            };
            let Some(remaining) = decision.notes_remaining.filter(|n| *n > 0) else {
                return Ok(());
            };
            decision.notes_remaining = Some(match checkpoint {
                WorkNotesCheckpoint::CompletedResponse => remaining - 1,
                WorkNotesCheckpoint::BeforeCompact => 0,
            });
            remaining
        };
        if !self.persist_completion_review_locked().await {
            if let Some(decision) =
                &mut self.state.lock().await.completion_review.pending_compaction
            {
                decision.notes_remaining = Some(previous);
            }
            return Err(CodexErr::Fatal(
                "Could not persist progress of the work-notes capture".into(),
            ));
        }
        Ok(())
    }

    pub(crate) async fn install_completion_review_input(
        self: &Arc<Self>,
        context: &Arc<TurnContext>,
        revision: u64,
        consume: bool,
    ) -> bool {
        self.install_review_input_with_skills(context, revision, consume, None)
            .await
    }

    pub(super) async fn install_review_input_with_skills(
        self: &Arc<Self>,
        context: &Arc<TurnContext>,
        revision: u64,
        consume: bool,
        selected_skills: Option<Vec<ResponseItem>>,
    ) -> bool {
        let context = Arc::clone(context);
        self.completion_review_transaction(move |session| async move {
            let prepared_context = if selected_skills.is_some() {
                Some(session.plan_input_context(&context).await)
            } else {
                None
            };
            let mut state = session.state.lock().await;
            let Some(pending) = state
                .completion_review
                .pending_inputs
                .iter_mut()
                .find(|input| input.revision == revision)
            else {
                return true;
            };
            let needs_installation = pending.installation != ReviewInputInstallation::Recorded;
            let preparing_skills =
                selected_skills.is_some() && pending.remaining_skill_items.is_none();
            if preparing_skills {
                pending.remaining_skill_items = selected_skills;
                if needs_installation {
                    pending.remaining_context = prepared_context;
                }
            }
            if needs_installation {
                pending.installation = ReviewInputInstallation::Installing;
            }
            let input = pending.clone();
            if preparing_skills {
                state.completion_review.unsampled_fresh_model =
                    Some(context.model_info.slug.clone());
            }
            drop(state);
            // Journal context, prompt and selected skills before the first conversation item.
            if (needs_installation || preparing_skills)
                && !session.persist_completion_review_locked().await
            {
                return false;
            }
            if needs_installation {
                if input.remaining_skill_items.is_some()
                    && !session
                        .finish_fresh_input_context(&context, revision, input.remaining_context)
                        .await
                {
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
            }
            let skills = input.remaining_skill_items.unwrap_or_default();
            if consume && !skills.is_empty() {
                session.record_conversation_items(&context, &skills).await;
            }
            if consume && (needs_installation || !skills.is_empty()) {
                let recorder = session.services.rollout.lock().await.clone();
                let result = match recorder {
                    Some(recorder) => recorder.flush().await,
                    None => Ok(()),
                };
                if !session.check_completion_review_persistence(result).await {
                    return false;
                }
            }
            let mut state = session.state.lock().await;
            if needs_installation
                && let Some(input) = state
                    .completion_review
                    .pending_inputs
                    .iter_mut()
                    .find(|input| input.revision == revision)
            {
                input.remaining_context = None;
                input.installation = ReviewInputInstallation::Recorded;
            }
            if consume {
                state.completion_review.consume_input(revision);
            }
            drop(state);
            if needs_installation || consume {
                session.persist_completion_review_locked().await
            } else {
                true
            }
        })
        .await
        .unwrap_or(false)
    }

    /// Called under the installation lock, before the prompt body. Finish the frozen
    /// prefix first; an explicit settings change on resume can then append its own delta.
    async fn finish_fresh_input_context(
        &self,
        context: &TurnContext,
        revision: u64,
        saved: Option<InputContextProjection>,
    ) -> bool {
        if let Some(saved) = saved {
            self.record_input_context(context, saved).await;
        }
        if let Some(mut installed) = self.state.lock().await.history.reference_context_item() {
            let mut target = context.to_turn_context_item();
            // Rediscovering AGENTS or a different skill catalog at startup is not a
            // settings override for a prompt whose context was already selected.
            installed.user_instructions = None;
            target.user_instructions = None;
            if installed == target {
                return true;
            }
        }
        let correction = self.plan_input_context(context).await;
        {
            let mut state = self.state.lock().await;
            let Some(pending) = state
                .completion_review
                .pending_inputs
                .iter_mut()
                .find(|input| input.revision == revision)
            else {
                return false;
            };
            pending.remaining_context = Some(correction.clone());
        }
        if !self.persist_completion_review_locked().await {
            return false;
        }
        self.record_input_context(context, correction).await;
        true
    }

    pub(crate) async fn start_completion_review_main_task(
        &self,
        turn_id: &str,
        context: &TurnContext,
    ) -> bool {
        let _guard = self.completion_review_installation.lock().await;
        {
            let mut state = self.state.lock().await;
            if !state.session_configuration.tracks_completion_review() {
                return true;
            }
            state
                .completion_review
                .set_main_task(turn_id, &context.model_info.slug);
            state.completion_review.recovery_settings =
                Some(RegularTaskSettings::from_turn(context));
        }
        self.persist_completion_review_locked().await
    }

    pub(super) async fn update_pending_regular_settings(
        &self,
        updates: &SessionSettingsUpdate,
        model: Option<String>,
    ) {
        let _guard = self.completion_review_installation.lock().await;
        let mut state = self.state.lock().await;
        if !state.regular_continuation_directory_matches() {
            return;
        }
        let Some(checkpoint) = state
            .pending_continuation
            .as_mut()
            .filter(|checkpoint| checkpoint.target == PendingContinuationTarget::Regular)
        else {
            return;
        };
        if model.is_some() {
            checkpoint.model.clone_from(&model);
        }
        let review = &mut state.completion_review;
        if !review.replacement_answer_required
            && review.main_task_id.is_none()
            && review.pending_inputs.is_empty()
        {
            return;
        }
        if let Some(saved) = &mut review.recovery_settings {
            saved.apply_override(updates);
        }
        if model.is_some() {
            review.recovery_model.clone_from(&model);
        }
        if let Some(model) = model
            && let Some(handoff) = review.handoff.as_mut().filter(|handoff| {
                matches!(
                    handoff.disposition,
                    ReviewHandoffDisposition::Started { .. }
                )
            })
        {
            handoff.model = model;
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

    pub(super) async fn finish_ordinary_input(self: &Arc<Self>) -> bool {
        {
            let state = self.state.lock().await;
            let review = &state.completion_review;
            if !review.awaiting_compact_followup
                && review.unsampled_app_paths.is_empty()
                && review.unsampled_fresh_model.is_none()
            {
                return true;
            }
        }
        self.completion_review_transaction(|session| async move {
            let mut state = session.state.lock().await;
            state.completion_review.awaiting_compact_followup = false;
            state.completion_review.unsampled_fresh_model = None;
            state.completion_review.owed_settings = None;
            state.completion_review.unsampled_app_paths.clear();
            drop(state);
            session.persist_completion_review_locked().await
        })
        .await
        .unwrap_or(false)
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
            .ok()
            .map(|(_, ticket)| ticket)
    }

    pub(crate) async fn reserve_manual_completion_review(
        &self,
    ) -> Result<(CompletedTurnForReview, ReviewTicket), CompletionReviewReservationError> {
        let _lifecycle = self.task_lifecycle.lock().await;
        if self.active_turn.lock().await.is_some() {
            return Err(CompletionReviewReservationError::TaskRunning);
        }
        self.begin_completion_review_for_candidate(false).await
    }

    async fn begin_completion_review_for_candidate(
        &self,
        automatic: bool,
    ) -> Result<(CompletedTurnForReview, ReviewTicket), CompletionReviewReservationError> {
        let _guard = self.completion_review_installation.lock().await;
        let mut state = self.state.lock().await;
        if !state.session_configuration.tracks_completion_review() {
            return Err(CompletionReviewReservationError::NoCompletedTurn);
        }
        let review = &mut state.completion_review;
        let ticket = review.begin_review(automatic).ok_or_else(|| {
            if !review.request_frame.exact
                || review
                    .episode
                    .as_ref()
                    .is_some_and(|episode| !episode.request_frame.exact)
            {
                CompletionReviewReservationError::InexactRequest
            } else if review.input_revision != review.consumed_input_revision
                || !review.pending_inputs.is_empty()
            {
                CompletionReviewReservationError::PendingInput
            } else {
                CompletionReviewReservationError::NoCompletedTurn
            }
        })?;
        let completed_turn = review
            .eligible_episode()
            .ok_or(CompletionReviewReservationError::NoCompletedTurn)?
            .clone();
        drop(state);
        if !self.persist_completion_review_locked().await {
            return Err(CompletionReviewReservationError::Persistence);
        }
        Ok((completed_turn, ticket))
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
        review.invalidate_request_provenance();
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
        if !cwd_matches || !review.ticket_is_current(ticket) || review.handoff.is_some() {
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

    pub(super) async fn offer_completion_review_advice(
        &self,
    ) -> Result<Option<String>, CompletionReviewReservationError> {
        let _guard = self.completion_review_installation.lock().await;
        let mut state = self.state.lock().await;
        let review = &mut state.completion_review;
        let Some(handoff) = review.unstarted_handoff() else {
            return Ok(None);
        };
        let attempt_id = handoff.ticket.attempt_id.clone();
        if handoff.disposition == ReviewHandoffDisposition::Prepared {
            review.defer_handoff();
            drop(state);
            if !self.persist_completion_review_locked().await {
                return Err(CompletionReviewReservationError::Persistence);
            }
        }
        Ok(Some(attempt_id))
    }

    pub(crate) async fn claim_completion_review_handoff(
        &self,
        attempt_id: &str,
        turn_id: &str,
        confirmed: bool,
    ) -> Option<PendingContinuation> {
        let _guard = self.completion_review_installation.lock().await;
        let mut state = self.state.lock().await;
        if state.pending_continuation.is_some() {
            return None;
        }
        let cwd_matches = state
            .completion_review
            .eligible_episode()
            .is_some_and(|episode| episode.cwd == state.session_configuration.cwd);
        let handoff = state.completion_review.unstarted_handoff()?;
        let expected = if confirmed {
            ReviewHandoffDisposition::Deferred
        } else {
            ReviewHandoffDisposition::Prepared
        };
        if !cwd_matches
            || handoff.ticket.attempt_id != attempt_id
            || handoff.disposition != expected
        {
            return None;
        }
        let mode =
            state
                .mode_for_continuation()
                .with_updates(Some(handoff.model.clone()), None, None);
        let settings = RegularTaskSettings::for_review_handoff(&state.session_configuration, mode);
        let review = &mut state.completion_review;
        let handoff = review.handoff.as_mut()?;
        handoff.disposition = ReviewHandoffDisposition::Started {
            turn_id: turn_id.to_string(),
        };
        let checkpoint = PendingContinuation {
            source: TurnContinuationSource::PostTurnCompletionReview,
            continued_from_turn_id: Some(handoff.ticket.candidate_completion_id.clone()),
            model: Some(handoff.model.clone()),
            pause_reason: None,
            target: PendingContinuationTarget::Regular,
        };
        review.recovery_model = checkpoint.model.clone();
        review.recovery_output_schema = None;
        review.recovery_settings = Some(settings);
        review.replacement_answer_required = true;
        drop(state);
        self.persist_completion_review_locked()
            .await
            .then_some(checkpoint)
    }

    pub(crate) fn reserve_review_work(
        &self,
        operation: InternalReviewOperation,
    ) -> (InternalReviewOperation, crate::runtime_work::WorkGuard) {
        let (ticket, stage) = match &operation {
            InternalReviewOperation::Review(ticket) => (ticket, "queuedReview"),
            InternalReviewOperation::Continue(ticket) => (ticket, "queuedContinuation"),
        };
        let work = crate::runtime_work::WorkGuard::new(
            self.conversation_id,
            &ticket.candidate_completion_id,
            stage,
        );
        (operation, work)
    }

    pub(crate) async fn queue_completion_review_operation(
        &self,
        reservation: (InternalReviewOperation, crate::runtime_work::WorkGuard),
    ) {
        let op = match &reservation.0 {
            InternalReviewOperation::Review(_) => Op::ReviewCompletedTurn,
            InternalReviewOperation::Continue(_) => Op::Continue,
        };
        let id = format!("completion-review-{}", uuid::Uuid::new_v4());
        self.state
            .lock()
            .await
            .internal_review_operations
            .insert(id.clone(), (reservation.0, Some(reservation.1)));
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
                .remove(&id)
                .map(|(operation, _work)| operation);
            let message = match operation {
                Some(InternalReviewOperation::Review(ticket))
                    if self.completion_review_ticket_is_current(&ticket).await =>
                {
                    self.set_pending_continuation(Some(PendingContinuation {
                        source: TurnContinuationSource::Interrupted,
                        continued_from_turn_id: Some(ticket.candidate_completion_id),
                        model: None,
                        pause_reason: None,
                        target: PendingContinuationTarget::PostTurnCompletionReview,
                    }))
                    .await;
                    "Completion review could not be queued. Use /continue to retry the review."
                }
                Some(InternalReviewOperation::Continue(ticket)) => {
                    let _guard = self.completion_review_installation.lock().await;
                    let mut state = self.state.lock().await;
                    let review = &mut state.completion_review;
                    if !review.ticket_is_current(&ticket) || !review.defer_handoff() {
                        return;
                    }
                    drop(state);
                    if !self.persist_completion_review_locked().await {
                        return;
                    }
                    "Review advice could not be queued. Use /continue to apply the saved advice."
                }
                Some(InternalReviewOperation::Review(_)) | None => return,
            };
            self.send_event_raw(Event {
                id,
                msg: EventMsg::Warning(WarningEvent {
                    message: message.into(),
                }),
            })
            .await;
        }
    }

    pub(crate) async fn take_completion_review_operation(
        &self,
        id: &str,
    ) -> Option<(
        InternalReviewOperation,
        Option<crate::runtime_work::WorkGuard>,
    )> {
        self.state
            .lock()
            .await
            .internal_review_operations
            .remove(id)
    }

    pub(crate) async fn prepared_completion_review_handoff(
        &self,
    ) -> Option<InternalReviewOperation> {
        let state = self.state.lock().await;
        if state.pending_continuation.is_some() {
            return None;
        }
        state
            .completion_review
            .unstarted_handoff()
            .filter(|handoff| handoff.disposition == ReviewHandoffDisposition::Prepared)
            .map(|handoff| InternalReviewOperation::Continue(handoff.ticket.clone()))
    }

    pub(crate) async fn completion_review_main_instructions(
        &self,
        include_reference: bool,
    ) -> Result<Option<String>, CodexErr> {
        let state = self.state.lock().await;
        Self::render_review_instructions(&state, include_reference)
    }

    pub(crate) async fn compaction_review_frames(
        self: &Arc<Self>,
        context: &Arc<TurnContext>,
    ) -> CodexResult<ReviewPromptFrames> {
        let context = Arc::clone(context);
        self.completion_review_transaction(move |session| async move {
            session.compaction_review_frames_locked(&context).await
        })
        .await
        .map_err(|e| CodexErr::Fatal(e.to_string()))?
    }

    async fn compaction_review_frames_locked(
        &self,
        context: &TurnContext,
    ) -> CodexResult<ReviewPromptFrames> {
        let base = self.get_base_instructions_for_turn(context).await;
        let state = self.state.lock().await;
        let mut frames = state.completion_review.prompt_frames.clone();
        if state.completion_review.defer_new_prompt_frame {
            return Ok(frames);
        }
        let attempt = Self::active_review_attempt(&state)?;
        if state.completion_review.replacement_answer_required
            && state.completion_review.input_revision
                == state.completion_review.consumed_input_revision
            && frames.0.last().is_none_or(|f| f.attempt != attempt)
        {
            let items = state.history.raw_items();
            let marker = items
                .iter()
                .rposition(crate::compact::is_work_notes_request_item);
            let work = &items[..marker.unwrap_or(items.len())];
            // A job checkpoint (and a manual compact) can run before new advice is
            // admitted. Its candidate has no insertion point in already sampled work.
            if state
                .completion_review
                .handoff
                .as_ref()
                .is_some_and(|handoff| {
                    !handoff.advisory_recorded || work.last() != Some(&handoff.advisory_item())
                })
            {
                drop(state);
                self.record_frame_selection_locked(&frames, Some(context))
                    .await?;
                return Ok(frames);
            }
            let Some(text) = Self::render_review_instructions(&state, false)? else {
                return Ok(frames);
            };
            let work_tokens = match marker {
                Some(index) => state
                    .history
                    .work_notes_baseline_at(index, &context.model_info.slug)
                    .map(|baseline| baseline.work_tokens),
                None => state.helper_tokens_from_usage(&context.model_info.slug),
            }
            .unwrap_or_else(|| super::turn::estimate_work_notes_prompt_tokens(work, &base));
            let new = ReviewPromptFrame::new(text, ReviewPromptFrame::anchor(work), attempt);
            if work_tokens
                .saturating_add(frames.tokens())
                .saturating_add(new.tokens)
                > super::turn::unsampled_review_token_limit(context)
            {
                drop(state);
                self.record_frame_selection_locked(&frames, Some(context))
                    .await?;
                return Ok(frames);
            }
            frames.0.push(new);
            drop(state);
            self.record_frame_selection_locked(&frames, None).await?;
        }
        Ok(frames)
    }

    async fn record_frame_selection_locked(
        &self,
        frames: &ReviewPromptFrames,
        deferred_source: Option<&TurnContext>,
    ) -> CodexResult<()> {
        {
            let mut state = self.state.lock().await;
            state.completion_review.prompt_frames = frames.clone();
            state.completion_review.defer_new_prompt_frame = deferred_source.is_some();
            state.completion_review.deferred_capture_source = deferred_source.map(|ctx| {
                DeferredCaptureSource::Settings(Box::new(RegularTaskSettings::from_turn(ctx)))
            });
        }
        if !self.persist_completion_review_locked().await {
            return Err(CodexErr::Fatal(
                "Could not persist the selected review frame. No request was sent.".into(),
            ));
        }
        Ok(())
    }

    fn active_review_attempt(state: &SessionState) -> CodexResult<Option<String>> {
        let review = &state.completion_review;
        if !review.replacement_answer_required {
            return Ok(None);
        }
        let Some(handoff) = &review.handoff else {
            return Ok(None);
        };
        if !matches!(
            handoff.disposition,
            ReviewHandoffDisposition::Started { .. }
        ) || !review.ticket_is_current(&handoff.ticket)
        {
            return Err(CodexErr::Fatal(
                "Review follow-up snapshots are unavailable. Repair persistence and resume from the last durable rollout, or supply a new complete request. No incomplete follow-up request was sent.".into()
            ));
        }
        Ok(Some(handoff.ticket.attempt_id.clone()))
    }

    pub(super) async fn review_sampling(
        self: &Arc<Self>,
        context: &Arc<TurnContext>,
        capture_source: &Arc<TurnContext>,
        input: &[ResponseItem],
        allow_compact: bool,
    ) -> CodexResult<ReviewSampling> {
        let target = review_input_budget(context);
        {
            let state = self.state.lock().await;
            if !state.completion_review.replacement_answer_required {
                return Ok(ReviewSampling::Ready(ReviewPromptFrames::default()));
            }
            // Already-admitted tool rounds only read the projection. Avoid spawning a durable
            // installation transaction or scanning the history for a new anchor on the hot path.
            let frames = &state.completion_review.prompt_frames;
            if let Some(frame) = frames.0.last()
                && frame.attempt == Self::active_review_attempt(&state)?
                && state
                    .history_tokens_from_usage(&context.model_info.slug)
                    .is_some_and(|tokens| tokens.saturating_add(frames.tokens()) <= target)
            {
                return Ok(ReviewSampling::Ready(frames.clone()));
            }
        }
        let anchor = ReviewPromptFrame::anchor(input);
        let helper_limit = super::turn::unsampled_review_token_limit(capture_source);
        let context = Arc::clone(context);
        let capture_source = Arc::clone(capture_source);
        self.completion_review_transaction(move |session| async move {
            let state = session.state.lock().await;
            let attempt = Self::active_review_attempt(&state)?;
            if !state.completion_review.replacement_answer_required {
                return Ok(ReviewSampling::Ready(ReviewPromptFrames::default()));
            }
            let mut frames = state.completion_review.prompt_frames.clone();
            let history = review_work_tokens(&state, &context);
            let pinned = frames.0.last().is_some_and(|frame| frame.attempt == attempt);
            let render = |full| -> CodexResult<ReviewPromptFrame> {
                let text = Self::render_review_instructions(&state, full)?
                    .ok_or_else(|| CodexErr::Fatal("Missing active review frame.".into()))?;
                Ok(ReviewPromptFrame::new(text, anchor, attempt.clone()))
            };
            if !pinned {
                let full = render(true)?;
                let cost = history.saturating_add(frames.tokens()).saturating_add(full.tokens);
                frames.0.push(if cost <= target { full } else { render(false)? });
            }
            let prompt_tokens = history.saturating_add(frames.tokens());
            if prompt_tokens > target {
                // Even if omitting the optional reference would now fit, changing a frame that
                // has governed work would rewrite the hot input. Compact that work unchanged.
                let mandatory = render(false)?.tokens;
                if mandatory <= target && allow_compact {
                    tracing::info!(prompt_tokens, target, pinned, "compacting history to fit review follow-up");
                    if !pinned && helper_work_tokens(&state, &capture_source)
                        .saturating_add(frames.tokens()) > helper_limit {
                        // The started handoff owns the new scope durably. It has governed no
                        // ordinary work yet. Install it after compacting the outgoing prefix.
                        let outgoing = state.completion_review.prompt_frames.clone();
                        drop(state);
                        session.record_frame_selection_locked(&outgoing, Some(&capture_source)).await?;
                        return Ok(ReviewSampling::Compact);
                    }
                } else {
                    let recovery = if mandatory > target {
                        "The mandatory review frame alone does not fit. Select a larger context window, then /continue."
                    } else {
                        "Compaction did not free enough space. Run /compact or select a larger context window, then /continue."
                    };
                    return Err(CodexErr::Fatal(format!(
                        "Review follow-up context exceeds the main model's payload budget ({prompt_tokens} estimated tokens, {target} available). {recovery} No truncated follow-up request was sent."
                    )));
                }
            }
            if !pinned {
                drop(state);
                session.record_frame_selection_locked(&frames, None).await?;
            }
            Ok(if prompt_tokens > target { ReviewSampling::Compact } else { ReviewSampling::Ready(frames) })
        }).await.map_err(|e| CodexErr::Fatal(e.to_string()))?
    }

    fn render_review_instructions(
        state: &SessionState,
        include_reference: bool,
    ) -> CodexResult<Option<String>> {
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
        Self::active_review_attempt(state)?;
        let episode = review
            .eligible_episode()
            .ok_or_else(|| CodexErr::Fatal("Review follow-up candidate is unavailable.".into()))?;
        let advisory = handoff.advisory_item();
        let instructions = if state.history.raw_items().contains(&advisory) {
            crate::tasks::post_turn_completion_review::REPLACEMENT_ANSWER_CONTRACT
        } else {
            &handoff.advisory
        };
        let precedence = if review.prompt_frames.0.first().is_some_and(|frame| {
            frame.attempt.as_deref() != Some(handoff.ticket.attempt_id.as_str())
        }) {
            "\nEarlier review frames describe earlier work. Use the current scope and candidate below; earlier verdicts are not additional user requirements."
        } else {
            ""
        };
        Ok(Some(format!(
            "{instructions}{precedence}\n{}",
            crate::tasks::post_turn_completion_review::render_completed_turn_context(
                episode,
                include_reference
            )
        )))
    }

    /// False means compact the outgoing work before writing this advisory to history.
    pub(super) async fn admit_completion_review_advisory(
        self: &Arc<Self>,
        context: &Arc<TurnContext>,
        capture_source: &Arc<TurnContext>,
        allow_compact: bool,
    ) -> CodexResult<bool> {
        // Enter the installation transaction before reading state; pause joins this owner.
        let context = Arc::clone(context);
        let capture_source = Arc::clone(capture_source);
        self.completion_review_transaction(move |session| async move {
            session
                .admit_completion_review_advisory_locked(&context, &capture_source, allow_compact)
                .await
        })
        .await
        .map_err(|e| CodexErr::Fatal(e.to_string()))?
    }

    async fn admit_completion_review_advisory_locked(
        &self,
        context: &TurnContext,
        capture_source: &TurnContext,
        allow_compact: bool,
    ) -> CodexResult<bool> {
        let state = self.state.lock().await;
        let handoff = state.completion_review.handoff.as_ref();
        let Some(handoff) = handoff.filter(|handoff| {
            !handoff.advisory_recorded
                && matches!(
                    handoff.disposition,
                    ReviewHandoffDisposition::Started { .. }
                )
        }) else {
            return Ok(true);
        };
        // A crash can leave this handoff's advisory at the tail without its state flag.
        // The same advice *before* intervening work belongs to an earlier review.
        let item = handoff.advisory_item();
        let exists = state.history.raw_items().last() == Some(&item);
        if !exists {
            let work = review_work_tokens(&state, context);
            let outgoing = &state.completion_review.prompt_frames;
            let advice_tokens = crate::context_manager::estimate_item_token_count(&item);
            let prompt_tokens = work
                .saturating_add(outgoing.tokens())
                .saturating_add(advice_tokens);
            let target = review_input_budget(context);
            if advice_tokens > target || (!allow_compact && prompt_tokens > target) {
                return Err(CodexErr::Fatal(format!(
                    "Review advice exceeds the main model's payload budget ({prompt_tokens} estimated tokens, {target} available). Select a larger context window, then /continue. No truncated follow-up request was sent."
                )));
            }
            if allow_compact
                && helper_work_tokens(&state, capture_source)
                    .saturating_add(outgoing.tokens())
                    .saturating_add(advice_tokens)
                    > super::turn::unsampled_review_token_limit(capture_source)
            {
                let outgoing = outgoing.clone();
                drop(state);
                self.record_frame_selection_locked(&outgoing, Some(capture_source))
                    .await?;
                return Ok(false);
            }
        }
        drop(state);
        if !exists {
            self.record_conversation_items(context, &[item]).await;
        }
        if let Some(handoff) = self.state.lock().await.completion_review.handoff.as_mut() {
            handoff.advisory_recorded = true;
        }
        if !self.persist_completion_review_locked().await {
            return Err(CodexErr::Fatal(
                "Could not persist review advisory admission.".into(),
            ));
        }
        Ok(true)
    }

    pub(crate) async fn stop_completion_review_operations(&self) {
        let _guard = self.completion_review_installation.lock().await;
        let mut state = self.state.lock().await;
        // Release queued ownership now, but preserve the private submission IDs
        // so draining the channel cannot reinterpret them as manual commands.
        for (_, guard) in state.internal_review_operations.values_mut() {
            *guard = None;
        }
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
        // Stop scheduling without discarding a completed review's unapplied advice.
        if !started_main && !state.completion_review.defer_handoff() {
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
        if let Some(operation) = self.prepared_completion_review_handoff().await {
            self.queue_completion_review_operation(self.reserve_review_work(operation))
                .await;
        }
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
