use super::*;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ReconstructedRollout {
    pub(crate) history: Vec<ResponseItem>,
    pub(crate) reference_context_item: Option<TurnContextItem>,
    pub(crate) previous_turn_settings: Option<PreviousTurnSettings>,
    pub(crate) pending_continuation: Option<PendingContinuation>,
}

impl Session {
    pub(crate) async fn reconstruct_history_from_rollout(
        &self,
        turn_context: &TurnContext,
        rollout_items: &[RolloutItem],
    ) -> ReconstructedRollout {
        let mut history = ContextManager::new();
        let mut context_stack = Vec::<TurnContextItem>::new();
        let mut pending_continuation = None;

        for item in rollout_items {
            match item {
                RolloutItem::ResponseItem(response_item) => {
                    history.record_items(
                        std::iter::once(response_item),
                        turn_context.truncation_policy,
                    );
                    if is_user_turn_boundary_response_item(response_item) {
                        pending_continuation = None;
                    }
                }
                RolloutItem::Compacted(compacted) => {
                    if let Some(replacement) = &compacted.replacement_history {
                        history.replace(replacement.clone());
                    } else {
                        let user_messages = collect_user_messages(history.raw_items());
                        let rebuilt = compact::build_compacted_history(
                            self.build_initial_context(turn_context).await,
                            &user_messages,
                            &compacted.message,
                        );
                        history.replace(rebuilt);
                    }
                    context_stack.clear();
                }
                RolloutItem::TurnContext(item) => {
                    context_stack.push(item.clone());
                }
                RolloutItem::EventMsg(EventMsg::ThreadRolledBack(rollback)) => {
                    history.drop_last_n_user_turns(rollback.num_turns);
                    let turns_to_drop = usize::try_from(rollback.num_turns).unwrap_or(usize::MAX);
                    let keep = context_stack.len().saturating_sub(turns_to_drop);
                    context_stack.truncate(keep);
                    pending_continuation = None;
                }
                RolloutItem::EventMsg(EventMsg::TurnAborted(ev))
                    if ev.reason == TurnAbortReason::Interrupted =>
                {
                    pending_continuation = Some(PendingContinuation {
                        source: TurnContinuationSource::Interrupted,
                        continued_from_turn_id: None,
                    });
                }
                RolloutItem::EventMsg(EventMsg::UserMessage(_)) => {
                    pending_continuation = None;
                }
                RolloutItem::SessionMeta(_) | RolloutItem::EventMsg(_) => {}
            }
        }

        let history = history.raw_items().to_vec();
        let reference_context_item = context_stack.last().cloned();
        let previous_turn_settings =
            reference_context_item
                .as_ref()
                .map(|item| PreviousTurnSettings {
                    model: item.model.clone(),
                });
        let pending_continuation = pending_continuation.or_else(|| {
            history_needs_continuation(&history).then_some(PendingContinuation {
                source: TurnContinuationSource::Interrupted,
                continued_from_turn_id: None,
            })
        });

        ReconstructedRollout {
            history,
            reference_context_item,
            previous_turn_settings,
            pending_continuation,
        }
    }
}
