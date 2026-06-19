//! Construction and initial wiring for ChatWidget.

use super::*;

impl ChatWidget {
    pub(super) fn resolve_keybindings(
        config: &Config,
        enhanced_keys_supported: bool,
    ) -> Keybindings {
        #[cfg(target_os = "linux")]
        let is_wsl = crate::clipboard_paste::is_probably_wsl();
        #[cfg(not(target_os = "linux"))]
        let is_wsl = false;

        Keybindings::from_config(&config.keybindings, enhanced_keys_supported, is_wsl)
    }

    pub(crate) fn new(common: ChatWidgetInit, thread_manager: Arc<ThreadManager>) -> Self {
        let ChatWidgetInit {
            config,
            frame_requester,
            app_event_tx,
            initial_user_message,
            enhanced_keys_supported,
            auth_manager,
            models_manager,
            feedback,
            is_first_run,
            feedback_audience,
            model,
            status_line_invalid_items_warned,
            otel_manager,
        } = common;
        let model = model.filter(|m| !m.trim().is_empty());
        let mut config = config;
        config.model = model.clone();
        let keybindings = Self::resolve_keybindings(&config, enhanced_keys_supported);
        let progress_legend_mode = config.tui_progress_legend_mode;
        let (progress_trace_styles, progress_trace_style_warnings) =
            resolve_progress_trace_styles(config.tui_progress_trace_style.as_ref());
        crate::markdown_render::set_syntax_highlight_theme(
            config.tui_syntax_highlight_theme.clone(),
        );
        let mut rng = rand::rng();
        let placeholder = PLACEHOLDERS[rng.random_range(0..PLACEHOLDERS.len())].to_string();
        let codex_op_tx = spawn_agent(config.clone(), app_event_tx.clone(), thread_manager);

        let model_override = model.as_deref();
        let model_for_header = model
            .clone()
            .unwrap_or_else(|| DEFAULT_MODEL_DISPLAY_NAME.to_string());
        let active_collaboration_mask =
            Self::initial_collaboration_mask(&config, models_manager.as_ref(), model_override);
        let header_model = active_collaboration_mask
            .as_ref()
            .and_then(|mask| mask.model.clone())
            .unwrap_or_else(|| model_for_header.clone());
        let fallback_default = Settings {
            model: header_model.clone(),
            reasoning_effort: None,
            developer_instructions: None,
        };
        // Collaboration modes start in Default mode.
        let current_collaboration_mode = CollaborationMode {
            mode: ModeKind::Default,
            settings: fallback_default,
        };

        let active_cell = Some(Self::placeholder_session_header_cell(&config));

        let current_cwd = Some(config.cwd.clone());
        let mut widget = Self {
            app_event_tx: app_event_tx.clone(),
            frame_requester: frame_requester.clone(),
            codex_op_tx,
            bottom_pane: BottomPane::new(BottomPaneParams {
                frame_requester,
                app_event_tx,
                has_input_focus: true,
                enhanced_keys_supported,
                placeholder_text: placeholder,
                disable_paste_burst: config.disable_paste_burst,
                animations_enabled: config.animations,
                progress_legend_mode,
                progress_trace_styles,
                skills: None,
            }),
            active_cell,
            active_cell_revision: 0,
            config,
            keybindings,
            skills_all: Vec::new(),
            skills_initial_state: None,
            current_collaboration_mode,
            active_collaboration_mask,
            auth_manager,
            models_manager,
            otel_manager,
            session_header: SessionHeader::new(header_model),
            initial_user_message,
            token_info: None,
            rate_limit_snapshot: None,
            plan_type: None,
            rate_limit_warnings: RateLimitWarningState::default(),
            rate_limit_switch_prompt: RateLimitSwitchPromptState::default(),
            rate_limit_poller: None,
            adaptive_chunking: AdaptiveChunkingPolicy::default(),
            stream_controller: None,
            plan_stream_controller: None,
            last_copyable_output: None,
            running_commands: HashMap::new(),
            suppressed_exec_calls: HashSet::new(),
            last_unified_wait: None,
            unified_exec_wait_streak: None,
            task_complete_pending: false,
            unified_exec_processes: Vec::new(),
            agent_turn_running: false,
            mcp_startup_status: None,
            connectors_cache: ConnectorsCacheState::default(),
            interrupts: InterruptManager::new(),
            reasoning_buffer: String::new(),
            full_reasoning_buffer: String::new(),
            current_status_header: String::from("Working"),
            retry_status_header: None,
            active_runtime_context: None,
            running_turn_model: None,
            running_turn_reasoning_effort: None,
            thread_id: None,
            thread_name: None,
            has_completed_assistant_message: false,
            last_assistant_output_markdown: None,
            copyable_messages: Vec::new(),
            copy_code_ui_state: None,
            copy_message_ui_state: None,
            forked_from: None,
            queued_user_messages: VecDeque::new(),
            active_turn_id: None,
            pending_steers: VecDeque::new(),
            rejected_steers_queue: VecDeque::new(),
            rejected_steer_history_records: VecDeque::new(),
            user_turn_pending_start: false,
            last_rendered_user_message_display: None,
            next_queued_user_message_id: 1,
            next_pending_steer_client_id: 1,
            queued_edit_state: None,
            show_welcome_banner: is_first_run,
            suppress_session_configured_redraw: false,
            suppress_queue_autosend: false,
            pending_notification: None,
            quit_shortcut_expires_at: None,
            quit_shortcut_key: None,
            is_review_mode: false,
            pre_review_token_info: None,
            needs_final_message_separator: false,
            had_work_activity: false,
            turn_progress_trace: Vec::new(),
            pending_separator_progress_trace: None,
            progress_legend_mode,
            progress_trace_styles,
            saw_plan_update_this_turn: false,
            saw_plan_item_this_turn: false,
            plan_delta_buffer: String::new(),
            plan_item_active: false,
            last_separator_elapsed_secs: None,
            last_rendered_width: std::cell::Cell::new(None),
            feedback,
            feedback_audience,
            current_rollout_path: None,
            current_cwd,
            status_line_invalid_items_warned,
            status_line_branch: None,
            status_line_branch_cwd: None,
            status_line_branch_pending: false,
            status_line_branch_lookup_complete: false,
            external_editor_state: ExternalEditorState::Closed,
        };

        for warning in progress_trace_style_warnings {
            widget.add_to_history(history_cell::new_warning_event(warning));
        }

        widget
            .bottom_pane
            .set_keybindings(widget.keybindings.clone());
        widget.prefetch_rate_limits();
        widget
            .bottom_pane
            .set_steer_enabled(widget.config.features.enabled(Feature::Steer));
        let status_line_enabled = !widget.status_line_items_with_invalids().0.is_empty();
        widget
            .bottom_pane
            .set_status_line_enabled(status_line_enabled);
        widget.bottom_pane.set_collaboration_modes_enabled(
            widget.config.features.enabled(Feature::CollaborationModes),
        );
        widget.sync_personality_command_enabled();
        #[cfg(target_os = "windows")]
        widget.bottom_pane.set_windows_degraded_sandbox_active(
            codex_core::windows_sandbox::ELEVATED_SANDBOX_NUX_ENABLED
                && matches!(
                    WindowsSandboxLevel::from_config(&widget.config),
                    WindowsSandboxLevel::RestrictedToken
                ),
        );
        widget.update_collaboration_mode_indicator();
        widget.refresh_model_display();
        widget.refresh_status_line();

        widget
            .bottom_pane
            .set_connectors_enabled(widget.config.features.enabled(Feature::Apps));

        widget
    }

    pub(crate) fn new_with_op_sender(
        common: ChatWidgetInit,
        codex_op_tx: UnboundedSender<Op>,
    ) -> Self {
        let ChatWidgetInit {
            config,
            frame_requester,
            app_event_tx,
            initial_user_message,
            enhanced_keys_supported,
            auth_manager,
            models_manager,
            feedback,
            is_first_run,
            feedback_audience,
            model,
            status_line_invalid_items_warned,
            otel_manager,
        } = common;
        let model = model.filter(|m| !m.trim().is_empty());
        let mut config = config;
        config.model = model.clone();
        let keybindings = Self::resolve_keybindings(&config, enhanced_keys_supported);
        let progress_legend_mode = config.tui_progress_legend_mode;
        let (progress_trace_styles, progress_trace_style_warnings) =
            resolve_progress_trace_styles(config.tui_progress_trace_style.as_ref());
        crate::markdown_render::set_syntax_highlight_theme(
            config.tui_syntax_highlight_theme.clone(),
        );
        let mut rng = rand::rng();
        let placeholder = PLACEHOLDERS[rng.random_range(0..PLACEHOLDERS.len())].to_string();

        let model_override = model.as_deref();
        let model_for_header = model
            .clone()
            .unwrap_or_else(|| DEFAULT_MODEL_DISPLAY_NAME.to_string());
        let active_collaboration_mask =
            Self::initial_collaboration_mask(&config, models_manager.as_ref(), model_override);
        let header_model = active_collaboration_mask
            .as_ref()
            .and_then(|mask| mask.model.clone())
            .unwrap_or_else(|| model_for_header.clone());
        let fallback_default = Settings {
            model: header_model.clone(),
            reasoning_effort: None,
            developer_instructions: None,
        };
        // Collaboration modes start in Default mode.
        let current_collaboration_mode = CollaborationMode {
            mode: ModeKind::Default,
            settings: fallback_default,
        };

        let active_cell = Some(Self::placeholder_session_header_cell(&config));
        let current_cwd = Some(config.cwd.clone());

        let mut widget = Self {
            app_event_tx: app_event_tx.clone(),
            frame_requester: frame_requester.clone(),
            codex_op_tx,
            bottom_pane: BottomPane::new(BottomPaneParams {
                frame_requester,
                app_event_tx,
                has_input_focus: true,
                enhanced_keys_supported,
                placeholder_text: placeholder,
                disable_paste_burst: config.disable_paste_burst,
                animations_enabled: config.animations,
                progress_legend_mode,
                progress_trace_styles,
                skills: None,
            }),
            active_cell,
            active_cell_revision: 0,
            config,
            keybindings,
            skills_all: Vec::new(),
            skills_initial_state: None,
            current_collaboration_mode,
            active_collaboration_mask,
            auth_manager,
            models_manager,
            otel_manager,
            session_header: SessionHeader::new(header_model),
            initial_user_message,
            token_info: None,
            rate_limit_snapshot: None,
            plan_type: None,
            rate_limit_warnings: RateLimitWarningState::default(),
            rate_limit_switch_prompt: RateLimitSwitchPromptState::default(),
            rate_limit_poller: None,
            adaptive_chunking: AdaptiveChunkingPolicy::default(),
            stream_controller: None,
            plan_stream_controller: None,
            last_copyable_output: None,
            running_commands: HashMap::new(),
            suppressed_exec_calls: HashSet::new(),
            last_unified_wait: None,
            unified_exec_wait_streak: None,
            task_complete_pending: false,
            unified_exec_processes: Vec::new(),
            agent_turn_running: false,
            mcp_startup_status: None,
            connectors_cache: ConnectorsCacheState::default(),
            interrupts: InterruptManager::new(),
            reasoning_buffer: String::new(),
            full_reasoning_buffer: String::new(),
            current_status_header: String::from("Working"),
            retry_status_header: None,
            active_runtime_context: None,
            running_turn_model: None,
            running_turn_reasoning_effort: None,
            thread_id: None,
            thread_name: None,
            has_completed_assistant_message: false,
            last_assistant_output_markdown: None,
            copyable_messages: Vec::new(),
            copy_code_ui_state: None,
            copy_message_ui_state: None,
            forked_from: None,
            saw_plan_update_this_turn: false,
            saw_plan_item_this_turn: false,
            plan_delta_buffer: String::new(),
            plan_item_active: false,
            queued_user_messages: VecDeque::new(),
            active_turn_id: None,
            pending_steers: VecDeque::new(),
            rejected_steers_queue: VecDeque::new(),
            rejected_steer_history_records: VecDeque::new(),
            user_turn_pending_start: false,
            last_rendered_user_message_display: None,
            next_queued_user_message_id: 1,
            next_pending_steer_client_id: 1,
            queued_edit_state: None,
            show_welcome_banner: is_first_run,
            suppress_session_configured_redraw: false,
            suppress_queue_autosend: false,
            pending_notification: None,
            quit_shortcut_expires_at: None,
            quit_shortcut_key: None,
            is_review_mode: false,
            pre_review_token_info: None,
            needs_final_message_separator: false,
            had_work_activity: false,
            turn_progress_trace: Vec::new(),
            pending_separator_progress_trace: None,
            progress_legend_mode,
            progress_trace_styles,
            last_separator_elapsed_secs: None,
            last_rendered_width: std::cell::Cell::new(None),
            feedback,
            feedback_audience,
            current_rollout_path: None,
            current_cwd,
            status_line_invalid_items_warned,
            status_line_branch: None,
            status_line_branch_cwd: None,
            status_line_branch_pending: false,
            status_line_branch_lookup_complete: false,
            external_editor_state: ExternalEditorState::Closed,
        };

        for warning in progress_trace_style_warnings {
            widget.add_to_history(history_cell::new_warning_event(warning));
        }

        widget
            .bottom_pane
            .set_keybindings(widget.keybindings.clone());
        widget.prefetch_rate_limits();
        widget
            .bottom_pane
            .set_steer_enabled(widget.config.features.enabled(Feature::Steer));
        let status_line_enabled = !widget.status_line_items_with_invalids().0.is_empty();
        widget
            .bottom_pane
            .set_status_line_enabled(status_line_enabled);
        widget.bottom_pane.set_collaboration_modes_enabled(
            widget.config.features.enabled(Feature::CollaborationModes),
        );
        widget.sync_personality_command_enabled();
        widget.refresh_status_line();

        widget
    }

    /// Create a ChatWidget attached to an existing conversation (e.g., a fork).
    pub(crate) fn new_from_existing(
        common: ChatWidgetInit,
        conversation: std::sync::Arc<codex_core::CodexThread>,
        session_configured: codex_core::protocol::SessionConfiguredEvent,
    ) -> Self {
        let ChatWidgetInit {
            config,
            frame_requester,
            app_event_tx,
            initial_user_message,
            enhanced_keys_supported,
            auth_manager,
            models_manager,
            feedback,
            is_first_run: _,
            feedback_audience,
            model,
            status_line_invalid_items_warned,
            otel_manager,
        } = common;
        let model = model.filter(|m| !m.trim().is_empty());
        let keybindings = Self::resolve_keybindings(&config, enhanced_keys_supported);
        let progress_legend_mode = config.tui_progress_legend_mode;
        let (progress_trace_styles, progress_trace_style_warnings) =
            resolve_progress_trace_styles(config.tui_progress_trace_style.as_ref());
        crate::markdown_render::set_syntax_highlight_theme(
            config.tui_syntax_highlight_theme.clone(),
        );
        let mut rng = rand::rng();
        let placeholder = PLACEHOLDERS[rng.random_range(0..PLACEHOLDERS.len())].to_string();

        let model_override = model.as_deref();
        let header_model = model
            .clone()
            .unwrap_or_else(|| session_configured.model.clone());
        let active_collaboration_mask =
            Self::initial_collaboration_mask(&config, models_manager.as_ref(), model_override);
        let header_model = active_collaboration_mask
            .as_ref()
            .and_then(|mask| mask.model.clone())
            .unwrap_or(header_model);

        let current_cwd = Some(session_configured.cwd.clone());
        let codex_op_tx =
            spawn_agent_from_existing(conversation, session_configured, app_event_tx.clone());

        let fallback_default = Settings {
            model: header_model.clone(),
            reasoning_effort: None,
            developer_instructions: None,
        };
        // Collaboration modes start in Default mode.
        let current_collaboration_mode = CollaborationMode {
            mode: ModeKind::Default,
            settings: fallback_default,
        };

        let mut widget = Self {
            app_event_tx: app_event_tx.clone(),
            frame_requester: frame_requester.clone(),
            codex_op_tx,
            bottom_pane: BottomPane::new(BottomPaneParams {
                frame_requester,
                app_event_tx,
                has_input_focus: true,
                enhanced_keys_supported,
                placeholder_text: placeholder,
                disable_paste_burst: config.disable_paste_burst,
                animations_enabled: config.animations,
                progress_legend_mode,
                progress_trace_styles,
                skills: None,
            }),
            active_cell: None,
            active_cell_revision: 0,
            config,
            keybindings,
            skills_all: Vec::new(),
            skills_initial_state: None,
            current_collaboration_mode,
            active_collaboration_mask,
            auth_manager,
            models_manager,
            otel_manager,
            session_header: SessionHeader::new(header_model),
            initial_user_message,
            token_info: None,
            rate_limit_snapshot: None,
            plan_type: None,
            rate_limit_warnings: RateLimitWarningState::default(),
            rate_limit_switch_prompt: RateLimitSwitchPromptState::default(),
            rate_limit_poller: None,
            adaptive_chunking: AdaptiveChunkingPolicy::default(),
            stream_controller: None,
            plan_stream_controller: None,
            last_copyable_output: None,
            running_commands: HashMap::new(),
            suppressed_exec_calls: HashSet::new(),
            last_unified_wait: None,
            unified_exec_wait_streak: None,
            task_complete_pending: false,
            unified_exec_processes: Vec::new(),
            agent_turn_running: false,
            mcp_startup_status: None,
            connectors_cache: ConnectorsCacheState::default(),
            interrupts: InterruptManager::new(),
            reasoning_buffer: String::new(),
            full_reasoning_buffer: String::new(),
            current_status_header: String::from("Working"),
            retry_status_header: None,
            active_runtime_context: None,
            running_turn_model: None,
            running_turn_reasoning_effort: None,
            thread_id: None,
            thread_name: None,
            has_completed_assistant_message: false,
            last_assistant_output_markdown: None,
            copyable_messages: Vec::new(),
            copy_code_ui_state: None,
            copy_message_ui_state: None,
            forked_from: None,
            queued_user_messages: VecDeque::new(),
            active_turn_id: None,
            pending_steers: VecDeque::new(),
            rejected_steers_queue: VecDeque::new(),
            rejected_steer_history_records: VecDeque::new(),
            user_turn_pending_start: false,
            last_rendered_user_message_display: None,
            next_queued_user_message_id: 1,
            next_pending_steer_client_id: 1,
            queued_edit_state: None,
            show_welcome_banner: false,
            suppress_session_configured_redraw: true,
            suppress_queue_autosend: false,
            pending_notification: None,
            quit_shortcut_expires_at: None,
            quit_shortcut_key: None,
            is_review_mode: false,
            pre_review_token_info: None,
            needs_final_message_separator: false,
            had_work_activity: false,
            turn_progress_trace: Vec::new(),
            pending_separator_progress_trace: None,
            progress_legend_mode,
            progress_trace_styles,
            saw_plan_update_this_turn: false,
            saw_plan_item_this_turn: false,
            plan_delta_buffer: String::new(),
            plan_item_active: false,
            last_separator_elapsed_secs: None,
            last_rendered_width: std::cell::Cell::new(None),
            feedback,
            feedback_audience,
            current_rollout_path: None,
            current_cwd,
            status_line_invalid_items_warned,
            status_line_branch: None,
            status_line_branch_cwd: None,
            status_line_branch_pending: false,
            status_line_branch_lookup_complete: false,
            external_editor_state: ExternalEditorState::Closed,
        };

        for warning in progress_trace_style_warnings {
            widget.add_to_history(history_cell::new_warning_event(warning));
        }

        widget
            .bottom_pane
            .set_keybindings(widget.keybindings.clone());
        widget.prefetch_rate_limits();
        widget
            .bottom_pane
            .set_steer_enabled(widget.config.features.enabled(Feature::Steer));
        let status_line_enabled = !widget.status_line_items_with_invalids().0.is_empty();
        widget
            .bottom_pane
            .set_status_line_enabled(status_line_enabled);
        widget.bottom_pane.set_collaboration_modes_enabled(
            widget.config.features.enabled(Feature::CollaborationModes),
        );
        widget.sync_personality_command_enabled();
        #[cfg(target_os = "windows")]
        widget.bottom_pane.set_windows_degraded_sandbox_active(
            codex_core::windows_sandbox::ELEVATED_SANDBOX_NUX_ENABLED
                && matches!(
                    WindowsSandboxLevel::from_config(&widget.config),
                    WindowsSandboxLevel::RestrictedToken
                ),
        );
        widget.update_collaboration_mode_indicator();
        widget.refresh_model_display();
        widget.refresh_status_line();

        widget
    }
}
