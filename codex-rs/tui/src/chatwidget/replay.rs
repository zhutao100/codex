//! History replay and review-mode replay handlers for ChatWidget.

use super::*;

impl ChatWidget {
    pub(super) fn on_get_history_entry_response(
        &mut self,
        event: codex_core::protocol::GetHistoryEntryResponseEvent,
    ) {
        let codex_core::protocol::GetHistoryEntryResponseEvent {
            offset,
            log_id,
            entry,
        } = event;
        self.bottom_pane
            .on_history_entry_response(log_id, offset, entry.map(|e| e.text));
    }

    /// Replay a subset of initial events into the UI to seed the transcript when
    /// resuming an existing session. This approximates the live event flow and
    /// is intentionally conservative: only safe-to-replay items are rendered to
    /// avoid triggering side effects. Event ids are passed as `None` to
    /// distinguish replayed events from live ones.
    pub(super) fn replay_initial_messages(&mut self, events: Vec<EventMsg>) {
        for msg in events {
            if matches!(
                msg,
                EventMsg::SessionConfigured(_) | EventMsg::ThreadNameUpdated(_)
            ) {
                continue;
            }
            // `id: None` indicates a synthetic/fake id coming from replay.
            self.dispatch_event_msg(None, msg, true);
        }
    }

    pub(crate) fn handle_codex_event_replay(&mut self, event: Event) {
        let Event { msg, .. } = event;
        if matches!(msg, EventMsg::ShutdownComplete) {
            return;
        }
        self.dispatch_event_msg(None, msg, true);
    }

    pub(super) fn on_entered_review_mode(&mut self, review: ReviewRequest, from_replay: bool) {
        // Enter review mode and emit a concise banner
        if self.pre_review_token_info.is_none() {
            self.pre_review_token_info = Some(self.token_info.clone());
        }
        // Avoid toggling running state for replayed history events on resume.
        if !from_replay && !self.bottom_pane.is_task_running() {
            self.bottom_pane.set_task_running(true);
        }
        self.is_review_mode = true;
        let hint = review
            .user_facing_hint
            .unwrap_or_else(|| codex_core::review_prompts::user_facing_hint(&review.target));
        let banner = format!(">> Code review started: {hint} <<");
        self.add_to_history(history_cell::new_review_status_line(banner));
        self.request_redraw();
    }

    pub(super) fn on_exited_review_mode(&mut self, review: ExitedReviewModeEvent) {
        // Leave review mode; if output is present, flush pending stream + show results.
        if let Some(output) = review.review_output {
            self.flush_answer_stream_with_separator();
            self.flush_interrupt_queue();
            self.flush_active_cell();

            if output.findings.is_empty() {
                let explanation = output.overall_explanation.trim().to_string();
                if explanation.is_empty() {
                    tracing::error!("Reviewer failed to output a response.");
                    self.add_to_history(history_cell::new_error_event(
                        "Reviewer failed to output a response.".to_owned(),
                    ));
                } else {
                    // Show explanation when there are no structured findings.
                    let mut rendered: Vec<ratatui::text::Line<'static>> = vec!["".into()];
                    append_markdown(&explanation, None, &mut rendered);
                    let body_cell = AgentMessageCell::new(rendered, false);
                    self.app_event_tx
                        .send(AppEvent::InsertHistoryCell(Box::new(body_cell)));
                }
            }
            // Final message is rendered as part of the AgentMessage.
        }
        if let Some(output) = review.post_turn_completion_review_output {
            self.flush_answer_stream_with_separator();
            self.flush_interrupt_queue();
            self.flush_active_cell();

            let mut rendered: Vec<ratatui::text::Line<'static>> = vec!["".into()];
            append_markdown(output.evaluation.trim(), None, &mut rendered);
            rendered.push("".into());
            rendered.push(
                format!(
                    "Fix actions advised: {}",
                    if output.fix_actions_advised {
                        "yes"
                    } else {
                        "no"
                    }
                )
                .into(),
            );
            let body_cell = AgentMessageCell::new(rendered, false);
            self.app_event_tx
                .send(AppEvent::InsertHistoryCell(Box::new(body_cell)));
        }

        self.is_review_mode = false;
        self.restore_pre_review_token_info();
        // Append a finishing banner at the end of this turn.
        self.add_to_history(history_cell::new_review_status_line(
            "<< Code review finished >>".to_string(),
        ));
        self.request_redraw();
    }
}
