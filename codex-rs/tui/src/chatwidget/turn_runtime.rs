//! Turn runtime state transitions for ChatWidget.

use super::*;

impl ChatWidget {
    /// Synchronize the bottom-pane "task running" indicator with the current lifecycles.
    ///
    /// The bottom pane only has one running flag, but this module treats it as a derived state of
    /// both the agent turn lifecycle and MCP startup lifecycle.
    pub(super) fn update_task_running_state(&mut self) {
        self.bottom_pane
            .set_task_running(self.agent_turn_running || self.mcp_startup_status.is_some());
        if self.agent_turn_running {
            let active_model = self
                .active_runtime_context
                .as_ref()
                .and_then(|snapshot| {
                    (!snapshot.model.trim().is_empty()).then(|| snapshot.model.clone())
                })
                .or_else(|| self.running_turn_model.clone());
            let active_reasoning_effort = self
                .active_runtime_context
                .as_ref()
                .map(|snapshot| snapshot.reasoning_effort)
                .unwrap_or(self.running_turn_reasoning_effort);
            self.bottom_pane.set_active_model(active_model);
            self.bottom_pane
                .set_active_reasoning_effort(active_reasoning_effort);
        } else {
            self.bottom_pane.set_active_model(None);
            self.bottom_pane.set_active_reasoning_effort(None);
        }
    }

    pub(super) fn on_task_started(&mut self, turn_id: Option<String>) {
        self.active_turn_id = turn_id;
        self.user_turn_pending_start = false;
        if self.running_turn_model.is_none() {
            if let Some(snapshot) = self
                .active_runtime_context
                .as_ref()
                .filter(|snapshot| !snapshot.model.trim().is_empty())
            {
                self.running_turn_model = Some(snapshot.model.clone());
                self.running_turn_reasoning_effort = snapshot.reasoning_effort;
            } else {
                self.running_turn_model = Some(self.current_model().to_string());
                self.running_turn_reasoning_effort = self.effective_reasoning_effort();
            }
        }
        self.agent_turn_running = true;
        self.saw_plan_update_this_turn = false;
        self.saw_plan_item_this_turn = false;
        self.plan_delta_buffer.clear();
        self.plan_item_active = false;
        self.adaptive_chunking.reset();
        self.plan_stream_controller = None;
        self.otel_manager.reset_runtime_metrics();
        self.bottom_pane.clear_quit_shortcut_hint();
        self.quit_shortcut_expires_at = None;
        self.quit_shortcut_key = None;
        self.update_task_running_state();
        self.retry_status_header = None;
        self.bottom_pane.set_interrupt_hint_visible(true);
        self.bottom_pane.clear_progress_trace();
        self.turn_progress_trace.clear();
        self.pending_separator_progress_trace = None;
        self.set_status_header(String::from("Working"));
        self.full_reasoning_buffer.clear();
        self.reasoning_buffer.clear();
        self.refresh_status_line();
        self.request_redraw();
    }

    pub(super) fn on_task_complete(
        &mut self,
        last_agent_message: Option<String>,
        from_replay: bool,
    ) {
        if let Some(last_message) = last_agent_message.as_ref()
            && !last_message.trim().is_empty()
        {
            self.last_assistant_output_markdown = Some(last_message.clone());
            if !from_replay || !self.last_copyable_response_is(last_message) {
                self.push_copyable_message(CopyableRole::Response, last_message);
            }
            self.last_copyable_output = Some(last_message.clone());
        }
        // If a stream is currently active, finalize it.
        self.flush_answer_stream_with_separator();
        if let Some(mut controller) = self.plan_stream_controller.take()
            && let Some(cell) = controller.finalize()
        {
            self.add_boxed_history(cell);
        }
        self.flush_unified_exec_wait_streak();
        if !from_replay {
            let runtime_metrics = self.otel_manager.runtime_metrics_summary();
            let separator_trace = if self.turn_progress_trace.is_empty() {
                self.pending_separator_progress_trace.take()
            } else {
                Some(std::mem::take(&mut self.turn_progress_trace))
            };
            if runtime_metrics.is_some() {
                let elapsed_seconds = self
                    .bottom_pane
                    .status_widget()
                    .map(crate::status_indicator_widget::StatusIndicatorWidget::elapsed_seconds);
                self.add_to_history(history_cell::FinalMessageSeparator::new(
                    elapsed_seconds,
                    runtime_metrics,
                    separator_trace,
                    self.progress_trace_styles,
                ));
            }
            self.needs_final_message_separator = false;
            self.had_work_activity = false;
            self.request_status_line_branch_refresh();
        }
        // Mark task stopped and request redraw now that all content is in history.
        self.user_turn_pending_start = false;
        self.agent_turn_running = false;
        self.active_turn_id = None;
        self.running_turn_model = None;
        self.running_turn_reasoning_effort = None;
        self.update_task_running_state();
        self.bottom_pane.clear_progress_trace();
        self.turn_progress_trace.clear();
        self.pending_separator_progress_trace = None;
        self.running_commands.clear();
        self.suppressed_exec_calls.clear();
        self.last_unified_wait = None;
        self.unified_exec_wait_streak = None;
        self.clear_unified_exec_processes();
        self.refresh_status_line();
        self.request_redraw();
        if !from_replay {
            self.move_pending_steers_to_rejected_queue();
        }

        if !from_replay && !self.has_queued_follow_up_messages() {
            self.maybe_prompt_plan_implementation();
        }
        // Keep this flag for replayed completion events so a subsequent live TurnComplete can
        // still show the prompt once after thread switch replay.
        if !from_replay {
            self.saw_plan_item_this_turn = false;
        }
        // If there is a queued user message, send exactly one now to begin the next turn.
        self.maybe_send_next_queued_input();
        // Emit a notification when the turn completes (suppressed if focused).
        self.notify(Notification::AgentTurnComplete {
            response: last_agent_message.unwrap_or_default(),
        });

        self.maybe_show_pending_rate_limit_prompt();
    }

    pub(super) fn maybe_prompt_plan_implementation(&mut self) {
        if !self.collaboration_modes_enabled() {
            return;
        }
        if self.has_queued_follow_up_messages() {
            return;
        }
        if self.active_mode_kind() != ModeKind::Plan {
            return;
        }
        if !self.saw_plan_item_this_turn {
            return;
        }
        if !self.bottom_pane.no_modal_or_popup_active() {
            return;
        }

        if matches!(
            self.rate_limit_switch_prompt,
            RateLimitSwitchPromptState::Pending
        ) {
            return;
        }

        self.open_plan_implementation_prompt();
    }

    pub(super) fn open_plan_implementation_prompt(&mut self) {
        let default_mask = collaboration_modes::default_mode_mask(self.models_manager.as_ref());
        let (implement_actions, implement_disabled_reason) = match default_mask {
            Some(mask) => {
                let user_text = PLAN_IMPLEMENTATION_CODING_MESSAGE.to_string();
                let actions: Vec<SelectionAction> = vec![Box::new(move |tx| {
                    tx.send(AppEvent::SubmitUserMessageWithMode {
                        text: user_text.clone(),
                        collaboration_mode: mask.clone(),
                    });
                })];
                (actions, None)
            }
            None => (Vec::new(), Some("Default mode unavailable".to_string())),
        };

        let items = vec![
            SelectionItem {
                name: PLAN_IMPLEMENTATION_YES.to_string(),
                description: Some("Switch to Default and start coding.".to_string()),
                selected_description: None,
                is_current: false,
                actions: implement_actions,
                disabled_reason: implement_disabled_reason,
                dismiss_on_select: true,
                ..Default::default()
            },
            SelectionItem {
                name: PLAN_IMPLEMENTATION_NO.to_string(),
                description: Some("Continue planning with the model.".to_string()),
                selected_description: None,
                is_current: false,
                actions: Vec::new(),
                dismiss_on_select: true,
                ..Default::default()
            },
        ];

        self.bottom_pane.show_selection_view(SelectionViewParams {
            title: Some(PLAN_IMPLEMENTATION_TITLE.to_string()),
            subtitle: None,
            footer_hint: Some(standard_popup_hint_line()),
            items,
            ..Default::default()
        });
    }

    /// Finalize any active exec as failed and stop/clear agent-turn UI state.
    ///
    /// This does not clear MCP startup tracking, because MCP startup can overlap with turn cleanup
    /// and should continue to drive the bottom-pane running indicator while it is in progress.
    pub(super) fn finalize_turn(&mut self) {
        // Ensure any spinner is replaced by a red ✗ and flushed into history.
        self.finalize_active_cell_as_failed();
        // Reset running state and clear streaming buffers.
        self.user_turn_pending_start = false;
        self.agent_turn_running = false;
        self.active_turn_id = None;
        self.running_turn_model = None;
        self.running_turn_reasoning_effort = None;
        self.update_task_running_state();
        self.bottom_pane.clear_progress_trace();
        self.running_commands.clear();
        self.suppressed_exec_calls.clear();
        self.last_unified_wait = None;
        self.unified_exec_wait_streak = None;
        self.clear_unified_exec_processes();
        self.adaptive_chunking.reset();
        self.stream_controller = None;
        self.plan_stream_controller = None;
        self.request_status_line_branch_refresh();
        self.refresh_status_line();
        self.maybe_show_pending_rate_limit_prompt();
    }

    pub(super) fn on_model_cap_error(&mut self, model: String, reset_after_seconds: Option<u64>) {
        self.with_queue_autosend_suppressed(|this| {
            this.finalize_turn();
            this.move_pending_steers_to_rejected_queue();

            let mut message =
                format!("Model {model} is at capacity. Please try a different model.");
            if let Some(seconds) = reset_after_seconds {
                message.push_str(&format!(
                    " Try again in {}.",
                    format_duration_short(seconds)
                ));
            } else {
                message.push_str(" Try again later.");
            }

            this.add_to_history(history_cell::new_warning_event(message));
            this.request_redraw();
        });
        self.maybe_send_next_queued_input();
    }

    pub(super) fn on_error(&mut self, message: String) {
        self.with_queue_autosend_suppressed(|this| {
            this.finalize_turn();
            this.move_pending_steers_to_rejected_queue();
            this.add_to_history(history_cell::new_error_event(message));
            this.request_redraw();
        });

        // After an error ends the turn, try sending the next queued input.
        self.maybe_send_next_queued_input();
    }

    pub(super) fn on_cyber_policy_error(&mut self) {
        self.with_queue_autosend_suppressed(|this| {
            this.finalize_turn();
            this.move_pending_steers_to_rejected_queue();
            this.add_to_history(history_cell::new_cyber_policy_error_event());
            this.request_redraw();
        });

        // After an error ends the turn, try sending the next queued input.
        self.maybe_send_next_queued_input();
    }

    pub(super) fn on_warning(&mut self, message: impl Into<String>) {
        self.add_to_history(history_cell::new_warning_event(message.into()));
        self.request_redraw();
    }

    pub(super) fn on_model_verification(&mut self, verifications: &[ModelVerification]) {
        if verifications.contains(&ModelVerification::TrustedAccessForCyber) {
            self.on_warning(TRUSTED_ACCESS_FOR_CYBER_VERIFICATION_WARNING);
        }
    }

    pub(super) fn on_paused_turn(&mut self) {
        self.with_queue_autosend_suppressed(|this| {
            this.finalize_turn();
            if this.is_review_mode {
                this.is_review_mode = false;
                this.restore_pre_review_token_info();
            }
            this.restore_pending_steers_to_composer_or_reject();
            this.add_info_message(
                "Conversation paused.".to_string(),
                Some("Use `/continue` to resume this turn.".to_string()),
            );
            this.request_redraw();
        });
    }

    pub(super) fn on_plan_update(&mut self, update: UpdatePlanArgs) {
        self.saw_plan_update_this_turn = true;
        self.add_to_history(history_cell::new_plan_update(update));
    }

    pub(super) fn worked_elapsed_from(&mut self, current_elapsed: u64) -> u64 {
        let baseline = match self.last_separator_elapsed_secs {
            Some(last) if current_elapsed < last => 0,
            Some(last) => last,
            None => 0,
        };
        let elapsed = current_elapsed.saturating_sub(baseline);
        self.last_separator_elapsed_secs = Some(current_elapsed);
        elapsed
    }

    pub(super) fn has_queued_follow_up_messages(&self) -> bool {
        !self.rejected_steers_queue.is_empty() || !self.queued_user_messages.is_empty()
    }
}
