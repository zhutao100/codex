//! Composer input flow coordination for ChatWidget.

use super::*;

impl ChatWidget {
    pub(super) fn is_user_turn_pending_or_running(&self) -> bool {
        self.user_turn_pending_start || self.bottom_pane.is_task_running()
    }

    pub(super) fn is_plan_streaming_in_tui(&self) -> bool {
        self.plan_stream_controller.is_some()
    }

    pub(super) fn only_user_shell_commands_running(&self) -> bool {
        self.agent_turn_running
            && !self.running_commands.is_empty()
            && self
                .running_commands
                .values()
                .all(|command| command.source == ExecCommandSource::UserShell)
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
                let mut tag = String::new();
                if matches!(message.action, QueuedInputAction::Plain)
                    && (message.model_override.is_some() || message.effort_override.is_some())
                {
                    let effective_model =
                        message.model_override.as_deref().unwrap_or(session_model);
                    let effective_effort = message.effort_override.unwrap_or(session_effort);
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
        self.queue_user_message_with_action(user_message, QueuedInputAction::Plain, Vec::new());
    }

    pub(super) fn queue_user_message_with_action(
        &mut self,
        user_message: UserMessage,
        action: QueuedInputAction,
        pending_pastes: Vec<(String, String)>,
    ) {
        self.queue_user_message_with_options(user_message, None, None, action, pending_pastes);
    }

    pub(super) fn queue_user_message_with_overrides(
        &mut self,
        user_message: UserMessage,
        model_override: Option<String>,
        effort_override: Option<Option<ReasoningEffortConfig>>,
    ) {
        self.queue_user_message_with_options(
            user_message,
            model_override,
            effort_override,
            QueuedInputAction::Plain,
            Vec::new(),
        );
    }

    pub(super) fn queue_user_message_with_options(
        &mut self,
        user_message: UserMessage,
        model_override: Option<String>,
        effort_override: Option<Option<ReasoningEffortConfig>>,
        action: QueuedInputAction,
        pending_pastes: Vec<(String, String)>,
    ) {
        let (model_override, effort_override) = if matches!(action, QueuedInputAction::Plain) {
            (model_override, effort_override)
        } else {
            (None, None)
        };
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
                action,
                pending_pastes,
            };
            self.queued_user_messages.push_back(queued);
            self.refresh_pending_input_preview();
        } else {
            self.submit_user_message_with_overrides(user_message, model_override, effort_override);
        }
    }

    pub(super) fn maybe_send_next_queued_input(&mut self) {
        if self.suppress_queue_autosend {
            return;
        }
        if self.is_user_turn_pending_or_running()
            || self.queued_edit_state.is_some()
            || !self.bottom_pane.no_modal_or_popup_active()
        {
            return;
        }
        while !self.is_user_turn_pending_or_running()
            && self.queued_edit_state.is_none()
            && self.bottom_pane.no_modal_or_popup_active()
        {
            if !self.rejected_steers_queue.is_empty() {
                let mut rejected_messages = Vec::new();
                while let Some(message) = self.rejected_steers_queue.pop_front() {
                    let history_record = self
                        .rejected_steer_history_records
                        .pop_front()
                        .unwrap_or(UserMessageHistoryRecord::UserMessageText);
                    rejected_messages.push(user_message_for_history(message, &history_record));
                }
                self.submit_user_message(merge_user_messages(rejected_messages));
                break;
            }
            let Some(queued) = self.queued_user_messages.pop_front() else {
                break;
            };
            match queued.action {
                QueuedInputAction::Plain => {
                    self.submit_queued_user_message(queued);
                    break;
                }
                QueuedInputAction::ParseSlash => {
                    if self.submit_queued_slash_prompt(queued) == QueueDrain::Stop {
                        break;
                    }
                }
                QueuedInputAction::RunShell => {
                    if self.submit_queued_shell_prompt(queued) == QueueDrain::Stop {
                        break;
                    }
                }
            }
        }
        // Update the list to reflect the remaining queued messages (if any).
        self.refresh_pending_input_preview();
    }

    pub(super) fn with_queue_autosend_suppressed(&mut self, f: impl FnOnce(&mut Self)) {
        let previous = self.suppress_queue_autosend;
        self.suppress_queue_autosend = true;
        f(self);
        self.suppress_queue_autosend = previous;
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
