use super::*;
use crate::completion_review::AcceptedReviewInput;
use crate::completion_review::AutoCompactionDecision;
use crate::completion_review::CaptureJournal;
use crate::completion_review::CompletionReviewReservationError;
use crate::completion_review::CompletionReviewState;
use crate::completion_review::DeferredCaptureSource;
use crate::completion_review::InputContextProjection;
use crate::completion_review::InternalReviewOperation;
use crate::completion_review::PendingReviewAdvice;
use crate::completion_review::ReviewAdvice;
use crate::completion_review::ReviewHandoff;
use crate::completion_review::ReviewHandoffDisposition;
use crate::completion_review::ReviewInputInstallation;
use crate::completion_review::ReviewPromptFrame;
use crate::completion_review::ReviewPromptFrames;
use crate::completion_review::ReviewSampling;
use crate::completion_review::ReviewSamplingInput;
use crate::completion_review::ReviewTicket;
use crate::completion_review::TaggedCaptureCall;
use crate::completion_review::WorkNotesCheckpoint;

/// A new ordinary handoff, not yet part of the outgoing work. The authenticated
/// send publishes it before any possible transmission, including the first retry.
#[derive(Debug)]
pub(crate) struct ReviewFrameProposal {
    session: std::sync::Weak<Session>,
    context: Arc<TurnContext>,
    frames: ReviewPromptFrames,
    mandatory: Option<(usize, Box<ReviewPromptFrame>)>,
    advice: Option<ResponseItem>,
    committed: tokio::sync::OnceCell<FrameForm>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FrameForm {
    Preferred,
    Mandatory,
}

impl ReviewFrameProposal {
    pub(super) fn new(
        session: &Arc<Session>,
        context: &Arc<TurnContext>,
        frames: ReviewPromptFrames,
        advice: Option<ResponseItem>,
        mandatory: Option<(usize, Box<ReviewPromptFrame>)>,
    ) -> Arc<Self> {
        Arc::new(Self {
            session: Arc::downgrade(session),
            context: Arc::clone(context),
            frames,
            mandatory,
            advice,
            committed: Default::default(),
        })
    }

    pub(super) fn unrecorded_advice(&self) -> Option<&ResponseItem> {
        self.advice
            .as_ref()
            .filter(|_| self.committed.get().is_none())
    }

    fn owns_outgoing(&self, state: &SessionState) -> CodexResult<bool> {
        let Some((last, outgoing)) = self.frames.0.split_last() else {
            return Ok(false);
        };
        Ok(state.completion_review.replacement_answer_required
            && Session::active_review_attempt(state)? == last.attempt
            && state.completion_review.prompt_frames.0 == outgoing)
    }

    pub(super) async fn defer_refused(
        self: &Arc<Self>,
        source: &Arc<TurnContext>,
        endpoint: Option<String>,
        cancel: &CancellationToken,
    ) -> CodexResult<()> {
        let session = self.session.upgrade().ok_or(CodexErr::TurnAborted)?;
        if self.committed.get().is_some() {
            // An earlier attempt crossed the dispatch boundary, even if its stream failed.
            return session
                .check_review_handoff_floor(&self.context, source, cancel)
                .await;
        }
        let proposal = Arc::clone(self);
        let source = Arc::clone(source);
        let cancel = cancel.clone();
        session
            .completion_review_transaction(move |session| async move {
                let state = session.state.lock().await;
                if !proposal.owns_outgoing(&state)? {
                    return Err(CodexErr::ReviewSuperseded);
                }
                if state.completion_review.defer_new_prompt_frame {
                    // Revalidate an already-selected handoff without replacing its saved source.
                    drop(state);
                    return session
                        .check_review_handoff_floor(&proposal.context, &source, &cancel)
                        .await;
                }
                let mut outgoing = state.completion_review.prompt_frames.clone();
                // A completion without usage still owns the outgoing prefix. Its size
                // receipt, if any, is selected independently by helper_work_tokens.
                let same_source = source.model_provider_id == proposal.context.model_provider_id
                    && state
                        .completion_review
                        .last_work_settings
                        .as_ref()
                        .filter(|work| work.mode.model() == source.model_info.slug)
                        .and_then(|work| work.sampled_controls.as_ref())
                        .is_some_and(|sample| {
                            sample.route.provider == source.model_provider_id
                                && sample.owns_endpoint(endpoint.as_deref())
                        });
                drop(state);
                let addition = if same_source {
                    session
                        .checkpoint_addition(
                            &source,
                            &proposal.context,
                            &outgoing,
                            proposal.unrecorded_advice(),
                            &cancel,
                        )
                        .await?
                        .unwrap_or_default()
                } else {
                    CheckpointAddition::default()
                };
                session
                    .check_review_handoff_floor_with_advice(
                        &proposal.context,
                        &source,
                        addition.advice.is_some(),
                        &cancel,
                    )
                    .await?;
                if let Some(advice) = addition.advice {
                    session
                        .record_review_advice_locked(&proposal.context, advice)
                        .await;
                }
                outgoing.0.extend(addition.frame);
                session
                    .record_frame_selection_locked(&outgoing, Some(&source))
                    .await
            })
            .await?
    }

    fn cost(&self, original: i64, form: FrameForm) -> i64 {
        match (&self.mandatory, self.frames.0.last(), form) {
            (Some((_, mandatory)), Some(preferred), FrameForm::Mandatory) => {
                original.saturating_sub(preferred.tokens - mandatory.tokens)
            }
            _ => original,
        }
    }

    pub(super) fn dispatched_tokens(&self, original: i64) -> i64 {
        self.cost(
            original,
            self.committed
                .get()
                .copied()
                .unwrap_or(FrameForm::Preferred),
        )
    }

    pub(crate) async fn admit(
        self: &Arc<Self>,
        estimated: i64,
        target: i64,
        items: &mut [ResponseItem],
        endpoint: Option<&str>,
    ) -> CodexResult<()> {
        self.commit(estimated, target, endpoint).await?;
        if let Some((index, mandatory)) = &self.mandatory
            && self.committed.get() == Some(&FrameForm::Mandatory)
        {
            items[*index] = mandatory.item.clone();
        }
        Ok(())
    }

    pub(super) async fn commit(
        self: &Arc<Self>,
        estimated: i64,
        target: i64,
        endpoint: Option<&str>,
    ) -> CodexResult<()> {
        let chosen = self
            .committed
            .get_or_try_init(|| async {
                let session = self.session.upgrade().ok_or(CodexErr::TurnAborted)?;
                let proposal = Arc::clone(self);
                let endpoint = endpoint.map(str::to_owned);
                session
                    .completion_review_transaction(move |session| async move {
                        let state = session.state.lock().await;
                        if !proposal.owns_outgoing(&state)? {
                            return Err(CodexErr::ReviewSuperseded);
                        }
                        let form = if estimated > target && proposal.mandatory.is_some() {
                            FrameForm::Mandatory
                        } else {
                            FrameForm::Preferred
                        };
                        crate::client_common::ReviewAdmission::check_size(
                            proposal.cost(estimated, form),
                            target,
                            endpoint.as_deref(),
                        )?;
                        drop(state);
                        if let Some(advice) = &proposal.advice {
                            session
                                .record_review_advice_locked(&proposal.context, advice)
                                .await;
                        }
                        let mut frames = std::borrow::Cow::Borrowed(&proposal.frames);
                        if let Some((_, mandatory)) = &proposal.mandatory
                            && form == FrameForm::Mandatory
                            && let Some(current) = frames.to_mut().0.last_mut()
                        {
                            *current = mandatory.as_ref().clone();
                        }
                        session.record_frame_selection_locked(&frames, None).await?;
                        Ok(form)
                    })
                    .await?
            })
            .await?;
        crate::client_common::ReviewAdmission::check_size(
            self.cost(estimated, *chosen),
            target,
            endpoint,
        )
    }
}

fn review_input_budget(context: &TurnContext) -> i64 {
    context
        .model_context_window()
        .unwrap_or_default()
        .saturating_sub(8_192)
}

impl AutoCompactionDecision {
    pub(super) fn fit_for_source(&self, source: &TurnContext) -> compact::WorkFit {
        if self.source.mode.model() == source.model_info.slug {
            // Each helper checks its provider and its actual authenticated endpoint.
            self.fit.clone()
        } else {
            compact::WorkFit::local()
        }
    }
}

struct ReviewWorkEstimate {
    transcript: i64,
    routing_input: Vec<ResponseItem>,
}

impl ReviewWorkEstimate {
    fn new(state: &SessionState, end: usize, transcript: i64) -> Self {
        // Only user messages and the separately journaled app selections choose connector tools.
        let routing_input = state.history.raw_items()[..end]
            .iter()
            .filter(|item| matches!(item, ResponseItem::Message { role, .. } if role == "user"))
            .cloned()
            .collect();
        Self {
            transcript,
            routing_input,
        }
    }

    async fn resolve(
        self,
        session: &Session,
        context: &TurnContext,
        active: &TurnContext,
        cancel: &CancellationToken,
    ) -> CodexResult<i64> {
        let tools =
            super::turn::compaction_tool_specs(session, context, &self.routing_input, cancel).await;
        if !std::ptr::eq(active, context) {
            session.activate_mcp_context(active).await;
        }
        Ok(self.transcript.saturating_add(tools?.tokens()).max(0))
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ReviewFrameView {
    Work,
    AdmittingAdvice,
    AfterCheckpoint { separate_advice: bool },
}

#[derive(Default)]
struct CheckpointAddition<'a> {
    advice: Option<&'a ResponseItem>,
    frame: Option<ReviewPromptFrame>,
}

fn helper_work_tokens(
    state: &SessionState,
    source: &TurnContext,
    marker: Option<usize>,
) -> ReviewWorkEstimate {
    let base = state.base_instructions_for_turn(source);
    let end = marker.unwrap_or(state.history.raw_items().len());
    let local = state
        .history
        .projected_prefix_tokens(end, source.helper_projection())
        .saturating_add(crate::truncate::approx_token_count(&base.text) as i64);
    let chat = source
        .provider
        .configured_endpoint(Some(crate::auth::AuthMode::Chatgpt));
    let api = source
        .provider
        .configured_endpoint(Some(crate::auth::AuthMode::ApiKey));
    let measured = state
        .history
        .work_receipt_source(marker)
        .filter(|r| {
            r.model == source.model_info.slug
                && r.provider == source.model_provider_id
                && (r.endpoint == chat.as_deref() || r.endpoint == api.as_deref())
        })
        .and_then(|r| super::turn::measured_helper_work(state, source, &base, marker, r));
    // Only unsampled additions use this bound. Both helpers keep the chosen suffix,
    // but a login switch can make either one price the work locally at dispatch.
    let transcript = measured.map_or(
        local,
        |(work, _)| if chat == api { work } else { work.max(local) },
    );
    ReviewWorkEstimate::new(state, end, transcript)
}

impl Session {
    pub(super) async fn ordinary_review_admission(
        &self,
        context: &TurnContext,
        prompt: &Prompt,
        request_only_tokens: i64,
        proposal: Option<Arc<ReviewFrameProposal>>,
    ) -> crate::client_common::ReviewAdmission {
        let state = self.state.lock().await;
        let endpoint = state
            .history
            .work_receipt_source(None)
            .and_then(|s| s.endpoint);
        let measured = state
            .history
            .review_transcript_tokens(
                context.usage_source(endpoint),
                &prompt.base_instructions,
                state.server_reasoning_included(),
                crate::context_manager::HelperProjection {
                    structured_outputs: prompt.tools.structured_outputs,
                    ..context.helper_projection()
                },
            )
            .map(|tokens| {
                tokens
                    .saturating_add(prompt.tools.tokens())
                    .max(0)
                    .saturating_add(request_only_tokens)
                    .saturating_add(
                        proposal
                            .as_ref()
                            .and_then(|p| p.unrecorded_advice())
                            .map_or(0, crate::context_manager::estimate_item_token_count),
                    )
            });
        crate::client_common::ReviewAdmission {
            target: review_input_budget(context),
            measured: endpoint
                .zip(measured)
                .map(|(owner, tokens)| (owner.to_owned(), tokens)),
            proposal,
        }
    }

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
        source_fit: &crate::compact::WorkFit,
        legacy_attempts: u8,
        allow_new_capture: bool,
    ) -> CodexResult<AutoCompactionDecision> {
        let _guard = self.completion_review_installation.lock().await;
        let (decision, previous) = {
            let mut state = self.state.lock().await;
            let slot = &state.completion_review.pending_compaction;
            let complete = |d: &AutoCompactionDecision| match d.fit.source_fit {
                Some(crate::compact::SourceFit::Local) => true,
                Some(crate::compact::SourceFit::Measured { .. }) => d.fit.fit_route.is_some(),
                None => false,
            };
            if let Some(saved) = slot
                && saved.capture.notes_remaining.is_some()
                && complete(saved)
            {
                return Ok(saved.clone());
            }
            let mut decision = slot.clone().unwrap_or_else(|| AutoCompactionDecision {
                source: state.checkpoint_source_settings(source),
                placement,
                fit: Default::default(),
                capture: CaptureJournal::default(),
            });
            // Older offsets included an unidentified tool surface. Upgrade from attributable
            // pre-capture work if available, otherwise use the current local projection.
            if !complete(&decision) {
                decision.fit = if decision.fit.source_fit.is_some() {
                    // A legacy measured correction without its owner cannot be attributed.
                    crate::compact::WorkFit::local()
                } else {
                    source_fit.clone()
                };
            }
            decision.capture.notes_remaining.get_or_insert_with(|| {
                if decision.source.captures_notes() && (slot.is_some() || allow_new_capture) {
                    crate::compact::WORK_NOTES_CAPTURE_MAX_ATTEMPTS.saturating_sub(legacy_attempts)
                } else {
                    0
                }
            });
            let previous = state
                .completion_review
                .pending_compaction
                .replace(decision.clone());
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

    pub(super) async fn claim_tagged_capture_call(&self, item: &ResponseItem) -> CodexResult<()> {
        let _guard = self.completion_review_installation.lock().await;
        {
            let mut state = self.state.lock().await;
            let offset = crate::compact::next_capture_offset(state.history.raw_items())
                .ok_or_else(|| {
                    CodexErr::Fatal("Missing work-notes request for a capture call".into())
                })?;
            let decision = state
                .completion_review
                .pending_compaction
                .as_mut()
                .ok_or_else(|| {
                    CodexErr::Fatal("Missing work-notes decision for a capture call".into())
                })?;
            decision.capture.tagged_calls.push(
                TaggedCaptureCall::from_item(offset, item).map_err(|err| {
                    CodexErr::Fatal(format!("Could not identify the tagged capture call: {err}"))
                })?,
            );
        }
        if !self.persist_completion_review_locked().await {
            if let Some(decision) =
                &mut self.state.lock().await.completion_review.pending_compaction
            {
                decision.capture.tagged_calls.pop();
            }
            return Err(CodexErr::Fatal(
                "Could not persist ownership of a tagged capture call".into(),
            ));
        }
        Ok(())
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
            let Some(remaining) = decision.capture.notes_remaining.filter(|n| *n > 0) else {
                return Ok(());
            };
            decision.capture.notes_remaining = Some(match checkpoint {
                WorkNotesCheckpoint::CompletedResponse => remaining - 1,
                WorkNotesCheckpoint::BeforeCompact => 0,
            });
            remaining
        };
        if !self.persist_completion_review_locked().await {
            if let Some(decision) =
                &mut self.state.lock().await.completion_review.pending_compaction
            {
                decision.capture.notes_remaining = Some(previous);
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

    // Called under the review installation guard. Flush new bytes before publishing their ID.
    async fn archive_sampled_base(&self, text: String) -> Option<ThreadId> {
        let tracked = {
            let state = self.state.lock().await;
            if let Some(id) = state.recorded_base_id(&text) {
                return Some(id);
            }
            state.session_configuration.tracks_completion_review()
        };
        let id = ThreadId::new();
        if tracked {
            let recorder = self.services.rollout.lock().await.clone();
            if let Some(recorder) = recorder {
                let result = async {
                    recorder
                        .record_items(&[RolloutItem::SampledInstructions {
                            id,
                            text: text.clone(),
                        }])
                        .await?;
                    recorder.flush().await
                }
                .await;
                if !self.check_completion_review_persistence(result).await {
                    return None;
                }
            }
        }
        self.state.lock().await.recorded_bases.push((id, text));
        Some(id)
    }

    pub(super) async fn finish_ordinary_input(
        self: &Arc<Self>,
        context: &TurnContext,
        prompt: &Prompt,
        endpoint: Option<&str>,
    ) -> bool {
        let base = &prompt.base_instructions;
        let settings = {
            let state = self.state.lock().await;
            let mut settings = state.task_settings(context, base);
            settings.sampled_controls =
                context.sampled_controls_at(endpoint, prompt.parallel_tool_calls_at(endpoint));
            let review = &state.completion_review;
            if settings.recorded_base.is_some()
                && !review.awaiting_compact_followup
                && review.owed_settings.is_none()
                && review.unsampled_app_paths.is_empty()
                && review.unsampled_fresh_model.is_none()
                && review.last_work_settings.as_ref() == Some(&settings)
            {
                return true;
            }
            settings
        };
        let unarchived = settings.recorded_base.is_none().then(|| base.text.clone());
        self.completion_review_transaction(move |session| async move {
            let mut settings = settings;
            if let Some(text) = unarchived {
                let Some(id) = session.archive_sampled_base(text).await else {
                    return false;
                };
                settings.recorded_base = Some(id);
            }
            let mut state = session.state.lock().await;
            state
                .completion_review
                .release_owed_response(Some(&settings));
            state.completion_review.last_work_settings = Some(settings);
            state.completion_review.awaiting_compact_followup = false;
            state.completion_review.unsampled_fresh_model = None;
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
        cancel: &CancellationToken,
    ) -> CodexResult<ReviewPromptFrames> {
        let context = Arc::clone(context);
        let cancel = cancel.clone();
        self.completion_review_transaction(move |session| async move {
            let mut frames = session
                .state
                .lock()
                .await
                .completion_review
                .prompt_frames
                .clone();
            if let Some(addition) = session
                .checkpoint_addition(&context, &context, &frames, None, &cancel)
                .await?
            {
                let deferred = addition.frame.is_none();
                frames.0.extend(addition.frame);
                session
                    .record_frame_selection_locked(&frames, deferred.then_some(context.as_ref()))
                    .await?;
            }
            Ok(frames)
        })
        .await
        .map_err(|e| CodexErr::Fatal(e.to_string()))?
    }

    /// Preview under the installation guard. An empty addition defers a new candidate;
    /// `None` leaves an existing selection (including its recovery source) alone.
    async fn checkpoint_addition<'a>(
        &self,
        context: &TurnContext,
        active: &TurnContext,
        frames: &ReviewPromptFrames,
        advice: Option<&'a ResponseItem>,
        cancel: &CancellationToken,
    ) -> CodexResult<Option<CheckpointAddition<'a>>> {
        let state = self.state.lock().await;
        if state.completion_review.defer_new_prompt_frame {
            return Ok(None);
        }
        let attempt = Self::active_review_attempt(&state)?;
        if !state.completion_review.replacement_answer_required
            || state.completion_review.input_revision
                != state.completion_review.consumed_input_revision
            || frames.0.last().is_some_and(|f| f.attempt == attempt)
        {
            return Ok(None);
        }
        let items = state.history.raw_items();
        let marker = items.iter().rposition(compact::is_work_notes_request_item);
        let work = &items[..marker.unwrap_or(items.len())];
        // Even a small candidate must not be inserted into already sampled work.
        if advice.is_none()
            && state
                .completion_review
                .handoff
                .as_ref()
                .is_some_and(|handoff| {
                    !handoff.advisory_recorded || work.last() != Some(&handoff.advisory_item())
                })
        {
            return Ok(Some(CheckpointAddition::default()));
        }
        let view = if advice.is_some() {
            ReviewFrameView::AdmittingAdvice
        } else {
            ReviewFrameView::Work
        };
        let text = Self::render_review_frame(&state, false, view)?
            .ok_or_else(|| CodexErr::Fatal("Missing active review frame.".into()))?;
        let work_tokens = helper_work_tokens(&state, context, marker);
        let new = ReviewPromptFrame::new(
            text,
            ReviewPromptFrame::anchor(work) + usize::from(advice.is_some()),
            attempt,
        );
        drop(state);
        let work = work_tokens.resolve(self, context, active, cancel).await?;
        let remaining = super::turn::unsampled_review_token_limit(context)
            .saturating_sub(work)
            .saturating_sub(frames.tokens());
        let advice_cost = advice.map_or(0, crate::context_manager::estimate_item_token_count);
        Ok(Some(CheckpointAddition {
            advice: advice.filter(|_| advice_cost <= remaining),
            frame: (advice_cost.saturating_add(new.tokens) <= remaining).then_some(new),
        }))
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
                DeferredCaptureSource::Settings(Box::new(state.checkpoint_source_settings(ctx)))
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
        &self,
        input: ReviewSamplingInput<'_>,
    ) -> CodexResult<ReviewSampling> {
        let state = self.state.lock().await;
        let review = &state.completion_review;
        let attempt = Self::active_review_attempt(&state)?;
        if input
            .advice
            .is_some_and(|a| Some(&a.attempt) != attempt.as_ref())
        {
            return Err(CodexErr::ReviewSuperseded);
        }
        if !review.replacement_answer_required {
            return Ok(ReviewSampling::Ready(ReviewPromptFrames::default()));
        }
        let mut frames = review.prompt_frames.clone();
        if frames
            .0
            .last()
            .is_some_and(|frame| frame.attempt == attempt)
        {
            return if input.advice.is_some() {
                Err(CodexErr::ReviewSuperseded)
            } else {
                Ok(ReviewSampling::Ready(frames))
            };
        }
        if review.awaiting_compact_followup
            && review.handoff.is_none()
            && review
                .pending_inputs
                .iter()
                .any(AcceptedReviewInput::is_uninstalled_steering)
        {
            return Ok(ReviewSampling::Ready(frames));
        }
        let anchor = ReviewPromptFrame::anchor(input.items);
        let view = if input.advice.is_some() {
            ReviewFrameView::AdmittingAdvice
        } else {
            ReviewFrameView::Work
        };
        let render = |full| -> CodexResult<ReviewPromptFrame> {
            let text = Self::render_review_frame(&state, full, view)?
                .ok_or_else(|| CodexErr::Fatal("Missing active review frame.".into()))?;
            Ok(ReviewPromptFrame::new(text, anchor, attempt.clone()))
        };
        let full = render(true)?;
        let mandatory = render(false)?;
        let mandatory_alternative = (mandatory.tokens < full.tokens).then(|| Box::new(mandatory));
        frames.0.push(full);
        Ok(ReviewSampling::FirstSend {
            frames,
            mandatory_alternative,
        })
    }

    fn render_review_instructions(
        state: &SessionState,
        include_reference: bool,
    ) -> CodexResult<Option<String>> {
        Self::render_review_frame(state, include_reference, ReviewFrameView::Work)
    }

    fn render_review_frame(
        state: &SessionState,
        include_reference: bool,
        view: ReviewFrameView,
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
        let separate_advice = match view {
            ReviewFrameView::Work => state.history.raw_items().contains(&handoff.advisory_item()),
            ReviewFrameView::AdmittingAdvice => true,
            // An unrecorded handoff will append its advice after installation. An admitted
            // advisory can disappear in the summary, so the frame itself must then carry it.
            ReviewFrameView::AfterCheckpoint { separate_advice } => separate_advice,
        };
        let instructions = if separate_advice {
            crate::tasks::post_turn_completion_review::REPLACEMENT_ANSWER_CONTRACT
        } else {
            &handoff.advisory
        };
        let precedence = if !matches!(view, ReviewFrameView::AfterCheckpoint { .. })
            && review.prompt_frames.0.first().is_some_and(|frame| {
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

    pub(super) async fn admit_completion_review_advisory(
        self: &Arc<Self>,
        context: &Arc<TurnContext>,
    ) -> CodexResult<ReviewAdvice> {
        // Enter the installation transaction before reading state; pause joins this owner.
        let context = Arc::clone(context);
        self.completion_review_transaction(move |session| async move {
            session
                .admit_completion_review_advisory_locked(&context)
                .await
        })
        .await
        .map_err(|e| CodexErr::Fatal(e.to_string()))?
    }

    fn mandatory_frame_tokens(state: &SessionState, view: ReviewFrameView) -> CodexResult<i64> {
        let text = Self::render_review_frame(state, false, view)?
            .ok_or_else(|| CodexErr::Fatal("Missing active review frame.".into()))?;
        Ok(crate::context_manager::estimate_item_token_count(
            &ResponseItem::from(DeveloperInstructions::new(text)),
        ))
    }

    fn checkpoint_review_cost(
        state: &SessionState,
        admitting_advice: bool,
    ) -> CodexResult<(i64, i64)> {
        let separate_advice = state
            .completion_review
            .handoff
            .as_ref()
            .filter(|h| !h.advisory_recorded && !admitting_advice);
        let frame = Self::mandatory_frame_tokens(
            state,
            ReviewFrameView::AfterCheckpoint {
                separate_advice: separate_advice.is_some(),
            },
        )?;
        Ok((
            frame,
            separate_advice.map_or(0, |h| {
                crate::context_manager::estimate_item_token_count(&h.advisory_item())
            }),
        ))
    }

    /// Space for history, instructions and tools on the first ordinary request.
    /// A newly accepted revision must not reserve a retired handoff's frame.
    pub(crate) async fn replacement_review_limit(
        &self,
        context: &TurnContext,
    ) -> CodexResult<Option<usize>> {
        let state = self.state.lock().await;
        let review = &state.completion_review;
        if !review.replacement_answer_required
            || !review.pending_inputs.is_empty()
            || review
                .handoff
                .as_ref()
                .is_some_and(|h| !review.ticket_is_current(&h.ticket))
        {
            return Ok(None);
        }
        let (frame, advice) = Self::checkpoint_review_cost(&state, false)?;
        Ok(Some(
            usize::try_from(
                review_input_budget(context)
                    .saturating_sub(frame)
                    .saturating_sub(advice),
            )
            .unwrap_or(0),
        ))
    }

    pub(super) async fn check_review_handoff_floor(
        &self,
        context: &TurnContext,
        source: &TurnContext,
        cancel: &CancellationToken,
    ) -> CodexResult<()> {
        self.check_review_handoff_floor_with_advice(context, source, false, cancel)
            .await
    }

    async fn check_review_handoff_floor_with_advice(
        &self,
        context: &TurnContext,
        source: &TurnContext,
        admitting_advice: bool,
        cancel: &CancellationToken,
    ) -> CodexResult<()> {
        let (frame, advice, notes) = {
            let state = self.state.lock().await;
            if !state.completion_review.replacement_answer_required
                || !state.completion_review.pending_inputs.is_empty()
            {
                return Ok(());
            }
            let notes = if source.final_output_json_schema.is_some() {
                // A structured source will not capture fresh notes. Its existing
                // wrapper, when present, is fixed rather than an unknown future output.
                compact::extract_existing_preserved_work_notes(state.history.raw_items())
                    .map(|text| compact::preserved_work_notes_message(&text))
            } else {
                None
            };
            let (frame, advice) = Self::checkpoint_review_cost(&state, admitting_advice)?;
            (frame, advice, notes)
        };
        let target = review_input_budget(context);
        if frame > target {
            return Err(CodexErr::Fatal(format!(
                "Review follow-up context exceeds the main model's payload budget. The mandatory review frame alone does not fit ({frame} estimated tokens, {target} available). Select a larger context window, then /continue. No truncated follow-up request was sent."
            )));
        }
        let initial_context = self.build_initial_context(context).await;
        let tools =
            super::turn::compaction_tool_specs(self, context, &initial_context, cancel).await?;
        let base = self.get_base_instructions_for_turn(context).await;
        let fixed = crate::compact_remote_v2::fixed_context_tokens(&initial_context, &base.text)
            .saturating_add(tools.tokens() as usize)
            .saturating_add(notes.as_ref().map_or(0, |item| {
                crate::context_manager::estimate_item_token_count(item) as usize
            }));
        let floor = (fixed as i64).saturating_add(frame).saturating_add(advice);
        if floor > target {
            return Err(CodexErr::Fatal(format!(
                "Review follow-up context exceeds the main model's payload budget even without history ({floor} estimated tokens, {target} available). Select a larger context window, then /continue. No truncated follow-up request was sent."
            )));
        }
        if compaction_implementation(source) == CompactionImplementation::RemoteV2
            && let Some(target) = crate::compact_remote_v2::installation_target(
                context.model_context_window(),
                context.model_info.auto_compact_token_limit(),
            )
            && fixed > target
        {
            return Err(CodexErr::Fatal(format!(
                "Review checkpoint needs {fixed} fixed tokens even before a summary, exceeding the {target}-token context target. Increase the limiting context window or automatic compaction threshold, then /continue. No helper request was sent."
            )));
        }
        Ok(())
    }

    async fn admit_completion_review_advisory_locked(
        &self,
        context: &TurnContext,
    ) -> CodexResult<ReviewAdvice> {
        let state = self.state.lock().await;
        let handoff = state.completion_review.handoff.as_ref();
        let Some(handoff) = handoff.filter(|handoff| {
            !handoff.advisory_recorded
                && matches!(
                    handoff.disposition,
                    ReviewHandoffDisposition::Started { .. }
                )
        }) else {
            return Ok(ReviewAdvice::Recorded);
        };
        let item = handoff.advisory_item();
        let attempt = handoff.ticket.attempt_id.clone();
        if state
            .completion_review
            .prompt_frames
            .0
            .last()
            .is_none_or(|f| f.attempt.as_ref() != Some(&attempt))
        {
            return Ok(ReviewAdvice::FirstSend(Box::new(PendingReviewAdvice {
                attempt,
                item,
            })));
        }
        drop(state);
        self.record_review_advice_locked(context, &item).await;
        if !self.persist_completion_review_locked().await {
            return Err(CodexErr::Fatal(
                "Could not persist review advisory admission.".into(),
            ));
        }
        Ok(ReviewAdvice::Recorded)
    }

    async fn record_review_advice_locked(&self, context: &TurnContext, item: &ResponseItem) {
        if self
            .state
            .lock()
            .await
            .completion_review
            .handoff
            .as_ref()
            .is_none_or(|h| h.advisory_recorded)
        {
            return;
        }
        self.record_conversation_items(context, std::slice::from_ref(item))
            .await;
        if let Some(handoff) = self.state.lock().await.completion_review.handoff.as_mut() {
            handoff.advisory_recorded = true;
        }
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
