use super::*;
use crate::completion_review::CompletionReviewState;
use crate::completion_review::ReviewHandoff;
use crate::completion_review::ReviewHandoffDisposition;
use crate::tasks::post_turn_completion_review::render_advisory_developer_message;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ReconstructedRollout {
    pub(crate) history: Vec<ResponseItem>,
    pub(crate) token_usage: Option<ReconstructedTokenUsage>,
    pub(crate) reference_context_item: Option<TurnContextItem>,
    pub(crate) previous_turn_settings: Option<PreviousTurnSettings>,
    pub(crate) pending_continuation: Option<PendingContinuation>,
    pub(crate) completion_review: Option<CompletionReviewState>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ReconstructedTokenUsage {
    pub(crate) info: TokenUsageInfo,
    pub(crate) helper: Option<Box<crate::protocol::HelperUsageContext>>,
    pub(crate) source_model: Option<String>,
    pub(crate) request_context_tokens: i64,
    pub(crate) server_reasoning_included: bool,
    pub(crate) last_usage_item_count: usize,
    pub(crate) measured_item_tokens: Option<i64>,
    pub(crate) measured_format_delta: Option<i64>,
    pub(crate) measured_base_tokens: Option<i64>,
    pub(crate) measured_tool_tokens: Option<i64>,
    pub(crate) measured_surface: Option<codex_protocol::protocol::MeasuredInputSurface>,
    pub(crate) rejected_context: Option<codex_protocol::protocol::RejectedContext>,
    pub(crate) work_notes_baseline: Option<crate::context_manager::WorkNotesBaseline>,
}

#[derive(Debug, Clone, Default, PartialEq)]
struct ReplayMetadata {
    reference_context_item: Option<TurnContextItem>,
    previous_turn_settings: Option<PreviousTurnSettings>,
}

#[derive(Debug, Clone, Default, PartialEq)]
struct ReplayEpoch {
    base_metadata: ReplayMetadata,
    post_base_checkpoints: Vec<ReplayMetadata>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum CompactionTailKind {
    #[default]
    None,
    StandaloneOrPreTurn,
    MidTurnWithInjectedContext,
}

impl ReplayMetadata {
    fn commit_user_boundary(&mut self, pending_context: Option<TurnContextItem>) {
        if let Some(context_item) = pending_context {
            self.reference_context_item = Some(context_item.clone());
            self.previous_turn_settings = Some(PreviousTurnSettings {
                collaboration_mode: context_item.collaboration_mode,
                model: context_item.model,
            });
        }
    }

    fn commit_reference_context(&mut self, pending_context: Option<TurnContextItem>) {
        if let Some(context_item) = pending_context {
            self.reference_context_item = Some(context_item);
        }
    }
}

impl ReplayEpoch {
    fn reset_to_base(&mut self, base_metadata: ReplayMetadata) {
        self.base_metadata = base_metadata;
        self.post_base_checkpoints.clear();
    }

    fn push_checkpoint(&mut self, metadata: &ReplayMetadata) {
        self.post_base_checkpoints.push(metadata.clone());
    }

    fn apply_rollback(
        &mut self,
        metadata: &mut ReplayMetadata,
        pending_context: &mut Option<TurnContextItem>,
        num_turns: u32,
    ) -> bool {
        if num_turns == 0 {
            return !self.post_base_checkpoints.is_empty();
        }

        *pending_context = None;

        let turns_to_drop = usize::try_from(num_turns).unwrap_or(usize::MAX);
        if turns_to_drop <= self.post_base_checkpoints.len() {
            let keep = self.post_base_checkpoints.len() - turns_to_drop;
            self.post_base_checkpoints.truncate(keep);
            *metadata = self
                .post_base_checkpoints
                .last()
                .cloned()
                .unwrap_or_else(|| self.base_metadata.clone());
            return !self.post_base_checkpoints.is_empty();
        }

        *metadata = ReplayMetadata::default();
        self.post_base_checkpoints.clear();
        self.base_metadata = ReplayMetadata::default();
        false
    }
}

impl Session {
    pub(crate) async fn reconstruct_history_from_rollout(
        &self,
        turn_context: &TurnContext,
        rollout_items: &[RolloutItem],
    ) -> ReconstructedRollout {
        let mut history = ContextManager::new();
        let base = self.get_base_instructions().await;
        let mut superseded_usage = None;
        let mut server_reasoning_included = false;
        let mut current_metadata = ReplayMetadata::default();
        let mut current_epoch = ReplayEpoch::default();
        let mut pending_context: Option<TurnContextItem> = None;
        let mut awaiting_adjacent_post_compaction_context = false;
        let mut latest_compaction_tail_kind = CompactionTailKind::None;
        let mut checkpoint_after_latest_compaction = false;
        let mut completion_review: Option<CompletionReviewState> = None;
        let mut recorded_advice = None;
        let mut awaiting_compact_followup = false;

        for (index, item) in rollout_items.iter().enumerate() {
            match item {
                RolloutItem::ResponseItem(response_item) => {
                    awaiting_adjacent_post_compaction_context = false;
                    if matches!(response_item, ResponseItem::Message { role, .. } if role == "developer")
                        && let Some(handoff) =
                            completion_review.as_mut().and_then(pending_started_advice)
                        && response_item == &handoff.advisory_item()
                    {
                        recorded_advice = Some((history.raw_items().len(), handoff.ticket.clone()));
                    }
                    let input_installed = completion_review
                        .as_mut()
                        .is_some_and(|review| review.observe_input_installation(response_item));
                    let policy = match (
                        response_item,
                        index.checked_sub(1).map(|i| &rollout_items[i]),
                    ) {
                        (
                            ResponseItem::FunctionCallOutput { call_id, .. }
                            | ResponseItem::CustomToolCallOutput { call_id, .. },
                            Some(RolloutItem::ToolOutputPolicy {
                                call_id: owner,
                                policy,
                            }),
                        ) if owner == call_id => Some(*policy),
                        _ => pending_context
                            .as_ref()
                            .or(current_metadata.reference_context_item.as_ref())
                            .and_then(|context| context.truncation_policy),
                    };
                    history.record_session_item(
                        response_item,
                        policy
                            .map(Into::into)
                            .unwrap_or(turn_context.truncation_policy),
                        server_reasoning_included,
                    );
                    if is_user_turn_boundary_response_item(response_item) {
                        if turn::is_review_rollout_user_message(response_item) {
                            current_metadata.commit_reference_context(pending_context.take());
                        } else {
                            current_metadata.commit_user_boundary(pending_context.take());
                            if let Some(review) = completion_review.as_mut() {
                                // A known amendment invalidates eligibility, not
                                // the prior candidate's continuation directory.
                                if !input_installed {
                                    review.invalidate_request_provenance();
                                    review.episode = None;
                                }
                                review.clear_operations();
                            }
                        }
                        current_epoch.push_checkpoint(&current_metadata);
                        checkpoint_after_latest_compaction = true;
                    }
                }
                RolloutItem::ToolOutputPolicy { .. } | RolloutItem::SampledInstructions { .. } => {}
                RolloutItem::Compacted(compacted) => {
                    recorded_advice = None;
                    // The marker alone commits a fresh history. A steer can still arrive
                    // before its first ordinary continuation, even if none was queued at install.
                    awaiting_compact_followup = true;
                    superseded_usage = history.token_info();
                    if let Some(review) = completion_review.as_mut() {
                        review.forget_compaction_source();
                    }
                    let has_exact_replacement = compacted.replacement_history.is_some();
                    if let Some(replacement) = &compacted.replacement_history {
                        history.replace(replacement.clone());
                    } else {
                        let user_messages = collect_user_messages(history.raw_items());
                        let rebuilt = compact::build_compacted_history(
                            Vec::new(),
                            &user_messages,
                            &compacted.message,
                        );
                        history.replace(rebuilt);
                    }
                    // A committed marker does not depend on the startup model or on a
                    // subsequent reference/usage write making it to disk.
                    let installed = compacted
                        .checkpoint
                        .as_ref()
                        .filter(|_| has_exact_replacement);
                    if let Some(checkpoint) = installed {
                        history.set_token_info_with_boundary(
                            Some(checkpoint.usage.clone()),
                            history.raw_items().len(),
                        );
                        server_reasoning_included = true;
                    } else if superseded_usage.is_some() {
                        let destination = match rollout_items.get(index + 1) {
                            Some(RolloutItem::TurnContext(ctx)) if has_exact_replacement => {
                                Some(ctx.model.as_str())
                            }
                            _ => completion_review
                                .as_ref()
                                .and_then(|r| r.recovery_model.as_deref()),
                        };
                        let destination = if let Some(model) =
                            destination.filter(|model| *model != turn_context.model_info.slug)
                        {
                            self.turn_context_with_model(turn_context, model).await.ok()
                        } else {
                            None
                        };
                        if let Some(destination) = destination {
                            let base = self.get_base_instructions_for_turn(&destination).await;
                            history.reestimate_usage(&base, destination.model_context_window());
                        } else {
                            history.reestimate_usage(&base, turn_context.model_context_window());
                        }
                        server_reasoning_included = true;
                    }
                    current_metadata.reference_context_item =
                        installed.and_then(|c| c.injected_context.clone());
                    pending_context = None;
                    current_epoch.reset_to_base(current_metadata.clone());
                    awaiting_adjacent_post_compaction_context = has_exact_replacement
                        && (installed.is_none()
                            || current_metadata.reference_context_item.is_some());
                    latest_compaction_tail_kind =
                        if current_metadata.reference_context_item.is_some() {
                            CompactionTailKind::MidTurnWithInjectedContext
                        } else {
                            CompactionTailKind::StandaloneOrPreTurn
                        };
                    checkpoint_after_latest_compaction = false;
                }
                RolloutItem::ContextSeed { items, reference } => {
                    let policy = reference
                        .truncation_policy
                        .map(Into::into)
                        .unwrap_or(turn_context.truncation_policy);
                    for item in items {
                        history.record_session_item(item, policy, server_reasoning_included);
                    }
                    current_metadata.reference_context_item = Some(reference.clone());
                    current_epoch.base_metadata.reference_context_item = Some(reference.clone());
                    pending_context = None;
                    awaiting_adjacent_post_compaction_context = false;
                }
                RolloutItem::TurnContext(item) => {
                    if awaiting_adjacent_post_compaction_context
                        && current_metadata
                            .reference_context_item
                            .as_ref()
                            .is_none_or(|r| r == item)
                    {
                        current_metadata.reference_context_item = Some(item.clone());
                        current_epoch.base_metadata.reference_context_item = Some(item.clone());
                        latest_compaction_tail_kind =
                            CompactionTailKind::MidTurnWithInjectedContext;
                    } else {
                        if completion_review
                            .as_mut()
                            .is_some_and(|r| r.observe_input_reference(item))
                        {
                            // The installation journal owns this reference, but only the actual
                            // user message below it can advance the user turn or rollback epoch.
                            current_metadata.reference_context_item = Some(item.clone());
                        }
                        pending_context = Some(item.clone());
                    }
                    awaiting_adjacent_post_compaction_context = false;
                }
                RolloutItem::EventMsg(EventMsg::ThreadRolledBack(rollback)) => {
                    recorded_advice = None;
                    completion_review = None;
                    awaiting_compact_followup = false;
                    awaiting_adjacent_post_compaction_context = false;
                    history.drop_last_n_user_turns(rollback.num_turns);
                    let turns_to_drop = usize::try_from(rollback.num_turns).unwrap_or(usize::MAX);
                    let crosses_epoch_base =
                        turns_to_drop > current_epoch.post_base_checkpoints.len();
                    checkpoint_after_latest_compaction = current_epoch.apply_rollback(
                        &mut current_metadata,
                        &mut pending_context,
                        rollback.num_turns,
                    );
                    if crosses_epoch_base && latest_compaction_tail_kind != CompactionTailKind::None
                    {
                        latest_compaction_tail_kind = CompactionTailKind::StandaloneOrPreTurn;
                    }
                }
                RolloutItem::EventMsg(EventMsg::UserMessage(_)) => {
                    awaiting_adjacent_post_compaction_context = false;
                }
                RolloutItem::EventMsg(EventMsg::TurnAborted(_)) => {
                    awaiting_adjacent_post_compaction_context = false;
                }
                RolloutItem::PostTurnCompletionReviewState(value) => {
                    completion_review = CompletionReviewState::from_rollout(value);
                    if let Some(RolloutItem::TurnContext(reference)) =
                        index.checked_sub(1).map(|i| &rollout_items[i])
                        && pending_context.as_ref() == Some(reference)
                        && completion_review
                            .as_ref()
                            .is_some_and(|r| r.owns_legacy_prepared_reference(reference))
                    {
                        current_metadata.reference_context_item = Some(reference.clone());
                    }
                    if value["awaiting_compact_followup"] == false {
                        awaiting_compact_followup = false;
                    } else if !awaiting_compact_followup
                        && let Some(review) = &mut completion_review
                    {
                        // A positive ordinary receipt can precede the owner's release snapshot.
                        review.release_owed_response(None);
                    }
                    if value["episode"]["automatic_reviews_started"].as_u64() == Some(3)
                        && let Some(review) = completion_review.as_mut()
                        && let Some(handoff) = legacy_capped_handoff(
                            &rollout_items[..index],
                            review,
                            &turn_context.model_info.slug,
                        )
                    {
                        review.attempt = Some(handoff.ticket.clone());
                        review.handoff = Some(handoff);
                    }
                }
                RolloutItem::TokenUsageContext {
                    info,
                    helper: None,
                    rejected_context: None,
                    ..
                }
                | RolloutItem::EventMsg(EventMsg::TokenCount(TokenCountEvent {
                    info: Some(info),
                    ..
                })) if history.is_legacy_capacity_signal(info) => {
                    // A display remainder is neither a bill nor a new work or refusal receipt.
                    awaiting_adjacent_post_compaction_context = false;
                }
                RolloutItem::TokenUsageContext {
                    info,
                    helper,
                    source_model,
                    request_context_tokens,
                    server_reasoning_included: included,
                    last_usage_item_count,
                    measured_item_tokens,
                    measured_format_delta,
                    measured_base_tokens,
                    measured_tool_tokens,
                    measured_surface,
                    rejected_context,
                } => {
                    awaiting_adjacent_post_compaction_context = false;
                    if awaiting_compact_followup
                        && helper.is_none()
                        && rejected_context.is_none()
                        && info.last_token_usage.input_tokens > 0
                    {
                        awaiting_compact_followup = false;
                        if let Some(review) = &mut completion_review {
                            review.release_owed_response(None);
                        }
                    }
                    if let Some(covered_item_count) = last_usage_item_count {
                        // Continuation cleanup can remove a previously covered suffix.
                        history.set_token_info_with_boundary(
                            Some(info.clone()),
                            (*covered_item_count).min(history.raw_items().len()),
                        );
                    } else {
                        history.set_token_info(Some(info.clone()));
                    }
                    history.set_measured_context(
                        (*request_context_tokens).max(0),
                        source_model.clone(),
                    );
                    history.measured_item_tokens = measured_item_tokens.filter(|cost| *cost >= 0);
                    history.measured_format_delta = *measured_format_delta;
                    history.measured_base_tokens = measured_base_tokens.filter(|&cost| cost >= 0);
                    history.measured_tool_tokens = measured_tool_tokens.filter(|&cost| cost >= 0);
                    history.measured_surface.clone_from(measured_surface);
                    history.rejected_context.clone_from(rejected_context);
                    history.helper_receipt.clone_from(helper);
                    superseded_usage = None;
                    server_reasoning_included = *included;
                }
                RolloutItem::EventMsg(EventMsg::TokenCount(TokenCountEvent {
                    info: Some(info),
                    ..
                })) => {
                    awaiting_adjacent_post_compaction_context = false;
                    // Repeated UI events must not cover later, unreported output.
                    if history.token_info().as_ref() != Some(info)
                        && superseded_usage.as_ref() != Some(info)
                    {
                        history.set_token_info(Some(info.clone()));
                        superseded_usage = None;
                        server_reasoning_included = false;
                    }
                }
                RolloutItem::EventMsg(_) | RolloutItem::SessionMeta(_) => {
                    awaiting_adjacent_post_compaction_context = false;
                }
            }
        }

        if let Some(review) = &mut completion_review {
            review.awaiting_compact_followup = awaiting_compact_followup;
        }
        let token_usage = history.token_info().map(|info| ReconstructedTokenUsage {
            info,
            helper: history.helper_receipt.clone(),
            source_model: history.usage_source_model().map(str::to_owned),
            request_context_tokens: history.request_context_tokens(),
            server_reasoning_included,
            last_usage_item_count: history.last_usage_item_count(),
            measured_item_tokens: history.measured_item_tokens,
            measured_format_delta: history.measured_format_delta,
            measured_base_tokens: history.measured_base_tokens,
            measured_tool_tokens: history.measured_tool_tokens,
            measured_surface: history.measured_surface.clone(),
            rejected_context: history.rejected_context.clone(),
            work_notes_baseline: history.work_notes_baseline.clone(),
        });
        let history = history.raw_items().to_vec();
        // Advice and its frame selection are separate writes. Only the handoff active
        // at the actual append can claim it. An interrupt marker or stale snapshot
        // can follow that append without undoing it.
        if let Some(handoff) = completion_review.as_mut().and_then(pending_started_advice)
            && let Some((index, owner)) = recorded_advice
            && owner == handoff.ticket
            && history.get(index) == Some(&handoff.advisory_item())
        {
            handoff.advisory_recorded = true;
        }
        let pending_notes = history
            .iter()
            .rev()
            .any(compact::is_work_notes_request_item);
        let suppress_compaction_continuation = latest_compaction_tail_kind
            == CompactionTailKind::StandaloneOrPreTurn
            && !checkpoint_after_latest_compaction
            && !pending_notes;
        let encrypted_only_continuation = latest_compaction_tail_kind
            == CompactionTailKind::MidTurnWithInjectedContext
            && !history.iter().any(is_user_turn_boundary_response_item)
            && history
                .iter()
                .rposition(|item| matches!(item, ResponseItem::Compaction { .. }))
                .is_some_and(|index| turn::tail_needs_continuation(&history[index + 1..]));
        // A pre-turn capture uses the resume-selected destination. After installation
        // the adjacent reference owns that destination, even when the older task still
        // owns the response or has unconsumed input.
        let regular_model = if pending_notes {
            Some(turn_context.model_info.slug.clone())
        } else if latest_compaction_tail_kind == CompactionTailKind::MidTurnWithInjectedContext
            && !checkpoint_after_latest_compaction
        {
            current_metadata
                .reference_context_item
                .as_ref()
                .map(|item| item.model.clone())
        } else {
            current_metadata
                .previous_turn_settings
                .as_ref()
                .map(|settings| settings.model.clone())
        };
        let mut pending_continuation = ((history_needs_continuation(&history)
            || encrypted_only_continuation)
            && !suppress_compaction_continuation)
            .then(|| PendingContinuation {
                source: TurnContinuationSource::Interrupted,
                continued_from_turn_id: None,
                model: regular_model.clone(),
                pause_reason: None,
                target: PendingContinuationTarget::Regular,
            });
        if let Some(review) = completion_review.as_ref()
            && (!review.pending_inputs.is_empty()
                || (review.main_task_id.is_some()
                    && !review.request_frame.current_user_messages.is_empty()))
        {
            pending_continuation = Some(PendingContinuation {
                source: TurnContinuationSource::Interrupted,
                continued_from_turn_id: review.main_task_id.clone(),
                model: review.recovery_model.clone().or(regular_model),
                pause_reason: None,
                target: PendingContinuationTarget::Regular,
            });
        }
        if let Some(review) = completion_review.as_ref()
            && let Some(ticket) = review.attempt.as_ref()
            && review.ticket_is_current(ticket)
        {
            match review.handoff.as_ref().map(|handoff| &handoff.disposition) {
                Some(ReviewHandoffDisposition::Prepared | ReviewHandoffDisposition::Deferred) => {}
                Some(ReviewHandoffDisposition::Started { turn_id }) => {
                    pending_continuation = Some(PendingContinuation {
                        source: TurnContinuationSource::Interrupted,
                        continued_from_turn_id: Some(turn_id.clone()),
                        model: review.recovery_model.clone().or_else(|| {
                            review.handoff.as_ref().map(|handoff| handoff.model.clone())
                        }),
                        pause_reason: None,
                        target: PendingContinuationTarget::Regular,
                    });
                }
                None => {
                    pending_continuation = Some(PendingContinuation {
                        source: TurnContinuationSource::Paused,
                        continued_from_turn_id: Some(ticket.candidate_completion_id.clone()),
                        model: None,
                        pause_reason: None,
                        target: PendingContinuationTarget::PostTurnCompletionReview,
                    });
                }
            }
        }

        ReconstructedRollout {
            history,
            token_usage,
            reference_context_item: current_metadata.reference_context_item,
            previous_turn_settings: current_metadata.previous_turn_settings,
            pending_continuation,
            completion_review,
        }
    }
}

fn pending_started_advice(review: &mut CompletionReviewState) -> Option<&mut ReviewHandoff> {
    let handoff = review.handoff.as_ref()?;
    if review.replacement_answer_required
        && !handoff.advisory_recorded
        && matches!(
            handoff.disposition,
            ReviewHandoffDisposition::Started { .. }
        )
        && review.ticket_is_current(&handoff.ticket)
    {
        review.handoff.as_mut()
    } else {
        None
    }
}

// The old cap wrote an actionable exit followed by an otherwise unchanged
// checkpoint with the reservation cleared. Do not infer advice from a verdict
// without both of those checkpoints, or from a stopped or already applied handoff.
fn legacy_capped_handoff(
    prefix: &[RolloutItem],
    finished: &CompletionReviewState,
    model: &str,
) -> Option<ReviewHandoff> {
    let [
        ..,
        RolloutItem::PostTurnCompletionReviewState(reserved),
        RolloutItem::EventMsg(EventMsg::EnteredReviewMode(_)),
        RolloutItem::EventMsg(EventMsg::ExitedReviewMode(exit)),
    ] = prefix
    else {
        return None;
    };
    let output = exit.post_turn_completion_review_output.as_ref()?;
    if !output.fix_actions_advised
        || output.evaluation.trim().is_empty()
        || reserved["episode"]["automatic_reviews_started"].as_u64() != Some(3)
    {
        return None;
    }
    let mut reserved = CompletionReviewState::from_rollout(reserved)?;
    let ticket = reserved.attempt.clone()?;
    if !ticket.automatic
        || !reserved.ticket_is_current(&ticket)
        || reserved.handoff.is_some()
        || reserved.main_task_id.is_some()
        || reserved.replacement_answer_required
    {
        return None;
    }
    reserved.clear_operations();
    (reserved == *finished).then(|| ReviewHandoff {
        ticket,
        advisory: render_advisory_developer_message(output),
        model: model.to_string(),
        disposition: ReviewHandoffDisposition::Deferred,
        advisory_recorded: false,
    })
}
