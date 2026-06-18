//! Composer input flow coordination for ChatWidget.

use super::*;

impl ChatWidget {
    pub(super) fn is_user_turn_pending_or_running(&self) -> bool {
        self.user_turn_pending_start || self.bottom_pane.is_task_running()
    }

    pub(super) fn refresh_pending_input_preview(&mut self) {
        let session_model = self.current_model();
        let session_effort = self.effective_reasoning_effort();
        let editing_id = self
            .queued_edit_state
            .as_ref()
            .map(|state| state.selected_id);
        let queued: Vec<String> = self
            .queued_user_messages
            .iter()
            .map(|message| {
                let effective_model = message.model_override.as_deref().unwrap_or(session_model);
                let effective_effort = message.effort_override.unwrap_or(session_effort);
                let mut tag = String::new();
                if message.model_override.is_some() || message.effort_override.is_some() {
                    tag = format!(
                        "[{effective_model} · reasoning {}] ",
                        Self::status_line_reasoning_effort_label(effective_effort)
                    );
                }

                if Some(message.id) == editing_id {
                    format!("✎ {tag}{}", message.text)
                } else {
                    format!("{tag}{}", message.text)
                }
            })
            .collect();
        let pending_steers = self
            .pending_steers
            .iter()
            .map(|steer| {
                user_message_preview_text(&steer.user_message, Some(&steer.history_record))
            })
            .collect();
        let rejected_steers = self
            .rejected_steers_queue
            .iter()
            .enumerate()
            .map(|(idx, message)| {
                user_message_preview_text(message, self.rejected_steer_history_records.get(idx))
            })
            .collect();
        self.bottom_pane
            .set_pending_input_preview(queued, pending_steers, rejected_steers);
    }

    pub(super) fn queue_user_message(&mut self, user_message: UserMessage) {
        self.queue_user_message_with_overrides(user_message, None, None);
    }

    pub(super) fn queue_user_message_with_overrides(
        &mut self,
        user_message: UserMessage,
        model_override: Option<String>,
        effort_override: Option<Option<ReasoningEffortConfig>>,
    ) {
        if !self.is_session_configured()
            || self.is_user_turn_pending_or_running()
            || self.is_review_mode
        {
            let id = self.next_queued_user_message_id;
            self.next_queued_user_message_id = self.next_queued_user_message_id.saturating_add(1);
            let queued = QueuedUserMessage {
                id,
                text: user_message.text,
                local_images: user_message.local_images,
                text_elements: user_message.text_elements,
                mention_paths: user_message.mention_paths,
                model_override,
                effort_override,
            };
            self.queued_user_messages.push_back(queued);
            self.refresh_pending_input_preview();
        } else {
            self.submit_user_message_with_overrides(user_message, model_override, effort_override);
        }
    }

    pub(super) fn maybe_send_next_queued_input(&mut self) {
        if self.is_user_turn_pending_or_running()
            || self.queued_edit_state.is_some()
            || !self.bottom_pane.no_modal_or_popup_active()
        {
            return;
        }
        if let Some(rejected) = self.rejected_steers_queue.pop_front() {
            self.rejected_steer_history_records.pop_front();
            self.submit_user_message(rejected);
        } else if let Some(queued) = self.queued_user_messages.pop_front() {
            self.submit_queued_user_message(queued);
        }
        // Update the list to reflect the remaining queued messages (if any).
        self.refresh_pending_input_preview();
    }

    pub(crate) fn submit_user_message_with_mode(
        &mut self,
        text: String,
        collaboration_mode: CollaborationModeMask,
    ) {
        if self.agent_turn_running
            && self.active_collaboration_mask.as_ref() != Some(&collaboration_mode)
        {
            self.add_error_message(
                "Cannot switch collaboration mode while a turn is running.".to_string(),
            );
            return;
        }
        self.set_collaboration_mask(collaboration_mode);
        self.submit_user_message(text.into());
    }
}
