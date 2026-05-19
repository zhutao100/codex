//! Footer status-line and status output controls for ChatWidget.

use super::*;

impl ChatWidget {
    /// Update the status indicator header and details.
    ///
    /// Passing `None` clears any existing details.
    pub(super) fn set_status(&mut self, header: String, details: Option<String>) {
        self.current_status_header = header.clone();
        self.bottom_pane.update_status(header, details);
    }

    /// Convenience wrapper around [`Self::set_status`];
    /// updates the status indicator header and clears any existing details.
    pub(super) fn set_status_header(&mut self, header: String) {
        self.set_status(header, None);
    }

    /// Sets the currently rendered footer status-line value and schedules a redraw.
    pub(crate) fn set_status_line(&mut self, status_line: Option<Line<'static>>) {
        self.bottom_pane.set_status_line(status_line);
        self.request_redraw();
    }

    /// Recomputes footer status-line content from config and current runtime state.
    ///
    /// This method is the status-line orchestrator: it parses configured item identifiers,
    /// warns once per session about invalid items, updates whether status-line mode is enabled,
    /// schedules async git-branch lookup when needed, and renders only values that are currently
    /// available.
    ///
    /// The omission behavior is intentional. If selected items are unavailable (for example before
    /// a session id exists or before branch lookup completes), those items are skipped without
    /// placeholders so the line remains compact and stable.
    pub(crate) fn refresh_status_line(&mut self) {
        let (items, invalid_items) = self.status_line_items_with_invalids();
        if self.thread_id.is_some()
            && !invalid_items.is_empty()
            && self
                .status_line_invalid_items_warned
                .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
        {
            let label = if invalid_items.len() == 1 {
                "item"
            } else {
                "items"
            };
            let message = format!(
                "Ignored invalid status line {label}: {}.",
                proper_join(invalid_items.as_slice())
            );
            self.on_warning(message);
        }
        if !items.contains(&StatusLineItem::GitBranch) {
            self.status_line_branch = None;
            self.status_line_branch_pending = false;
            self.status_line_branch_lookup_complete = false;
        }
        let enabled = !items.is_empty();
        self.bottom_pane.set_status_line_enabled(enabled);
        if !enabled {
            self.set_status_line(None);
            return;
        }

        let cwd = self.status_line_cwd().to_path_buf();
        self.sync_status_line_branch_state(&cwd);

        if items.contains(&StatusLineItem::GitBranch) && !self.status_line_branch_lookup_complete {
            self.request_status_line_branch(cwd);
        }

        let mut parts = Vec::new();
        for item in items {
            if let Some(value) = self.status_line_value_for_item(&item) {
                parts.push(value);
            }
        }

        let line = if parts.is_empty() {
            None
        } else {
            Some(Line::from(parts.join(" · ")))
        };
        self.set_status_line(line);
    }

    /// Records that status-line setup was canceled.
    ///
    /// Cancellation is intentionally side-effect free for config state; the existing configuration
    /// remains active and no persistence is attempted.
    pub(crate) fn cancel_status_line_setup(&self) {
        tracing::info!("Status line setup canceled by user");
    }

    /// Applies status-line item selection from the setup view to in-memory config.
    ///
    /// We persist an explicit empty list when all items are deselected so "unset"
    /// (`None`) can retain the built-in default footer status line configuration.
    pub(crate) fn setup_status_line(&mut self, items: Vec<StatusLineItem>) {
        tracing::info!("status line setup confirmed with items: {items:#?}");
        let ids = items.iter().map(ToString::to_string).collect::<Vec<_>>();
        self.config.tui_status_line = Some(ids);
        self.refresh_status_line();
    }

    pub(crate) fn set_progress_legend_mode(&mut self, mode: ProgressLegendMode) {
        self.progress_legend_mode = mode;
        self.config.tui_progress_legend_mode = mode;
        self.bottom_pane.set_progress_legend_mode(mode);
    }

    /// Stores async git-branch lookup results for the current status-line cwd.
    ///
    /// Results are dropped when they target an out-of-date cwd to avoid rendering stale branch
    /// names after directory changes.
    pub(crate) fn set_status_line_branch(&mut self, cwd: PathBuf, branch: Option<String>) {
        if self.status_line_branch_cwd.as_ref() != Some(&cwd) {
            self.status_line_branch_pending = false;
            return;
        }
        self.status_line_branch = branch;
        self.status_line_branch_pending = false;
        self.status_line_branch_lookup_complete = true;
    }

    pub(super) fn refresh_status_subject(&mut self) {
        self.sync_context_window_indicator();
        self.refresh_status_line();
        self.update_task_running_state();
        self.request_redraw();
    }

    pub(crate) fn set_token_info(&mut self, info: Option<TokenUsageInfo>) {
        self.token_info = info;
        self.sync_context_window_indicator();
    }

    pub(super) fn context_remaining_percent(&self, info: &TokenUsageInfo) -> Option<i64> {
        info.model_context_window.map(|window| {
            info.last_token_usage
                .percent_of_context_window_remaining(window)
        })
    }

    pub(super) fn context_used_tokens(
        &self,
        info: &TokenUsageInfo,
        percent_known: bool,
    ) -> Option<i64> {
        if percent_known {
            return None;
        }

        Some(info.total_token_usage.tokens_in_context_window())
    }

    pub(super) fn sync_context_window_indicator(&mut self) {
        let Some(info) = self.status_subject_token_info() else {
            self.bottom_pane.set_context_window(None, None);
            return;
        };
        let percent = self.context_remaining_percent(info);
        let used_tokens = self.context_used_tokens(info, percent.is_some());
        self.bottom_pane.set_context_window(percent, used_tokens);
    }

    pub(crate) fn add_status_output(&mut self) {
        if let Some(runtime_context) = self.active_runtime_context.as_ref() {
            let snapshot =
                crate::status::StatusOutputSnapshot::from_runtime_context(runtime_context);
            self.add_to_history(crate::status::new_status_output_from_snapshot(
                snapshot,
                self.auth_manager.as_ref(),
                self.rate_limit_snapshot.as_ref(),
                self.plan_type,
                Local::now(),
            ));
            return;
        }

        let default_usage = TokenUsage::default();
        let token_info = self.token_info.as_ref();
        let total_usage = token_info
            .map(|ti| &ti.total_token_usage)
            .unwrap_or(&default_usage);
        let collaboration_mode = self.collaboration_mode_label();
        let reasoning_effort_override = Some(self.effective_reasoning_effort());
        self.add_to_history(crate::status::new_status_output(
            &self.config,
            self.auth_manager.as_ref(),
            token_info,
            total_usage,
            &self.thread_id,
            self.thread_name.clone(),
            self.forked_from,
            self.rate_limit_snapshot.as_ref(),
            self.plan_type,
            Local::now(),
            self.model_display_name(),
            collaboration_mode,
            reasoning_effort_override,
        ));
    }

    pub(super) fn open_status_line_setup(&mut self) {
        let selected_items = self
            .config
            .tui_status_line
            .clone()
            .unwrap_or_else(Self::default_status_line_item_ids);
        let view =
            StatusLineSetupView::new(Some(selected_items.as_slice()), self.app_event_tx.clone());
        self.bottom_pane.show_view(Box::new(view));
    }

    pub(super) fn open_progress_legend_popup(&mut self) {
        let mut items = vec![SelectionItem {
            name: format!("Mode: {}", self.progress_legend_mode),
            description: Some("Choose when the progress legend is shown.".to_string()),
            actions: vec![Box::new(|tx| {
                tx.send(AppEvent::OpenProgressLegendModePicker);
            })],
            dismiss_on_select: false,
            ..Default::default()
        }];
        for category in [
            ProgressTraceCategory::Tool,
            ProgressTraceCategory::Edit,
            ProgressTraceCategory::Waiting,
            ProgressTraceCategory::Network,
            ProgressTraceCategory::Prefill,
            ProgressTraceCategory::Reasoning,
            ProgressTraceCategory::Gen,
        ] {
            let swatch_style =
                progress_trace_style_for_category(&self.progress_trace_styles, category).to_style();
            items.push(SelectionItem {
                name: progress_trace_category_label(category).to_string(),
                name_prefix: Some(Span::styled("▮ ", swatch_style)),
                description: Some(progress_trace_style_description(
                    category,
                    &self.progress_trace_styles,
                )),
                ..Default::default()
            });
        }
        self.show_selection_view(SelectionViewParams {
            title: Some("Progress Legend".to_string()),
            footer_hint: Some(standard_popup_hint_line()),
            items,
            initial_selected_idx: Some(1),
            ..Default::default()
        });
    }

    pub(crate) fn open_progress_legend_mode_picker(&mut self) {
        let mut items = Vec::new();
        for mode in [
            ProgressLegendMode::Off,
            ProgressLegendMode::Auto,
            ProgressLegendMode::Always,
        ] {
            items.push(SelectionItem {
                name: mode.to_string(),
                description: Some(format!("Set progress legend mode to '{mode}'.")),
                is_current: self.progress_legend_mode == mode,
                actions: vec![Box::new(move |tx| {
                    tx.send(AppEvent::SetProgressLegendMode { mode });
                })],
                dismiss_on_select: true,
                ..Default::default()
            });
        }

        self.show_selection_view(SelectionViewParams {
            title: Some("Progress legend mode".to_string()),
            footer_hint: Some(standard_popup_hint_line()),
            items,
            ..Default::default()
        });
    }

    pub(super) fn status_line_context_window_size(&self) -> Option<i64> {
        self.status_subject_context_window_size()
    }

    pub(super) fn status_line_context_remaining_percent(&self) -> Option<i64> {
        let Some(context_window) = self.status_line_context_window_size() else {
            return Some(100);
        };
        let default_usage = TokenUsage::default();
        let usage = self
            .status_subject_token_info()
            .map(|info| &info.last_token_usage)
            .unwrap_or(&default_usage);
        Some(
            usage
                .percent_of_context_window_remaining(context_window)
                .clamp(0, 100),
        )
    }

    pub(super) fn status_line_context_used_percent(&self) -> Option<i64> {
        let remaining = self.status_line_context_remaining_percent().unwrap_or(100);
        Some((100 - remaining).clamp(0, 100))
    }

    pub(super) fn status_line_total_usage(&self) -> TokenUsage {
        self.status_subject_token_info()
            .map(|info| info.total_token_usage.clone())
            .unwrap_or_default()
    }

    pub(super) fn status_subject_token_info(&self) -> Option<&TokenUsageInfo> {
        if let Some(snapshot) = self.active_runtime_context.as_ref() {
            return snapshot.token_info.as_ref();
        }

        self.token_info.as_ref()
    }

    pub(super) fn status_subject_model_context_window(&self) -> Option<i64> {
        if let Some(snapshot) = self.active_runtime_context.as_ref() {
            return snapshot.model_context_window;
        }

        self.config.model_context_window
    }

    pub(super) fn status_subject_context_window_size(&self) -> Option<i64> {
        self.status_subject_token_info()
            .and_then(|info| info.model_context_window)
            .or_else(|| self.status_subject_model_context_window())
    }

    pub(super) fn status_line_limit_display(
        &self,
        window: Option<&RateLimitWindowDisplay>,
        label: &str,
    ) -> Option<String> {
        let window = window?;
        let remaining = (100.0f64 - window.used_percent).clamp(0.0f64, 100.0f64);
        Some(format!("{label} {remaining:.0}%"))
    }

    pub(super) fn status_line_reasoning_effort_label(
        effort: Option<ReasoningEffortConfig>,
    ) -> &'static str {
        match effort {
            Some(ReasoningEffortConfig::Minimal) => "minimal",
            Some(ReasoningEffortConfig::Low) => "low",
            Some(ReasoningEffortConfig::Medium) => "medium",
            Some(ReasoningEffortConfig::High) => "high",
            Some(ReasoningEffortConfig::XHigh) => "xhigh",
            None | Some(ReasoningEffortConfig::None) => "default",
        }
    }
}
