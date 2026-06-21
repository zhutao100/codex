use super::*;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ReconstructedRollout {
    pub(crate) history: Vec<ResponseItem>,
    pub(crate) reference_context_item: Option<TurnContextItem>,
    pub(crate) previous_turn_settings: Option<PreviousTurnSettings>,
    pub(crate) pending_continuation: Option<PendingContinuation>,
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
        let mut current_metadata = ReplayMetadata::default();
        let mut current_epoch = ReplayEpoch::default();
        let mut pending_context: Option<TurnContextItem> = None;
        let mut awaiting_adjacent_post_compaction_context = false;
        let mut latest_compaction_tail_kind = CompactionTailKind::None;
        let mut checkpoint_after_latest_compaction = false;

        for item in rollout_items {
            match item {
                RolloutItem::ResponseItem(response_item) => {
                    awaiting_adjacent_post_compaction_context = false;
                    history.record_items(
                        std::iter::once(response_item),
                        turn_context.truncation_policy,
                    );
                    if is_user_turn_boundary_response_item(response_item) {
                        if turn::is_review_rollout_user_message(response_item) {
                            current_metadata.commit_reference_context(pending_context.take());
                        } else {
                            current_metadata.commit_user_boundary(pending_context.take());
                        }
                        current_epoch.push_checkpoint(&current_metadata);
                        checkpoint_after_latest_compaction = true;
                    }
                }
                RolloutItem::Compacted(compacted) => {
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
                    current_metadata.reference_context_item = None;
                    pending_context = None;
                    current_epoch.reset_to_base(current_metadata.clone());
                    awaiting_adjacent_post_compaction_context = has_exact_replacement;
                    latest_compaction_tail_kind = CompactionTailKind::StandaloneOrPreTurn;
                    checkpoint_after_latest_compaction = false;
                }
                RolloutItem::TurnContext(item) => {
                    if awaiting_adjacent_post_compaction_context {
                        current_metadata.reference_context_item = Some(item.clone());
                        current_epoch.base_metadata.reference_context_item = Some(item.clone());
                        latest_compaction_tail_kind =
                            CompactionTailKind::MidTurnWithInjectedContext;
                        awaiting_adjacent_post_compaction_context = false;
                    } else {
                        pending_context = Some(item.clone());
                    }
                }
                RolloutItem::EventMsg(EventMsg::ThreadRolledBack(rollback)) => {
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
                RolloutItem::EventMsg(_) | RolloutItem::SessionMeta(_) => {
                    awaiting_adjacent_post_compaction_context = false;
                }
            }
        }

        let history = history.raw_items().to_vec();
        let suppress_compaction_continuation = latest_compaction_tail_kind
            == CompactionTailKind::StandaloneOrPreTurn
            && !checkpoint_after_latest_compaction;
        let pending_continuation = (history_needs_continuation(&history)
            && !suppress_compaction_continuation)
            .then(|| PendingContinuation {
                source: TurnContinuationSource::Interrupted,
                continued_from_turn_id: None,
                model: current_metadata
                    .previous_turn_settings
                    .as_ref()
                    .map(|settings| settings.model.clone()),
                pause_reason: None,
                target: PendingContinuationTarget::Regular,
            });

        ReconstructedRollout {
            history,
            reference_context_item: current_metadata.reference_context_item,
            previous_turn_settings: current_metadata.previous_turn_settings,
            pending_continuation,
        }
    }
}
