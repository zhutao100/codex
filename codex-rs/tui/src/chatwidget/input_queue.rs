//! Queued user-message UI and editing for ChatWidget.

use super::*;

impl ChatWidget {
    pub(super) fn refresh_queued_user_messages(&mut self) {
        self.refresh_pending_input_preview();
    }

    pub(super) fn submit_queued_user_message(&mut self, queued: QueuedUserMessage) {
        let user_message = UserMessage {
            text: queued.text,
            local_images: queued.local_images,
            text_elements: queued.text_elements,
            mention_paths: queued.mention_paths,
        };
        self.submit_user_message_with_overrides(
            user_message,
            queued.model_override,
            queued.effort_override,
        );
    }

    pub(super) fn send_next_queued_user_message(&mut self) {
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
        self.refresh_pending_input_preview();
    }

    pub(super) fn handle_queue_edit_key_event(&mut self, key_event: KeyEvent) -> bool {
        match key_event {
            KeyEvent {
                code: KeyCode::Esc,
                kind: KeyEventKind::Press,
                ..
            } => {
                self.exit_queue_edit(false);
                true
            }
            KeyEvent {
                code: KeyCode::Enter,
                modifiers: KeyModifiers::NONE,
                kind: KeyEventKind::Press,
                ..
            } => {
                self.exit_queue_edit(true);
                true
            }
            KeyEvent {
                code: KeyCode::Up,
                modifiers: KeyModifiers::ALT,
                kind: KeyEventKind::Press,
                ..
            } => {
                self.switch_queue_edit(-1);
                true
            }
            KeyEvent {
                code: KeyCode::Down,
                modifiers: KeyModifiers::ALT,
                kind: KeyEventKind::Press,
                ..
            } => {
                self.switch_queue_edit(1);
                true
            }
            KeyEvent {
                code: KeyCode::Char('m' | 'M'),
                modifiers: KeyModifiers::ALT,
                kind: KeyEventKind::Press,
                ..
            } => {
                if let Some(id) = self
                    .queued_edit_state
                    .as_ref()
                    .map(|state| state.selected_id)
                {
                    self.open_queue_model_picker(id);
                }
                true
            }
            KeyEvent {
                code: KeyCode::Char('t' | 'T'),
                modifiers: KeyModifiers::ALT,
                kind: KeyEventKind::Press,
                ..
            } => {
                if let Some(id) = self
                    .queued_edit_state
                    .as_ref()
                    .map(|state| state.selected_id)
                {
                    self.open_queue_thinking_picker(id);
                }
                true
            }
            _ => false,
        }
    }

    pub(super) fn queue_popup_items(&self) -> Vec<QueuePopupItem> {
        let session_model = self.current_model();
        let session_effort = self.effective_reasoning_effort();

        self.queued_user_messages
            .iter()
            .map(|message| {
                let mut preview = message
                    .text
                    .lines()
                    .next()
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                if preview.is_empty() && !message.local_images.is_empty() {
                    preview = "[image]".to_string();
                }

                let effective_model = message.model_override.as_deref().unwrap_or(session_model);
                let effective_effort = message.effort_override.unwrap_or(session_effort);
                let mut meta_parts: Vec<String> = Vec::new();
                if !message.local_images.is_empty() {
                    meta_parts.push("img".to_string());
                }
                meta_parts.push(format!("model: {effective_model}"));
                meta_parts.push(format!(
                    "thinking: {}",
                    Self::status_line_reasoning_effort_label(effective_effort)
                ));

                QueuePopupItem {
                    id: message.id,
                    preview,
                    meta: Some(meta_parts.join(" · ")),
                }
            })
            .collect()
    }

    pub(super) fn open_queue_popup(&mut self) {
        if !self.bottom_pane.no_modal_or_popup_active()
            || self.queued_user_messages.is_empty()
            || self.queued_edit_state.is_some()
        {
            return;
        }
        let items = self.queue_popup_items();
        self.bottom_pane
            .show_view(Box::new(QueuePopup::new(items, self.app_event_tx.clone())));
        self.request_redraw();
    }

    pub(super) fn begin_queue_edit_most_recent(&mut self) {
        let Some(selected_id) = self.queued_user_messages.back().map(|message| message.id) else {
            return;
        };
        self.start_queue_edit(selected_id);
    }

    pub(crate) fn start_queue_edit(&mut self, id: u64) {
        if self.queued_edit_state.is_some()
            || self.queued_user_messages.is_empty()
            || !self.bottom_pane.no_modal_or_popup_active()
        {
            return;
        }

        if !self
            .queued_user_messages
            .iter()
            .any(|message| message.id == id)
        {
            return;
        }

        let composer_before_edit = QueuedComposerSnapshot {
            text: self.bottom_pane.composer_text(),
            text_elements: self.bottom_pane.composer_text_elements(),
            local_images: self.bottom_pane.composer_local_images(),
            mention_paths: self.bottom_pane.composer_mention_paths(),
        };

        self.queued_edit_state = Some(QueuedEditState {
            selected_id: id,
            composer_before_edit,
            drafts: HashMap::new(),
        });

        self.load_queue_edit_draft(id);
        self.update_queue_edit_footer_hint();
        self.refresh_queued_user_messages();
        self.request_redraw();
    }

    pub(crate) fn delete_queued_user_message(&mut self, id: u64) {
        let Some(idx) = self
            .queued_user_messages
            .iter()
            .position(|message| message.id == id)
        else {
            return;
        };

        self.queued_user_messages.remove(idx);
        self.refresh_queued_user_messages();
        self.request_redraw();
    }

    pub(crate) fn move_queued_user_message_up(&mut self, id: u64) {
        let Some(idx) = self
            .queued_user_messages
            .iter()
            .position(|message| message.id == id)
        else {
            return;
        };
        if idx == 0 {
            return;
        }

        self.queued_user_messages.swap(idx, idx - 1);
        self.refresh_queued_user_messages();
        self.request_redraw();
    }

    pub(crate) fn move_queued_user_message_down(&mut self, id: u64) {
        let len = self.queued_user_messages.len();
        let Some(idx) = self
            .queued_user_messages
            .iter()
            .position(|message| message.id == id)
        else {
            return;
        };
        if idx + 1 >= len {
            return;
        }

        self.queued_user_messages.swap(idx, idx + 1);
        self.refresh_queued_user_messages();
        self.request_redraw();
    }

    pub(crate) fn move_queued_user_message_to_front(&mut self, id: u64) {
        let Some(idx) = self
            .queued_user_messages
            .iter()
            .position(|message| message.id == id)
        else {
            return;
        };
        if idx == 0 {
            return;
        }

        let Some(message) = self.queued_user_messages.remove(idx) else {
            return;
        };
        self.queued_user_messages.push_front(message);
        self.refresh_queued_user_messages();
        self.request_redraw();
    }

    pub(crate) fn open_queue_model_picker(&mut self, id: u64) {
        let current_override = self
            .queued_edit_state
            .as_ref()
            .and_then(|state| state.drafts.get(&id))
            .map(|draft| draft.model_override.clone())
            .unwrap_or_else(|| {
                self.queued_user_messages
                    .iter()
                    .find(|message| message.id == id)
                    .and_then(|message| message.model_override.clone())
            });

        let session_model = self.current_model().to_string();
        let mut items: Vec<SelectionItem> = vec![SelectionItem {
            name: "Use session model".to_string(),
            description: Some(format!("Current session model: {session_model}")),
            is_current: current_override.is_none(),
            actions: vec![Box::new(move |tx| {
                tx.send(AppEvent::QueueSetModelOverride { id, model: None });
            })],
            dismiss_on_select: true,
            ..Default::default()
        }];

        let mut presets = match self.models_manager.try_list_models(&self.config) {
            Ok(presets) => presets,
            Err(_) => {
                self.add_info_message(
                    "Models are being updated; please try again in a moment.".to_string(),
                    None,
                );
                return;
            }
        };
        presets.sort_by(|a, b| a.display_name.cmp(&b.display_name));

        for preset in presets {
            let model_slug = preset.model.clone();
            let is_current = current_override.as_deref() == Some(model_slug.as_str());
            let description = (!preset.description.is_empty()).then_some(preset.description);
            let model_for_action = model_slug.clone();
            items.push(SelectionItem {
                name: preset.display_name,
                description,
                is_current,
                actions: vec![Box::new(move |tx| {
                    tx.send(AppEvent::QueueSetModelOverride {
                        id,
                        model: Some(model_for_action.clone()),
                    });
                })],
                dismiss_on_select: true,
                ..Default::default()
            });
        }

        self.bottom_pane.show_selection_view(SelectionViewParams {
            title: Some("Select Model for Queued Message".to_string()),
            subtitle: Some("Applies only when this message is sent.".to_string()),
            footer_hint: Some(standard_popup_hint_line()),
            items,
            ..Default::default()
        });
    }

    pub(crate) fn open_queue_thinking_picker(&mut self, id: u64) {
        let model_override = self
            .queued_edit_state
            .as_ref()
            .and_then(|state| state.drafts.get(&id))
            .map(|draft| draft.model_override.clone())
            .unwrap_or_else(|| {
                self.queued_user_messages
                    .iter()
                    .find(|message| message.id == id)
                    .and_then(|message| message.model_override.clone())
            });
        let model_slug = model_override.unwrap_or_else(|| self.current_model().to_string());

        let current_override = self
            .queued_edit_state
            .as_ref()
            .and_then(|state| state.drafts.get(&id))
            .map(|draft| draft.effort_override)
            .unwrap_or_else(|| {
                self.queued_user_messages
                    .iter()
                    .find(|message| message.id == id)
                    .and_then(|message| message.effort_override)
            });

        let presets = match self.models_manager.try_list_models(&self.config) {
            Ok(presets) => presets,
            Err(_) => {
                self.add_info_message(
                    "Models are being updated; please try again in a moment.".to_string(),
                    None,
                );
                return;
            }
        };
        let Some(preset) = presets
            .into_iter()
            .find(|preset| preset.model == model_slug)
        else {
            self.add_info_message(
                format!("Model '{model_slug}' is not available right now."),
                None,
            );
            return;
        };

        let default_effort = preset.default_reasoning_effort;
        let mut items: Vec<SelectionItem> = vec![
            SelectionItem {
                name: "Use session thinking".to_string(),
                description: Some("Inherit the current session reasoning level.".to_string()),
                is_current: current_override.is_none(),
                actions: vec![Box::new(move |tx| {
                    tx.send(AppEvent::QueueSetThinkingOverride { id, effort: None });
                })],
                dismiss_on_select: true,
                ..Default::default()
            },
            SelectionItem {
                name: format!("Default ({})", Self::reasoning_effort_label(default_effort)),
                description: Some("Use the model's default reasoning level.".to_string()),
                is_current: matches!(current_override, Some(None)),
                actions: vec![Box::new(move |tx| {
                    tx.send(AppEvent::QueueSetThinkingOverride {
                        id,
                        effort: Some(None),
                    });
                })],
                dismiss_on_select: true,
                ..Default::default()
            },
        ];

        for option in preset.supported_reasoning_efforts {
            let effort = option.effort;
            let mut label = Self::reasoning_effort_label(effort).to_string();
            if effort == default_effort {
                label.push_str(" (default)");
            }
            let description = (!option.description.is_empty()).then_some(option.description);
            let is_current = current_override == Some(Some(effort));
            items.push(SelectionItem {
                name: label,
                description,
                is_current,
                actions: vec![Box::new(move |tx| {
                    tx.send(AppEvent::QueueSetThinkingOverride {
                        id,
                        effort: Some(Some(effort)),
                    });
                })],
                dismiss_on_select: true,
                ..Default::default()
            });
        }

        self.bottom_pane.show_selection_view(SelectionViewParams {
            title: Some("Select Thinking for Queued Message".to_string()),
            subtitle: Some(format!("Model: {model_slug}")),
            footer_hint: Some(standard_popup_hint_line()),
            items,
            ..Default::default()
        });
    }

    pub(crate) fn set_queued_user_message_model_override(
        &mut self,
        id: u64,
        model: Option<String>,
    ) {
        if self.queued_edit_state.is_some() {
            let selected_id = self
                .queued_edit_state
                .as_ref()
                .map(|state| state.selected_id);
            let base = self
                .queued_edit_state
                .as_ref()
                .and_then(|state| state.drafts.get(&id).cloned())
                .or_else(|| {
                    if selected_id == Some(id) {
                        self.capture_queue_edit_draft(id)
                    } else {
                        self.queued_user_message_draft(id)
                    }
                });
            if let Some(mut draft) = base {
                draft.model_override = model;
                if let Some(state) = self.queued_edit_state.as_mut() {
                    state.drafts.insert(id, draft);
                }
            }
            self.refresh_queued_user_messages();
            self.request_redraw();
            return;
        }

        let Some(message) = self
            .queued_user_messages
            .iter_mut()
            .find(|message| message.id == id)
        else {
            return;
        };
        message.model_override = model;
        self.refresh_queued_user_messages();
        self.request_redraw();
    }

    pub(crate) fn set_queued_user_message_thinking_override(
        &mut self,
        id: u64,
        effort: Option<Option<ReasoningEffortConfig>>,
    ) {
        if self.queued_edit_state.is_some() {
            let selected_id = self
                .queued_edit_state
                .as_ref()
                .map(|state| state.selected_id);
            let base = self
                .queued_edit_state
                .as_ref()
                .and_then(|state| state.drafts.get(&id).cloned())
                .or_else(|| {
                    if selected_id == Some(id) {
                        self.capture_queue_edit_draft(id)
                    } else {
                        self.queued_user_message_draft(id)
                    }
                });
            if let Some(mut draft) = base {
                draft.effort_override = effort;
                if let Some(state) = self.queued_edit_state.as_mut() {
                    state.drafts.insert(id, draft);
                }
            }
            self.refresh_queued_user_messages();
            self.request_redraw();
            return;
        }

        let Some(message) = self
            .queued_user_messages
            .iter_mut()
            .find(|message| message.id == id)
        else {
            return;
        };
        message.effort_override = effort;
        self.refresh_queued_user_messages();
        self.request_redraw();
    }

    pub(super) fn exit_queue_edit(&mut self, save: bool) {
        let Some(mut state) = self.queued_edit_state.take() else {
            return;
        };

        if save {
            if let Some(draft) = self.capture_queue_edit_draft(state.selected_id) {
                state.drafts.insert(state.selected_id, draft);
            }

            for message in &mut self.queued_user_messages {
                if let Some(draft) = state.drafts.get(&message.id) {
                    message.text = draft.text.clone();
                    message.text_elements = draft.text_elements.clone();
                    message.local_images = draft.local_images.clone();
                    message.mention_paths = draft.mention_paths.clone();
                    message.model_override = draft.model_override.clone();
                    message.effort_override = draft.effort_override;
                }
            }
        }

        let local_image_paths: Vec<PathBuf> = state
            .composer_before_edit
            .local_images
            .iter()
            .map(|img| img.path.clone())
            .collect();
        self.bottom_pane.set_footer_hint_override(None);
        self.bottom_pane.set_composer_text_with_mention_paths(
            state.composer_before_edit.text,
            state.composer_before_edit.text_elements,
            local_image_paths,
            state.composer_before_edit.mention_paths,
        );

        self.refresh_queued_user_messages();
        self.request_redraw();
    }

    pub(super) fn switch_queue_edit(&mut self, direction: isize) {
        let Some(selected_id) = self
            .queued_edit_state
            .as_ref()
            .map(|state| state.selected_id)
        else {
            return;
        };

        if let Some(draft) = self.capture_queue_edit_draft(selected_id)
            && let Some(state) = self.queued_edit_state.as_mut()
        {
            state.drafts.insert(selected_id, draft);
        }

        let Some(current_idx) = self
            .queued_user_messages
            .iter()
            .position(|message| message.id == selected_id)
        else {
            self.exit_queue_edit(false);
            return;
        };

        let len = self.queued_user_messages.len();
        if len == 0 {
            self.exit_queue_edit(false);
            return;
        }

        let next_idx = ((current_idx as isize + direction).rem_euclid(len as isize)) as usize;
        let Some(next_id) = self
            .queued_user_messages
            .get(next_idx)
            .map(|message| message.id)
        else {
            return;
        };

        let draft = self
            .queued_edit_state
            .as_ref()
            .and_then(|state| state.drafts.get(&next_id).cloned())
            .or_else(|| self.queued_user_message_draft(next_id));
        let Some(draft) = draft else {
            return;
        };

        if let Some(state) = self.queued_edit_state.as_mut() {
            state.selected_id = next_id;
        }

        let local_image_paths: Vec<PathBuf> = draft
            .local_images
            .iter()
            .map(|img| img.path.clone())
            .collect();
        self.bottom_pane.set_composer_text_with_mention_paths(
            draft.text,
            draft.text_elements,
            local_image_paths,
            draft.mention_paths,
        );
        self.update_queue_edit_footer_hint();
        self.refresh_queued_user_messages();
        self.request_redraw();
    }

    pub(super) fn capture_queue_edit_draft(&self, id: u64) -> Option<QueuedUserMessageDraft> {
        let message = self
            .queued_user_messages
            .iter()
            .find(|message| message.id == id)?;

        let (model_override, effort_override) = self
            .queued_edit_state
            .as_ref()
            .and_then(|state| state.drafts.get(&id))
            .map(|draft| (draft.model_override.clone(), draft.effort_override))
            .unwrap_or_else(|| (message.model_override.clone(), message.effort_override));

        Some(QueuedUserMessageDraft {
            text: self.bottom_pane.composer_text(),
            text_elements: self.bottom_pane.composer_text_elements(),
            local_images: self.bottom_pane.composer_local_images(),
            mention_paths: self.bottom_pane.composer_mention_paths(),
            model_override,
            effort_override,
        })
    }

    pub(super) fn queued_user_message_draft(&self, id: u64) -> Option<QueuedUserMessageDraft> {
        let message = self
            .queued_user_messages
            .iter()
            .find(|message| message.id == id)?;

        Some(QueuedUserMessageDraft {
            text: message.text.clone(),
            text_elements: message.text_elements.clone(),
            local_images: message.local_images.clone(),
            mention_paths: message.mention_paths.clone(),
            model_override: message.model_override.clone(),
            effort_override: message.effort_override,
        })
    }

    pub(super) fn load_queue_edit_draft(&mut self, id: u64) {
        let draft = self
            .queued_edit_state
            .as_ref()
            .and_then(|state| state.drafts.get(&id).cloned())
            .or_else(|| self.queued_user_message_draft(id));

        let Some(draft) = draft else {
            return;
        };

        let local_image_paths: Vec<PathBuf> = draft
            .local_images
            .iter()
            .map(|img| img.path.clone())
            .collect();
        self.bottom_pane.set_composer_text_with_mention_paths(
            draft.text,
            draft.text_elements,
            local_image_paths,
            draft.mention_paths,
        );
    }

    pub(super) fn update_queue_edit_footer_hint(&mut self) {
        let Some(state) = self.queued_edit_state.as_ref() else {
            return;
        };

        let total = self.queued_user_messages.len();
        let position = self
            .queued_user_messages
            .iter()
            .position(|message| message.id == state.selected_id)
            .map(|idx| idx + 1)
            .unwrap_or_default();

        self.bottom_pane.set_footer_hint_override(Some(vec![
            ("Editing".to_string(), format!("{position}/{total}")),
            ("Enter".to_string(), "save".to_string()),
            ("Esc".to_string(), "cancel".to_string()),
            ("Alt+↑/↓".to_string(), "switch".to_string()),
            ("Alt+M".to_string(), "model".to_string()),
            ("Alt+T".to_string(), "thinking".to_string()),
        ]));
    }
}
