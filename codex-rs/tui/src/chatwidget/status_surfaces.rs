//! Footer status-line value rendering for ChatWidget.

use super::*;

impl ChatWidget {
    /// Forces a new git-branch lookup when `GitBranch` is part of the configured status line.
    pub(super) fn request_status_line_branch_refresh(&mut self) {
        let (items, _) = self.status_line_items_with_invalids();
        if items.is_empty() || !items.contains(&StatusLineItem::GitBranch) {
            return;
        }
        let cwd = self.status_line_cwd().to_path_buf();
        self.sync_status_line_branch_state(&cwd);
        self.request_status_line_branch(cwd);
    }

    pub(super) fn default_status_line_items() -> Vec<StatusLineItem> {
        vec![
            StatusLineItem::ModelWithReasoning,
            StatusLineItem::ContextRemaining,
            StatusLineItem::CurrentDir,
            StatusLineItem::GitBranch,
        ]
    }

    pub(super) fn default_status_line_item_ids() -> Vec<String> {
        Self::default_status_line_items()
            .into_iter()
            .map(|item| item.to_string())
            .collect()
    }

    /// Parses configured status-line ids into known items and collects unknown ids.
    ///
    /// Unknown ids are deduplicated in insertion order for warning messages.
    pub(super) fn status_line_items_with_invalids(&self) -> (Vec<StatusLineItem>, Vec<String>) {
        let mut invalid = Vec::new();
        let mut invalid_seen = HashSet::new();
        let mut items = Vec::new();
        let Some(config_items) = self.config.tui_status_line.as_ref() else {
            return (Self::default_status_line_items(), invalid);
        };
        for id in config_items {
            match id.parse::<StatusLineItem>() {
                Ok(item) => items.push(item),
                Err(_) => {
                    if invalid_seen.insert(id.clone()) {
                        invalid.push(format!(r#""{id}""#));
                    }
                }
            }
        }
        (items, invalid)
    }

    pub(super) fn status_line_cwd(&self) -> &Path {
        if let Some(snapshot) = self.active_runtime_context.as_ref() {
            return &snapshot.cwd;
        }
        self.current_cwd.as_ref().unwrap_or(&self.config.cwd)
    }

    pub(super) fn status_line_project_root(&self) -> Option<PathBuf> {
        let cwd = self.status_line_cwd();
        if let Some(repo_root) = get_git_repo_root(cwd) {
            return Some(repo_root);
        }

        self.config
            .config_layer_stack
            .get_layers(ConfigLayerStackOrdering::LowestPrecedenceFirst, true)
            .iter()
            .find_map(|layer| match &layer.name {
                ConfigLayerSource::Project { dot_codex_folder } => {
                    dot_codex_folder.as_path().parent().map(Path::to_path_buf)
                }
                _ => None,
            })
    }

    pub(super) fn status_line_project_root_name(&self) -> Option<String> {
        self.status_line_project_root().map(|root| {
            root.file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_else(|| format_directory_display(&root, None))
        })
    }

    /// Resets git-branch cache state when the status-line cwd changes.
    ///
    /// The branch cache is keyed by cwd because branch lookup is performed relative to that path.
    /// Keeping stale branch values across cwd changes would surface incorrect repository context.
    pub(super) fn sync_status_line_branch_state(&mut self, cwd: &Path) {
        if self
            .status_line_branch_cwd
            .as_ref()
            .is_some_and(|path| path == cwd)
        {
            return;
        }
        self.status_line_branch_cwd = Some(cwd.to_path_buf());
        self.status_line_branch = None;
        self.status_line_branch_pending = false;
        self.status_line_branch_lookup_complete = false;
    }

    /// Starts an async git-branch lookup unless one is already running.
    ///
    /// The resulting `StatusLineBranchUpdated` event carries the lookup cwd so callers can reject
    /// stale completions after directory changes.
    pub(super) fn request_status_line_branch(&mut self, cwd: PathBuf) {
        if self.status_line_branch_pending {
            return;
        }
        self.status_line_branch_pending = true;
        let tx = self.app_event_tx.clone();
        tokio::spawn(async move {
            let branch = current_branch_name(&cwd).await;
            tx.send(AppEvent::StatusLineBranchUpdated { cwd, branch });
        });
    }

    pub(super) fn status_line_model_display_name(&self) -> &str {
        if let Some(snapshot) = self.active_runtime_context.as_ref()
            && !snapshot.model.trim().is_empty()
        {
            return snapshot.model.as_str();
        }
        self.model_display_name()
    }

    pub(super) fn status_line_reasoning_effort(&self) -> Option<ReasoningEffortConfig> {
        if let Some(snapshot) = self.active_runtime_context.as_ref() {
            return snapshot.reasoning_effort;
        }
        self.effective_reasoning_effort()
    }

    /// Resolves a display string for one configured status-line item.
    ///
    /// Returning `None` means "omit this item for now", not "configuration error". Callers rely on
    /// this to keep partially available status lines readable while waiting for session, token, or
    /// git metadata.
    pub(super) fn status_line_value_for_item(&self, item: &StatusLineItem) -> Option<String> {
        match item {
            StatusLineItem::ModelName => Some(self.status_line_model_display_name().to_string()),
            StatusLineItem::ModelWithReasoning => {
                let label =
                    Self::status_line_reasoning_effort_label(self.status_line_reasoning_effort());
                Some(format!("{} {label}", self.status_line_model_display_name()))
            }
            StatusLineItem::CurrentDir => {
                Some(format_directory_display(self.status_line_cwd(), None))
            }
            StatusLineItem::ProjectRoot => self.status_line_project_root_name(),
            StatusLineItem::GitBranch => self.status_line_branch.clone(),
            StatusLineItem::UsedTokens => {
                let usage = self.status_line_total_usage();
                let total = usage.tokens_in_context_window();
                if total <= 0 {
                    None
                } else {
                    Some(format!("{} used", format_tokens_compact(total)))
                }
            }
            StatusLineItem::ContextRemaining => self
                .status_line_context_remaining_percent()
                .map(|remaining| format!("{remaining}% left")),
            StatusLineItem::ContextUsed => self
                .status_line_context_used_percent()
                .map(|used| format!("{used}% used")),
            StatusLineItem::FiveHourLimit => {
                let window = self
                    .rate_limit_snapshot
                    .as_ref()
                    .and_then(|s| s.primary.as_ref());
                let label = window
                    .and_then(|window| window.window_minutes)
                    .map(get_limits_duration)
                    .unwrap_or_else(|| "5h".to_string());
                self.status_line_limit_display(window, &label)
            }
            StatusLineItem::WeeklyLimit => {
                let window = self
                    .rate_limit_snapshot
                    .as_ref()
                    .and_then(|s| s.secondary.as_ref());
                let label = window
                    .and_then(|window| window.window_minutes)
                    .map(get_limits_duration)
                    .unwrap_or_else(|| "weekly".to_string());
                self.status_line_limit_display(window, &label)
            }
            StatusLineItem::CodexVersion => Some(CODEX_CLI_VERSION.to_string()),
            StatusLineItem::ContextWindowSize => self
                .status_line_context_window_size()
                .map(|cws| format!("{} window", format_tokens_compact(cws))),
            StatusLineItem::TotalInputTokens => Some(format!(
                "{} in",
                format_tokens_compact(self.status_line_total_usage().input_tokens)
            )),
            StatusLineItem::TotalOutputTokens => Some(format!(
                "{} out",
                format_tokens_compact(self.status_line_total_usage().output_tokens)
            )),
            StatusLineItem::SessionId => self
                .active_runtime_context
                .as_ref()
                .map(|snapshot| snapshot.session_id.to_string())
                .or_else(|| self.thread_id.map(|id| id.to_string())),
        }
    }
}
