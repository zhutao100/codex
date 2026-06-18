//! Interrupted-turn input restoration for ChatWidget.

use super::*;

impl ChatWidget {
    pub(super) fn handle_steer_rejected_error(&mut self, info: &CodexErrorInfo) -> bool {
        match info {
            CodexErrorInfo::ActiveTurnNotSteerable { .. } => self.enqueue_rejected_steer(),
            CodexErrorInfo::NoActiveTurnToSteer => {
                self.active_turn_id = None;
                self.finalize_turn();
                self.enqueue_pending_steer_as_queued()
            }
            CodexErrorInfo::ExpectedTurnMismatch { actual, .. } => {
                self.active_turn_id = Some(actual.clone());
                self.enqueue_pending_steer_as_queued()
            }
            _ => false,
        }
    }

    pub(super) fn enqueue_rejected_steer(&mut self) -> bool {
        let Some(pending_steer) = self.pending_steers.pop_front() else {
            tracing::warn!(
                "received active-turn-not-steerable error without a matching pending steer"
            );
            return false;
        };
        self.rejected_steers_queue
            .push_back(pending_steer.user_message);
        self.rejected_steer_history_records
            .push_back(pending_steer.history_record);
        self.refresh_pending_input_preview();
        true
    }

    pub(super) fn enqueue_pending_steer_as_queued(&mut self) -> bool {
        let Some(pending_steer) = self.pending_steers.pop_front() else {
            tracing::warn!("received steer race error without a matching pending steer");
            return false;
        };
        let id = self.next_queued_user_message_id;
        self.next_queued_user_message_id = self.next_queued_user_message_id.saturating_add(1);
        let PendingSteer {
            target_turn_id: _,
            user_message,
            history_record: _,
            compare_key: _,
        } = pending_steer;
        self.queued_user_messages.push_front(QueuedUserMessage {
            id,
            text: user_message.text,
            local_images: user_message.local_images,
            text_elements: user_message.text_elements,
            mention_paths: user_message.mention_paths,
            model_override: None,
            effort_override: None,
            action: QueuedInputAction::Plain,
            pending_pastes: Vec::new(),
        });
        self.refresh_pending_input_preview();
        true
    }

    pub(super) fn restore_pending_steers_to_composer_or_reject(&mut self) {
        if self.pending_steers.is_empty() {
            return;
        }

        let pending_steers = self
            .pending_steers
            .drain(..)
            .map(|pending| (pending.user_message, pending.history_record))
            .collect::<Vec<_>>();

        if self.bottom_pane.composer_is_empty() {
            let restored =
                merge_user_messages(pending_steers.into_iter().map(|(message, history_record)| {
                    user_message_for_history(message, &history_record)
                }));
            self.restore_user_message_to_composer(restored);
        } else {
            for (message, history_record) in pending_steers {
                self.rejected_steers_queue.push_back(message);
                self.rejected_steer_history_records
                    .push_back(history_record);
            }
        }

        self.refresh_pending_input_preview();
    }

    pub(super) fn move_pending_steers_to_rejected_queue(&mut self) {
        if self.pending_steers.is_empty() {
            return;
        }

        for pending in self.pending_steers.drain(..) {
            self.rejected_steers_queue.push_back(pending.user_message);
            self.rejected_steer_history_records
                .push_back(pending.history_record);
        }
        self.refresh_pending_input_preview();
    }

    pub(super) fn restore_user_message_to_composer(&mut self, user_message: UserMessage) {
        let local_image_paths = user_message
            .local_images
            .into_iter()
            .map(|image| image.path)
            .collect();
        self.bottom_pane.set_composer_text_with_mention_paths(
            user_message.text,
            user_message.text_elements,
            local_image_paths,
            user_message.mention_paths,
        );
    }

    /// Handle a turn aborted due to user interrupt (Esc).
    /// Keep queued messages in the queue for later.
    pub(super) fn on_interrupted_turn(&mut self, reason: TurnAbortReason) {
        self.with_queue_autosend_suppressed(|this| {
            // Finalize, log a gentle prompt, and clear running state.
            this.finalize_turn();
            this.restore_pending_steers_to_composer_or_reject();

            if reason != TurnAbortReason::ReviewEnded {
                this.add_to_history(history_cell::new_error_event(
                "Conversation interrupted - tell the model what to do differently. Something went wrong? Hit `/feedback` to report the issue.".to_owned(),
            ));
            }

            this.request_redraw();
        });
    }
}
