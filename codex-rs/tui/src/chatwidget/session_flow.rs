//! Session lifecycle handlers for ChatWidget.

use super::*;

impl ChatWidget {
    pub(super) fn on_session_configured(
        &mut self,
        event: codex_core::protocol::SessionConfiguredEvent,
    ) {
        self.bottom_pane
            .set_history_metadata(event.history_log_id, event.history_entry_count);
        self.set_skills(None);
        self.bottom_pane.set_connectors_snapshot(None);
        self.thread_id = Some(event.session_id);
        self.thread_name = event.thread_name.clone();
        self.forked_from = event.forked_from_id;
        self.current_rollout_path = event.rollout_path.clone();
        self.current_cwd = Some(event.cwd.clone());
        self.copyable_messages.clear();
        self.active_runtime_context = None;
        self.running_turn_model = None;
        self.running_turn_reasoning_effort = None;
        let initial_messages = event.initial_messages.clone();
        let forked_from_id = event.forked_from_id;
        self.last_copyable_output = None;
        let model_for_header = event.model.clone();
        self.session_header.set_model(&model_for_header);
        self.current_collaboration_mode = self.current_collaboration_mode.with_updates(
            Some(model_for_header.clone()),
            Some(event.reasoning_effort),
            None,
        );
        self.refresh_model_display();
        self.sync_personality_command_enabled();
        let session_info_cell = history_cell::new_session_info(
            &self.config,
            &model_for_header,
            event,
            self.show_welcome_banner,
            self.auth_manager
                .auth_cached()
                .and_then(|auth| auth.account_plan_type()),
        );
        self.apply_session_info_cell(session_info_cell);

        if let Some(messages) = initial_messages {
            self.replay_initial_messages(messages);
        }
        // Ask codex-core to enumerate custom prompts for this session.
        self.submit_op(Op::ListCustomPrompts);
        self.submit_op(Op::ListSkills {
            cwds: Vec::new(),
            force_reload: true,
        });
        if self.connectors_enabled() {
            self.prefetch_connectors();
        }
        if let Some(user_message) = self.initial_user_message.take() {
            self.submit_user_message(user_message);
        }
        if let Some(forked_from_id) = forked_from_id {
            self.emit_forked_thread_event(forked_from_id);
        }
        if !self.suppress_session_configured_redraw {
            self.request_redraw();
        }
    }

    pub(super) fn emit_forked_thread_event(&self, forked_from_id: ThreadId) {
        let app_event_tx = self.app_event_tx.clone();
        let codex_home = self.config.codex_home.clone();
        tokio::spawn(async move {
            let forked_from_id_text = forked_from_id.to_string();
            let send_name_and_id = |name: String| {
                let line: Line<'static> = vec![
                    "• ".dim(),
                    "Thread forked from ".into(),
                    name.cyan(),
                    " (".into(),
                    forked_from_id_text.clone().cyan(),
                    ")".into(),
                ]
                .into();
                app_event_tx.send(AppEvent::InsertHistoryCell(Box::new(
                    PlainHistoryCell::new(vec![line]),
                )));
            };
            let send_id_only = || {
                let line: Line<'static> = vec![
                    "• ".dim(),
                    "Thread forked from ".into(),
                    forked_from_id_text.clone().cyan(),
                ]
                .into();
                app_event_tx.send(AppEvent::InsertHistoryCell(Box::new(
                    PlainHistoryCell::new(vec![line]),
                )));
            };

            match find_thread_name_by_id(&codex_home, &forked_from_id).await {
                Ok(Some(name)) if !name.trim().is_empty() => {
                    send_name_and_id(name);
                }
                Ok(_) => send_id_only(),
                Err(err) => {
                    tracing::warn!("Failed to read forked thread name: {err}");
                    send_id_only();
                }
            }
        });
    }

    pub(super) fn on_thread_name_updated(
        &mut self,
        event: codex_core::protocol::ThreadNameUpdatedEvent,
    ) {
        if self.thread_id == Some(event.thread_id) {
            self.thread_name = event.thread_name.clone();
            let message = match event.thread_name.as_deref() {
                Some(thread_name) => format!("Renamed thread name to \"{thread_name}\"."),
                None => "Cleared thread name.".to_string(),
            };
            self.add_info_message(message, None);
            self.request_redraw();
        }
    }

    pub(super) fn on_runtime_context_activated(&mut self, snapshot: RuntimeContextSnapshot) {
        self.active_runtime_context = Some(snapshot);
        self.refresh_status_subject();
    }

    pub(super) fn on_runtime_context_updated(&mut self, snapshot: RuntimeContextSnapshot) {
        self.active_runtime_context = Some(snapshot);
        self.refresh_status_subject();
    }

    pub(super) fn on_runtime_context_deactivated(&mut self, event: RuntimeContextDeactivatedEvent) {
        if self
            .active_runtime_context
            .as_ref()
            .is_some_and(|snapshot| snapshot.scope_id == event.scope_id)
        {
            self.active_runtime_context = None;
            self.refresh_status_subject();
        }
    }

    pub(super) fn set_skills(&mut self, skills: Option<Vec<SkillMetadata>>) {
        self.bottom_pane.set_skills(skills);
    }
}
