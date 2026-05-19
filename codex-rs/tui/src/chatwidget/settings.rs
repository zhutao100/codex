//! Runtime settings mutations for ChatWidget.

use super::*;

impl ChatWidget {
    #[cfg(target_os = "windows")]
    pub(crate) fn clear_forced_auto_mode_downgrade(&mut self) {
        self.config.forced_auto_mode_downgraded_on_windows = false;
    }

    #[cfg(not(target_os = "windows"))]
    #[allow(dead_code)]
    pub(crate) fn clear_forced_auto_mode_downgrade(&mut self) {}

    /// Set the approval policy in the widget's config copy.
    pub(crate) fn set_approval_policy(&mut self, policy: AskForApproval) {
        if let Err(err) = self.config.approval_policy.set(policy) {
            tracing::warn!(%err, "failed to set approval_policy on chat config");
        }
    }

    /// Set the sandbox policy in the widget's config copy.
    pub(crate) fn set_sandbox_policy(&mut self, policy: SandboxPolicy) -> ConstraintResult<()> {
        #[cfg(target_os = "windows")]
        let should_clear_downgrade = !matches!(&policy, SandboxPolicy::ReadOnly { .. })
            || WindowsSandboxLevel::from_config(&self.config) != WindowsSandboxLevel::Disabled;

        self.config.sandbox_policy.set(policy)?;

        #[cfg(target_os = "windows")]
        if should_clear_downgrade {
            self.config.forced_auto_mode_downgraded_on_windows = false;
        }

        Ok(())
    }

    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    pub(crate) fn set_feature_enabled(&mut self, feature: Feature, enabled: bool) {
        if enabled {
            self.config.features.enable(feature);
        } else {
            self.config.features.disable(feature);
        }
        if feature == Feature::Steer {
            self.bottom_pane.set_steer_enabled(enabled);
        }
        if feature == Feature::CollaborationModes {
            self.bottom_pane.set_collaboration_modes_enabled(enabled);
            let settings = self.current_collaboration_mode.settings.clone();
            self.current_collaboration_mode = CollaborationMode {
                mode: ModeKind::Default,
                settings,
            };
            self.active_collaboration_mask = if enabled {
                collaboration_modes::default_mask(self.models_manager.as_ref())
            } else {
                None
            };
            self.update_collaboration_mode_indicator();
            self.refresh_model_display();
            self.request_redraw();
        }
        if feature == Feature::Personality {
            self.sync_personality_command_enabled();
        }
        #[cfg(target_os = "windows")]
        if matches!(
            feature,
            Feature::WindowsSandbox | Feature::WindowsSandboxElevated
        ) {
            self.bottom_pane.set_windows_degraded_sandbox_active(
                codex_core::windows_sandbox::ELEVATED_SANDBOX_NUX_ENABLED
                    && matches!(
                        WindowsSandboxLevel::from_config(&self.config),
                        WindowsSandboxLevel::RestrictedToken
                    ),
            );
        }
    }

    pub(crate) fn set_full_access_warning_acknowledged(&mut self, acknowledged: bool) {
        self.config.notices.hide_full_access_warning = Some(acknowledged);
    }

    pub(crate) fn set_world_writable_warning_acknowledged(&mut self, acknowledged: bool) {
        self.config.notices.hide_world_writable_warning = Some(acknowledged);
    }

    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    pub(crate) fn world_writable_warning_hidden(&self) -> bool {
        self.config
            .notices
            .hide_world_writable_warning
            .unwrap_or(false)
    }

    /// Set the reasoning effort in the stored collaboration mode.
    pub(crate) fn set_reasoning_effort(&mut self, effort: Option<ReasoningEffortConfig>) {
        self.current_collaboration_mode =
            self.current_collaboration_mode
                .with_updates(None, Some(effort), None);
        if self.collaboration_modes_enabled()
            && let Some(mask) = self.active_collaboration_mask.as_mut()
        {
            mask.reasoning_effort = Some(effort);
        }
    }

    /// Set the personality in the widget's config copy.
    pub(crate) fn set_personality(&mut self, personality: Personality) {
        self.config.personality = Some(personality);
    }

    /// Set the model in the widget's config copy and stored collaboration mode.
    pub(crate) fn set_model(&mut self, model: &str) {
        self.current_collaboration_mode =
            self.current_collaboration_mode
                .with_updates(Some(model.to_string()), None, None);
        if self.collaboration_modes_enabled()
            && let Some(mask) = self.active_collaboration_mask.as_mut()
        {
            mask.model = Some(model.to_string());
        }
        self.refresh_model_display();
    }

    pub(crate) fn current_model(&self) -> &str {
        if !self.collaboration_modes_enabled() {
            return self.current_collaboration_mode.model();
        }
        self.active_collaboration_mask
            .as_ref()
            .and_then(|mask| mask.model.as_deref())
            .unwrap_or_else(|| self.current_collaboration_mode.model())
    }

    pub(super) fn sync_personality_command_enabled(&mut self) {
        self.bottom_pane
            .set_personality_command_enabled(self.config.features.enabled(Feature::Personality));
    }

    pub(super) fn current_model_supports_personality(&self) -> bool {
        let model = self.current_model();
        self.models_manager
            .try_list_models(&self.config)
            .ok()
            .and_then(|models| {
                models
                    .into_iter()
                    .find(|preset| preset.model == model)
                    .map(|preset| preset.supports_personality)
            })
            .unwrap_or(false)
    }

    /// Return whether the effective model currently advertises image-input support.
    ///
    /// We intentionally default to `true` when model metadata cannot be read so transient catalog
    /// failures do not hard-block user input in the UI.
    pub(super) fn current_model_supports_images(&self) -> bool {
        let model = self.current_model();
        self.models_manager
            .try_list_models(&self.config)
            .ok()
            .and_then(|models| {
                models
                    .into_iter()
                    .find(|preset| preset.model == model)
                    .map(|preset| preset.input_modalities.contains(&InputModality::Image))
            })
            .unwrap_or(true)
    }

    pub(super) fn sync_image_paste_enabled(&mut self) {
        let enabled = self.current_model_supports_images();
        self.bottom_pane.set_image_paste_enabled(enabled);
    }

    pub(super) fn image_inputs_not_supported_message(&self) -> String {
        format!(
            "Model {} does not support image inputs. Remove images or switch models.",
            self.current_model()
        )
    }

    #[allow(dead_code)] // Used in tests
    pub(crate) fn current_collaboration_mode(&self) -> &CollaborationMode {
        &self.current_collaboration_mode
    }

    #[cfg(test)]
    pub(crate) fn current_reasoning_effort(&self) -> Option<ReasoningEffortConfig> {
        self.effective_reasoning_effort()
    }

    #[cfg(test)]
    pub(crate) fn active_collaboration_mode_kind(&self) -> ModeKind {
        self.active_mode_kind()
    }

    pub(super) fn is_session_configured(&self) -> bool {
        self.thread_id.is_some()
    }

    pub(super) fn collaboration_modes_enabled(&self) -> bool {
        self.config.features.enabled(Feature::CollaborationModes)
    }

    pub(super) fn initial_collaboration_mask(
        config: &Config,
        models_manager: &ModelsManager,
        model_override: Option<&str>,
    ) -> Option<CollaborationModeMask> {
        if !config.features.enabled(Feature::CollaborationModes) {
            return None;
        }
        let mut mask = match config.experimental_mode {
            Some(kind) => collaboration_modes::mask_for_kind(models_manager, kind)?,
            None => collaboration_modes::default_mask(models_manager)?,
        };
        if let Some(model_override) = model_override {
            mask.model = Some(model_override.to_string());
        }
        Some(mask)
    }

    pub(super) fn active_mode_kind(&self) -> ModeKind {
        self.active_collaboration_mask
            .as_ref()
            .and_then(|mask| mask.mode)
            .unwrap_or(ModeKind::Default)
    }

    pub(super) fn effective_reasoning_effort(&self) -> Option<ReasoningEffortConfig> {
        if !self.collaboration_modes_enabled() {
            return self.current_collaboration_mode.reasoning_effort();
        }
        let current_effort = self.current_collaboration_mode.reasoning_effort();
        self.active_collaboration_mask
            .as_ref()
            .and_then(|mask| mask.reasoning_effort)
            .unwrap_or(current_effort)
    }

    pub(super) fn effective_collaboration_mode(&self) -> CollaborationMode {
        if !self.collaboration_modes_enabled() {
            return self.current_collaboration_mode.clone();
        }
        self.active_collaboration_mask.as_ref().map_or_else(
            || self.current_collaboration_mode.clone(),
            |mask| self.current_collaboration_mode.apply_mask(mask),
        )
    }

    pub(super) fn refresh_model_display(&mut self) {
        let effective = self.effective_collaboration_mode();
        self.session_header.set_model(effective.model());
        self.bottom_pane
            .set_session_model(effective.model().to_string());
        self.bottom_pane
            .set_session_reasoning_effort(self.effective_reasoning_effort());
        // Keep composer paste affordances aligned with the currently effective model.
        self.sync_image_paste_enabled();
    }

    pub(super) fn model_display_name(&self) -> &str {
        let model = self.current_model();
        if model.is_empty() {
            DEFAULT_MODEL_DISPLAY_NAME
        } else {
            model
        }
    }

    /// Get the label for the current collaboration mode.
    pub(super) fn collaboration_mode_label(&self) -> Option<&'static str> {
        if !self.collaboration_modes_enabled() {
            return None;
        }
        let active_mode = self.active_mode_kind();
        active_mode
            .is_tui_visible()
            .then_some(active_mode.display_name())
    }

    pub(super) fn collaboration_mode_indicator(&self) -> Option<CollaborationModeIndicator> {
        if !self.collaboration_modes_enabled() {
            return None;
        }
        match self.active_mode_kind() {
            ModeKind::Plan => Some(CollaborationModeIndicator::Plan),
            ModeKind::Default | ModeKind::PairProgramming | ModeKind::Execute => None,
        }
    }

    pub(super) fn update_collaboration_mode_indicator(&mut self) {
        let indicator = self.collaboration_mode_indicator();
        self.bottom_pane.set_collaboration_mode_indicator(indicator);
    }

    /// Cycle to the next collaboration mode variant (Plan -> Default -> Plan).
    pub(super) fn cycle_collaboration_mode(&mut self) {
        if !self.collaboration_modes_enabled() {
            return;
        }

        if let Some(next_mask) = collaboration_modes::next_mask(
            self.models_manager.as_ref(),
            self.active_collaboration_mask.as_ref(),
        ) {
            self.set_collaboration_mask(next_mask);
        }
    }

    pub(super) fn cycle_model_shortcut(&mut self, direction: isize) {
        let current_model = self.current_model().to_string();
        let presets: Vec<ModelPreset> = match self.models_manager.try_list_models(&self.config) {
            Ok(models) => models,
            Err(_) => {
                self.add_info_message(
                    "Models are being updated; please try again in a moment.".to_string(),
                    None,
                );
                return;
            }
        };

        let choices = Self::model_shortcut_choices(&current_model, presets);

        if choices.len() <= 1 {
            return;
        }

        let next_idx = if let Some(current_idx) = choices
            .iter()
            .position(|preset| preset.model == current_model)
        {
            let len = choices.len() as isize;
            (current_idx as isize + direction).rem_euclid(len) as usize
        } else if direction >= 0 {
            0
        } else {
            choices.len() - 1
        };

        let next = choices[next_idx].clone();
        self.apply_model_and_effort(next.model.to_string(), Some(next.default_reasoning_effort));
    }

    pub(super) fn cycle_reasoning_effort_shortcut(&mut self, direction: isize) {
        let model_slug = self.current_model().to_string();
        let current_effort = self.effective_reasoning_effort();

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

        let Some(preset) = Self::picker_visible_model_presets(presets)
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
        let mut supported: HashSet<ReasoningEffortConfig> = preset
            .supported_reasoning_efforts
            .into_iter()
            .map(|option| option.effort)
            .collect();
        supported.insert(default_effort);

        let choices: Vec<ReasoningEffortConfig> = ReasoningEffortConfig::iter()
            .filter(|effort| *effort != ReasoningEffortConfig::None && supported.contains(effort))
            .collect();

        if choices.len() <= 1 {
            return;
        }

        let current_idx = current_effort
            .and_then(|effort| choices.iter().position(|choice| *choice == effort))
            .or_else(|| choices.iter().position(|choice| *choice == default_effort))
            .unwrap_or(0);

        let len = choices.len() as isize;
        let next_idx = (current_idx as isize + direction).rem_euclid(len) as usize;
        let next_effort = Some(choices[next_idx]);
        self.apply_model_and_effort(model_slug, next_effort);
    }

    /// Update the active collaboration mask.
    ///
    /// When collaboration modes are enabled and a preset is selected,
    /// the current mode is attached to submissions as `Op::UserTurn { collaboration_mode: Some(...) }`.
    pub(crate) fn set_collaboration_mask(&mut self, mask: CollaborationModeMask) {
        if !self.collaboration_modes_enabled() {
            return;
        }
        self.active_collaboration_mask = Some(mask);
        self.update_collaboration_mode_indicator();
        self.refresh_model_display();
        self.request_redraw();
    }
}
