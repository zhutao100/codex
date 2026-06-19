//! User-message submission behavior for ChatWidget.

use super::*;

impl ChatWidget {
    fn next_pending_steer_client_user_message_id(&mut self) -> String {
        let id = self.next_pending_steer_client_id;
        self.next_pending_steer_client_id = self.next_pending_steer_client_id.saturating_add(1);
        format!("tui-steer-{id}")
    }

    fn submit_shell_command(&mut self, command: &str) -> QueueDrain {
        let cmd = command.trim();
        if cmd.is_empty() {
            self.app_event_tx.send(AppEvent::InsertHistoryCell(Box::new(
                history_cell::new_info_event(
                    USER_SHELL_COMMAND_HELP_TITLE.to_string(),
                    Some(USER_SHELL_COMMAND_HELP_HINT.to_string()),
                ),
            )));
            QueueDrain::Continue
        } else {
            self.submit_op(Op::RunUserShellCommand {
                command: cmd.to_string(),
            });
            QueueDrain::Stop
        }
    }

    pub(super) fn submit_queued_shell_prompt(&mut self, queued: QueuedUserMessage) -> QueueDrain {
        match queued.text.strip_prefix('!') {
            Some(command) => self.submit_shell_command(command),
            None => {
                self.submit_queued_user_message(queued);
                QueueDrain::Stop
            }
        }
    }

    pub(super) fn submit_user_message(&mut self, user_message: UserMessage) {
        self.submit_user_message_with_overrides(user_message, None, None);
    }

    pub(super) fn submit_user_message_with_overrides(
        &mut self,
        user_message: UserMessage,
        model_override: Option<String>,
        effort_override: Option<Option<ReasoningEffortConfig>>,
    ) {
        if !self.is_session_configured() {
            tracing::warn!("cannot submit user message before session is configured; queueing");
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
                action: QueuedInputAction::Plain,
                pending_pastes: Vec::new(),
            };
            self.queued_user_messages.push_front(queued);
            self.refresh_queued_user_messages();
            return;
        }

        let UserMessage {
            text,
            local_images,
            text_elements,
            mention_paths,
        } = user_message;
        if text.is_empty() && local_images.is_empty() {
            return;
        }
        if !local_images.is_empty() && !self.current_model_supports_images() {
            self.restore_blocked_image_submission(text, text_elements, local_images, mention_paths);
            return;
        }

        let mut items: Vec<UserInput> = Vec::new();

        // Special-case: "!cmd" executes a local shell command instead of sending to the model.
        if let Some(stripped) = text.strip_prefix('!') {
            self.submit_shell_command(stripped);
            return;
        }

        for image in &local_images {
            items.push(UserInput::LocalImage {
                path: image.path.clone(),
            });
        }

        if !text.is_empty() {
            items.push(UserInput::Text {
                text: text.clone(),
                text_elements: text_elements.clone(),
            });
        }

        let mentions = collect_tool_mentions(&text, &mention_paths);
        let mut skill_names_lower: HashSet<String> = HashSet::new();

        if let Some(skills) = self.bottom_pane.skills() {
            skill_names_lower = skills
                .iter()
                .map(|skill| skill.name.to_ascii_lowercase())
                .collect();
            let skill_mentions = find_skill_mentions_with_tool_mentions(&mentions, skills);
            for skill in skill_mentions {
                items.push(UserInput::Skill {
                    name: skill.name.clone(),
                    path: skill.path.clone(),
                });
            }
        }

        if let Some(apps) = self.connectors_for_mentions() {
            let app_mentions = find_app_mentions(&mentions, apps, &skill_names_lower);
            for app in app_mentions {
                let app_id = app.id.as_str();
                items.push(UserInput::Mention {
                    name: app.name.clone(),
                    path: format!("app://{app_id}"),
                });
            }
        }

        let effective_mode = self.effective_collaboration_mode();
        let running_model = model_override
            .clone()
            .or_else(|| {
                self.agent_turn_running
                    .then(|| self.running_turn_model.clone())
                    .flatten()
            })
            .unwrap_or_else(|| effective_mode.model().to_string());
        let running_effort = effort_override.unwrap_or_else(|| {
            if self.agent_turn_running {
                self.running_turn_reasoning_effort
            } else {
                effective_mode.reasoning_effort()
            }
        });
        let collaboration_mode = if self.collaboration_modes_enabled() {
            self.active_collaboration_mask
                .as_ref()
                .map(|_| effective_mode.clone())
        } else {
            None
        };
        let personality = self
            .config
            .personality
            .filter(|_| self.config.features.enabled(Feature::Personality))
            .filter(|_| self.current_model_supports_personality());
        let render_in_history = !self.agent_turn_running;
        let history_record = UserMessageHistoryRecord::UserMessageText;
        let submitted_user_message = UserMessage {
            text: text.clone(),
            local_images,
            text_elements,
            mention_paths,
        };
        let pending_steer_compare_key =
            (!render_in_history).then(|| Self::pending_steer_compare_key_from_inputs(&items));
        let (op, pending_steer) = if render_in_history {
            (
                Op::UserTurn {
                    items,
                    cwd: self.config.cwd.clone(),
                    approval_policy: self.config.approval_policy.value(),
                    sandbox_policy: self.config.sandbox_policy.get().clone(),
                    model: running_model.clone(),
                    effort: running_effort,
                    summary: self.config.model_reasoning_summary,
                    final_output_json_schema: None,
                    collaboration_mode,
                    personality,
                    service_tier: self.config.service_tier.clone(),
                },
                None,
            )
        } else if let Some(expected_turn_id) = self.active_turn_id.clone() {
            let client_user_message_id = self.next_pending_steer_client_user_message_id();
            (
                Op::SteerInput {
                    expected_turn_id: expected_turn_id.clone(),
                    items,
                    client_user_message_id: Some(client_user_message_id.clone()),
                },
                Some(PendingSteer {
                    target_turn_id: expected_turn_id,
                    client_user_message_id: Some(client_user_message_id),
                    user_message: submitted_user_message.clone(),
                    history_record: history_record.clone(),
                    compare_key: pending_steer_compare_key
                        .expect("active steer should have a compare key"),
                }),
            )
        } else {
            self.queue_user_message_with_overrides(
                submitted_user_message,
                model_override,
                effort_override,
            );
            return;
        };

        if let Err(e) = self.codex_op_tx.send(op) {
            tracing::error!("failed to send message: {e}");
            return;
        }

        if render_in_history {
            self.running_turn_model = Some(running_model);
            self.running_turn_reasoning_effort = running_effort;
            self.user_turn_pending_start = true;
        }

        // Persist the text to cross-session message history.
        if !text.is_empty() {
            self.codex_op_tx
                .send(Op::AddToHistory { text })
                .unwrap_or_else(|e| {
                    tracing::error!("failed to send AddHistory op: {e}");
                });
        }

        if let Some(pending_steer) = pending_steer {
            self.pending_steers.push_back(pending_steer);
            self.refresh_pending_input_preview();
        } else {
            let display = UserMessageDisplay::from_user_message(user_message_for_history(
                submitted_user_message,
                &history_record,
            ));
            self.on_user_message_display(display);
        }

        self.needs_final_message_separator = false;
        self.refresh_status_line();
    }

    /// Restore the blocked submission draft without losing mention resolution state.
    ///
    /// The blocked-image path intentionally keeps the draft in the composer so
    /// users can remove attachments and retry. We must restore
    /// `mention_paths` alongside visible text; restoring only `$name` tokens
    /// makes the draft look correct while degrading mention resolution to
    /// name-only heuristics on retry.
    pub(super) fn restore_blocked_image_submission(
        &mut self,
        text: String,
        text_elements: Vec<TextElement>,
        local_images: Vec<LocalImageAttachment>,
        mention_paths: HashMap<String, String>,
    ) {
        // Preserve the user's composed payload so they can retry after changing models.
        let local_image_paths = local_images.iter().map(|img| img.path.clone()).collect();
        self.bottom_pane.set_composer_text_with_mention_paths(
            text,
            text_elements,
            local_image_paths,
            mention_paths,
        );
        self.add_to_history(history_cell::new_warning_event(
            self.image_inputs_not_supported_message(),
        ));
        self.request_redraw();
    }
}
