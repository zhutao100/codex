//! Slash-command dispatch and export helpers for ChatWidget.

use super::*;

#[derive(Debug, Default)]
pub(super) struct ParsedExportArgs {
    pub(super) format: Option<ChatExportFormat>,
    pub(super) overrides: ExportOverrides,
}

#[derive(Debug)]
pub(super) struct ExportDestination {
    pub(super) path: PathBuf,
    pub(super) format: ChatExportFormat,
}

#[derive(Debug)]
pub(super) enum ExportPathSpec {
    File(PathBuf),
    Dir(PathBuf),
}

pub(super) fn parse_export_args(args: &str, cwd: &Path) -> Result<ParsedExportArgs, String> {
    let mut parsed = ParsedExportArgs::default();
    if args.trim().is_empty() {
        return Ok(parsed);
    }

    let tokens =
        shlex::split(args).ok_or_else(|| "Could not parse /export arguments.".to_string())?;
    let mut positional: Option<String> = None;
    let mut idx = 0usize;

    while idx < tokens.len() {
        let token = &tokens[idx];
        if token == "--" {
            if idx + 1 >= tokens.len() {
                return Err("Expected a path after --.".to_string());
            }
            if tokens.len() - idx > 2 {
                return Err(format!(
                    "Unexpected /export arguments: {}",
                    tokens[idx + 2..].join(" ")
                ));
            }
            positional = Some(tokens[idx + 1].clone());
            break;
        }

        let next_value =
            |idx: &mut usize, tokens: &[String], flag: &str| -> Result<String, String> {
                *idx += 1;
                if *idx >= tokens.len() {
                    return Err(format!("Expected a value after {flag}."));
                }
                Ok(tokens[*idx].clone())
            };

        match token.as_str() {
            "-f" | "--format" => {
                let value = next_value(&mut idx, &tokens, token)?;
                parsed.format = Some(parse_export_format(&value)?);
            }
            "--json" => {
                parsed.format = Some(ChatExportFormat::Json);
            }
            "--markdown" | "--md" => {
                parsed.format = Some(ChatExportFormat::Markdown);
            }
            "-o" | "--output" => {
                let value = next_value(&mut idx, &tokens, token)?;
                parsed.overrides.output_path = Some(resolve_input_path(cwd, &value));
            }
            "-C" | "--dir" => {
                let value = next_value(&mut idx, &tokens, token)?;
                parsed.overrides.output_dir = Some(resolve_input_path(cwd, &value));
            }
            "--name" => {
                let value = next_value(&mut idx, &tokens, token)?;
                parsed.overrides.name = Some(value);
            }
            _ if token.starts_with('-') => {
                return Err(format!("Unknown /export flag: {token}"));
            }
            _ => {
                if positional.is_some() {
                    return Err("Provide only one export path.".to_string());
                }
                positional = Some(token.clone());
            }
        }

        idx += 1;
    }

    if parsed.overrides.output_path.is_some() && parsed.overrides.output_dir.is_some() {
        return Err("Use either --output or --dir, not both.".to_string());
    }

    if let Some(positional) = positional {
        if parsed.overrides.output_path.is_some() || parsed.overrides.output_dir.is_some() {
            return Err("Provide only one export path (flag or positional).".to_string());
        }
        match classify_export_path(&positional, cwd) {
            ExportPathSpec::File(path) => parsed.overrides.output_path = Some(path),
            ExportPathSpec::Dir(path) => parsed.overrides.output_dir = Some(path),
        }
    }

    Ok(parsed)
}

pub(super) fn parse_export_format(value: &str) -> Result<ChatExportFormat, String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "md" | "markdown" => Ok(ChatExportFormat::Markdown),
        "json" => Ok(ChatExportFormat::Json),
        _ => Err(format!(
            "Unknown export format: {value} (expected md or json)."
        )),
    }
}

pub(super) fn export_overrides_from_path_input(value: &str, cwd: &Path) -> ExportOverrides {
    match classify_export_path(value, cwd) {
        ExportPathSpec::File(path) => ExportOverrides {
            output_path: Some(path),
            ..Default::default()
        },
        ExportPathSpec::Dir(path) => ExportOverrides {
            output_dir: Some(path),
            ..Default::default()
        },
    }
}

pub(super) fn classify_export_path(value: &str, cwd: &Path) -> ExportPathSpec {
    let path = resolve_input_path(cwd, value);
    let trailing_separator = value.ends_with(std::path::MAIN_SEPARATOR)
        || (std::path::MAIN_SEPARATOR != '/' && value.ends_with('/'))
        || (std::path::MAIN_SEPARATOR != '\\' && value.ends_with('\\'));
    if trailing_separator || path.is_dir() {
        ExportPathSpec::Dir(path)
    } else {
        ExportPathSpec::File(path)
    }
}

pub(super) fn resolve_input_path(cwd: &Path, value: &str) -> PathBuf {
    let trimmed = value.trim();
    let path = if let Some(rest) = trimmed.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            home.join(rest)
        } else {
            PathBuf::from(trimmed)
        }
    } else {
        PathBuf::from(trimmed)
    };
    if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    }
}

pub(super) fn resolve_export_destination(
    rollout_path: &Path,
    format_override: Option<ChatExportFormat>,
    overrides: &ExportOverrides,
) -> Result<ExportDestination, String> {
    let mut format = format_override
        .or_else(|| {
            overrides
                .output_path
                .as_deref()
                .and_then(format_from_extension)
        })
        .unwrap_or(ChatExportFormat::Markdown);

    if let Some(mut path) = overrides.output_path.clone() {
        if path.is_dir() {
            return Err(format!("Export path is a directory: {}", path.display()));
        }
        if path.extension().is_none() {
            path.set_extension(format.extension());
        } else if let Some(from_ext) = format_from_extension(&path) {
            format = from_ext;
        }
        return Ok(ExportDestination { path, format });
    }

    let output_dir = overrides
        .output_dir
        .clone()
        .or_else(|| rollout_path.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."));
    if output_dir.is_file() {
        return Err(format!(
            "Export directory is a file: {}",
            output_dir.display()
        ));
    }

    let export_name = if let Some(name) = overrides.name.as_deref() {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            return Err("Export name cannot be empty.".to_string());
        }
        trimmed.to_string()
    } else {
        default_export_name(rollout_path)?
    };

    if export_name.contains(std::path::MAIN_SEPARATOR) || export_name.contains('/') {
        return Err("Export name must not contain path separators.".to_string());
    }

    let mut path = output_dir.join(export_name);
    path.set_extension(format.extension());

    Ok(ExportDestination { path, format })
}

pub(super) fn default_export_name(rollout_path: &Path) -> Result<String, String> {
    let stem = rollout_path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .map(str::trim)
        .filter(|stem| !stem.is_empty())
        .ok_or_else(|| "Failed to derive export name from rollout path.".to_string())?;
    Ok(stem.to_string())
}

pub(super) fn format_from_extension(path: &Path) -> Option<ChatExportFormat> {
    let ext = path.extension()?.to_str()?;
    match ext.to_ascii_lowercase().as_str() {
        "md" | "markdown" => Some(ChatExportFormat::Markdown),
        "json" => Some(ChatExportFormat::Json),
        _ => None,
    }
}

pub(super) fn diff_view_override_from_args(
    args: &str,
    default_view: DiffView,
) -> Result<DiffView, String> {
    if args.trim().is_empty() {
        return Ok(default_view);
    }

    let Some(tokens) = shlex::split(args) else {
        return Err("Failed to parse /diff arguments.".to_string());
    };

    if tokens.is_empty() {
        return Ok(default_view);
    }

    let mut view_override = None;
    let mut args_iter = tokens.iter();
    while let Some(arg) = args_iter.next() {
        if let Some(value) = arg.strip_prefix("--view=") {
            view_override = Some(parse_diff_view_value(value)?);
            continue;
        }

        match arg.as_str() {
            "--pretty" => view_override = Some(DiffView::Pretty),
            "--line" => view_override = Some(DiffView::Line),
            "--inline" => view_override = Some(DiffView::Inline),
            "--side-by-side" => view_override = Some(DiffView::SideBySide),
            "--view" => {
                let Some(value) = args_iter.next() else {
                    return Err(
                        "Expected a value after --view (pretty, line, inline, or side-by-side)."
                            .to_string(),
                    );
                };
                view_override = Some(parse_diff_view_value(value)?);
            }
            _ if arg.starts_with('-') => {
                return Err(format!("Unknown /diff flag: {arg}"));
            }
            _ => {
                return Err(format!("Unexpected /diff argument: {arg}"));
            }
        }
    }

    Ok(view_override.unwrap_or(default_view))
}

pub(super) fn parse_diff_view_value(value: &str) -> Result<DiffView, String> {
    match value {
        "pretty" => Ok(DiffView::Pretty),
        "line" => Ok(DiffView::Line),
        "inline" => Ok(DiffView::Inline),
        "side-by-side" | "side_by_side" | "side" => Ok(DiffView::SideBySide),
        _ => Err(format!(
            "Invalid /diff view '{value}'. Use 'pretty', 'line', 'inline', or 'side-by-side'."
        )),
    }
}

pub(super) fn parse_progress_legend_mode(value: &str) -> Result<ProgressLegendMode, String> {
    match value.trim() {
        "off" => Ok(ProgressLegendMode::Off),
        "auto" => Ok(ProgressLegendMode::Auto),
        "always" => Ok(ProgressLegendMode::Always),
        _ => Err("Invalid /legend-mode value. Use 'off', 'auto', or 'always'.".to_string()),
    }
}

impl ChatWidget {
    pub(super) fn dispatch_command(&mut self, cmd: SlashCommand) {
        if !cmd.available_during_task() && self.bottom_pane.is_task_running() {
            let message = format!(
                "'/{}' is disabled while a task is in progress.",
                cmd.command()
            );
            self.add_to_history(history_cell::new_error_event(message));
            self.bottom_pane.drain_pending_submission_state();
            self.request_redraw();
            return;
        }
        match cmd {
            SlashCommand::Feedback => {
                if !self.config.feedback_enabled {
                    let params = crate::bottom_pane::feedback_disabled_params();
                    self.bottom_pane.show_selection_view(params);
                    self.request_redraw();
                    return;
                }
                // Step 1: pick a category (UI built in feedback_view)
                let params =
                    crate::bottom_pane::feedback_selection_params(self.app_event_tx.clone());
                self.bottom_pane.show_selection_view(params);
                self.request_redraw();
            }
            SlashCommand::New => {
                self.app_event_tx.send(AppEvent::NewSession);
            }
            SlashCommand::Resume => {
                self.app_event_tx.send(AppEvent::OpenResumePicker);
            }
            SlashCommand::Session => {
                self.app_event_tx.send(AppEvent::OpenSessionsPicker {
                    view: crate::sessions_picker::SessionView::Active,
                });
            }
            SlashCommand::Archived => {
                self.app_event_tx.send(AppEvent::OpenSessionsPicker {
                    view: crate::sessions_picker::SessionView::Archived,
                });
            }
            SlashCommand::Fork => {
                self.app_event_tx.send(AppEvent::ForkCurrentSession);
            }
            SlashCommand::Init => {
                let init_target = self.config.cwd.join(DEFAULT_PROJECT_DOC_FILENAME);
                if init_target.exists() {
                    let message = format!(
                        "{DEFAULT_PROJECT_DOC_FILENAME} already exists here. Skipping /init to avoid overwriting it."
                    );
                    self.add_info_message(message, None);
                    return;
                }
                const INIT_PROMPT: &str = include_str!("../../prompt_for_init_command.md");
                self.submit_user_message(INIT_PROMPT.to_string().into());
            }
            SlashCommand::Compact => {
                self.clear_token_usage();
                self.app_event_tx.send(AppEvent::CodexOp(Op::Compact));
            }
            SlashCommand::Pause => {
                if self.bottom_pane.is_task_running() {
                    self.submit_op(Op::Pause);
                } else {
                    self.add_info_message("No running turn to pause.".to_string(), None);
                }
            }
            SlashCommand::Continue => {
                if self.bottom_pane.is_task_running() {
                    self.add_info_message(
                        "A turn is already running.".to_string(),
                        Some("Pause or interrupt it before continuing another turn.".to_string()),
                    );
                } else {
                    self.user_turn_pending_start = true;
                    self.submit_op(Op::Continue);
                }
            }
            SlashCommand::Review => {
                self.open_review_popup();
            }
            SlashCommand::ReviewCompletedTurn => {
                self.submit_op(Op::ReviewCompletedTurn);
            }
            SlashCommand::Rename => {
                self.open_rename_thread_view();
            }
            SlashCommand::Export => {
                self.open_export_picker();
            }
            SlashCommand::Model => {
                self.open_model_popup();
            }
            SlashCommand::Personality => {
                self.open_personality_popup();
            }
            SlashCommand::Plan => {
                if !self.collaboration_modes_enabled() {
                    self.add_info_message(
                        "Collaboration modes are disabled.".to_string(),
                        Some("Enable collaboration modes to use /plan.".to_string()),
                    );
                    return;
                }
                if let Some(mask) = collaboration_modes::plan_mask(self.models_manager.as_ref()) {
                    self.set_collaboration_mask(mask);
                } else {
                    self.add_info_message("Plan mode unavailable right now.".to_string(), None);
                }
            }
            SlashCommand::Collab => {
                if !self.collaboration_modes_enabled() {
                    self.add_info_message(
                        "Collaboration modes are disabled.".to_string(),
                        Some("Enable collaboration modes to use /collab.".to_string()),
                    );
                    return;
                }
                self.open_collaboration_modes_popup();
            }
            SlashCommand::Agent => {
                self.app_event_tx.send(AppEvent::OpenAgentPicker);
            }
            SlashCommand::Approvals => {
                self.open_approvals_popup();
            }
            SlashCommand::Permissions => {
                self.open_permissions_popup();
            }
            SlashCommand::ElevateSandbox => {
                #[cfg(target_os = "windows")]
                {
                    let windows_sandbox_level = WindowsSandboxLevel::from_config(&self.config);
                    let windows_degraded_sandbox_enabled =
                        matches!(windows_sandbox_level, WindowsSandboxLevel::RestrictedToken);
                    if !windows_degraded_sandbox_enabled
                        || !codex_core::windows_sandbox::ELEVATED_SANDBOX_NUX_ENABLED
                    {
                        // This command should not be visible/recognized outside degraded mode,
                        // but guard anyway in case something dispatches it directly.
                        return;
                    }

                    let Some(preset) = builtin_approval_presets()
                        .into_iter()
                        .find(|preset| preset.id == "auto")
                    else {
                        // Avoid panicking in interactive UI; treat this as a recoverable
                        // internal error.
                        self.add_error_message(
                            "Internal error: missing the 'auto' approval preset.".to_string(),
                        );
                        return;
                    };

                    if let Err(err) = self.config.approval_policy.can_set(&preset.approval) {
                        self.add_error_message(err.to_string());
                        return;
                    }

                    self.otel_manager.counter(
                        "codex.windows_sandbox.setup_elevated_sandbox_command",
                        1,
                        &[],
                    );
                    self.app_event_tx
                        .send(AppEvent::BeginWindowsSandboxElevatedSetup { preset });
                }
                #[cfg(not(target_os = "windows"))]
                {
                    let _ = &self.otel_manager;
                    // Not supported; on non-Windows this command should never be reachable.
                };
            }
            SlashCommand::Experimental => {
                self.open_experimental_popup();
            }
            SlashCommand::Quit | SlashCommand::Exit => {
                self.request_quit_without_confirmation();
            }
            SlashCommand::Logout => {
                if let Err(e) = codex_core::auth::logout(
                    &self.config.codex_home,
                    self.config.cli_auth_credentials_store_mode,
                ) {
                    tracing::error!("failed to logout: {e}");
                }
                self.request_quit_without_confirmation();
            }
            // SlashCommand::Undo => {
            //     self.app_event_tx.send(AppEvent::CodexOp(Op::Undo));
            // }
            SlashCommand::Diff => {
                self.add_diff_in_progress();
                let tx = self.app_event_tx.clone();
                let cwd = self.config.cwd.clone();
                let diff_view = self.config.diff_view;
                let syntax_theme = self.config.tui_syntax_highlight_theme.clone();
                let width = self.last_rendered_width.get().unwrap_or(80);
                tokio::spawn(async move {
                    let result = match get_git_diff(&cwd, diff_view, width, &syntax_theme).await {
                        Ok(result) => result,
                        Err(e) => GitDiffResult::Error(format!("Failed to compute diff: {e}")),
                    };
                    tx.send(AppEvent::DiffResult(result));
                });
            }
            SlashCommand::Copy => {
                let Some(text) = self.last_copyable_output.as_deref() else {
                    self.add_info_message(
                        "`/copy` is unavailable before the first Codex output or right after a rollback."
                            .to_string(),
                        None,
                    );
                    return;
                };

                match clipboard_text::copy_text_to_clipboard(text) {
                    Ok(()) => {
                        let hint = self.agent_turn_running.then_some(
                            "Current turn is still running; copied the latest completed output (not the in-progress response)."
                                .to_string(),
                        );
                        self.add_info_message(
                            "Copied latest Codex output to clipboard.".to_string(),
                            hint,
                        );
                    }
                    Err(err) => {
                        self.add_error_message(format!("Failed to copy to clipboard: {err}"))
                    }
                }
            }
            SlashCommand::Mention => {
                self.insert_str("@");
            }
            SlashCommand::CopyCodeBlock => {
                self.open_copy_code_block_picker();
            }
            SlashCommand::CopyMessage => {
                self.open_copy_message_picker(CopyMessageFilter::Responses);
            }
            SlashCommand::Skills => {
                self.open_skills_menu();
            }
            SlashCommand::Status => {
                self.add_status_output();
            }
            SlashCommand::DebugConfig => {
                self.add_debug_config_output();
            }
            SlashCommand::Statusline => {
                self.open_status_line_setup();
            }
            SlashCommand::Legend => {
                self.open_progress_legend_popup();
            }
            SlashCommand::LegendMode => {
                self.add_info_message(
                    format!("Progress legend mode is '{}'.", self.progress_legend_mode),
                    Some("Use /legend-mode off|auto|always to change it.".to_string()),
                );
            }
            SlashCommand::Ps => {
                self.add_ps_output();
            }
            SlashCommand::Mcp => {
                self.add_mcp_output();
            }
            SlashCommand::Apps => {
                self.add_connectors_output();
            }
            SlashCommand::Queue => {
                if self.queued_user_messages.is_empty() {
                    self.add_info_message("Queue is empty.".to_string(), None);
                } else {
                    self.open_queue_popup();
                }
            }
            SlashCommand::Rollout => {
                if let Some(path) = self.rollout_path() {
                    self.add_info_message(
                        format!("Current rollout path: {}", path.display()),
                        None,
                    );
                } else {
                    self.add_info_message("Rollout path is not available yet.".to_string(), None);
                }
            }
            SlashCommand::TestApproval => {
                use codex_core::protocol::EventMsg;
                use std::collections::HashMap;

                use codex_core::protocol::ApplyPatchApprovalRequestEvent;
                use codex_core::protocol::FileChange;

                self.app_event_tx.send(AppEvent::CodexEvent(Event {
                    id: "1".to_string(),
                    // msg: EventMsg::ExecApprovalRequest(ExecApprovalRequestEvent {
                    //     call_id: "1".to_string(),
                    //     command: vec!["git".into(), "apply".into()],
                    //     cwd: self.config.cwd.clone(),
                    //     reason: Some("test".to_string()),
                    // }),
                    msg: EventMsg::ApplyPatchApprovalRequest(ApplyPatchApprovalRequestEvent {
                        call_id: "1".to_string(),
                        turn_id: "turn-1".to_string(),
                        changes: HashMap::from([
                            (
                                PathBuf::from("/tmp/test.txt"),
                                FileChange::Add {
                                    content: "test".to_string(),
                                },
                            ),
                            (
                                PathBuf::from("/tmp/test2.txt"),
                                FileChange::Update {
                                    unified_diff: "+test\n-test2".to_string(),
                                    move_path: None,
                                },
                            ),
                        ]),
                        reason: None,
                        grant_root: Some(PathBuf::from("/tmp")),
                    }),
                }));
            }
        }
    }

    pub(super) fn dispatch_command_with_args(
        &mut self,
        cmd: SlashCommand,
        args: String,
        _text_elements: Vec<TextElement>,
    ) {
        if !cmd.supports_inline_args() {
            self.dispatch_command(cmd);
            return;
        }
        if !cmd.available_during_task() && self.bottom_pane.is_task_running() {
            let message = format!(
                "'/{}' is disabled while a task is in progress.",
                cmd.command()
            );
            self.add_to_history(history_cell::new_error_event(message));
            self.request_redraw();
            return;
        }

        let trimmed = args.trim();
        match cmd {
            SlashCommand::Export if !trimmed.is_empty() => {
                match parse_export_args(trimmed, &self.config.cwd) {
                    Ok(parsed) => {
                        self.start_export(parsed.format, parsed.overrides);
                        self.bottom_pane.drain_pending_submission_state();
                    }
                    Err(message) => {
                        self.add_error_message(message);
                    }
                }
            }
            SlashCommand::Rename if !trimmed.is_empty() => {
                let Some((prepared_args, _prepared_elements)) =
                    self.bottom_pane.prepare_inline_args_submission(false)
                else {
                    return;
                };
                let Some(name) = codex_core::util::normalize_thread_name(&prepared_args) else {
                    self.add_error_message("Thread name cannot be empty.".to_string());
                    return;
                };
                self.app_event_tx
                    .send(AppEvent::CodexOp(Op::SetThreadName { name }));
                self.bottom_pane.drain_pending_submission_state();
            }
            SlashCommand::Plan if !trimmed.is_empty() => {
                self.dispatch_command(cmd);
                if self.active_mode_kind() != ModeKind::Plan {
                    return;
                }
                let Some((prepared_args, prepared_elements)) =
                    self.bottom_pane.prepare_inline_args_submission(true)
                else {
                    return;
                };
                let user_message = UserMessage {
                    text: prepared_args,
                    local_images: self
                        .bottom_pane
                        .take_recent_submission_images_with_placeholders(),
                    text_elements: prepared_elements,
                    mention_paths: self.bottom_pane.take_mention_paths(),
                };
                if self.is_session_configured() {
                    self.reasoning_buffer.clear();
                    self.full_reasoning_buffer.clear();
                    self.set_status_header(String::from("Working"));
                    self.submit_user_message(user_message);
                } else {
                    self.queue_user_message(user_message);
                }
            }
            SlashCommand::Review if !trimmed.is_empty() => {
                let Some((prepared_args, _prepared_elements)) =
                    self.bottom_pane.prepare_inline_args_submission(false)
                else {
                    return;
                };
                self.submit_op(Op::Review {
                    review_request: ReviewRequest {
                        target: ReviewTarget::Custom {
                            instructions: prepared_args,
                        },
                        user_facing_hint: None,
                    },
                });
                self.bottom_pane.drain_pending_submission_state();
            }
            SlashCommand::Diff => {
                let diff_view = match diff_view_override_from_args(trimmed, self.config.diff_view) {
                    Ok(view) => view,
                    Err(message) => {
                        self.add_error_message(message);
                        return;
                    }
                };
                self.add_diff_in_progress();
                let tx = self.app_event_tx.clone();
                let cwd = self.config.cwd.clone();
                let syntax_theme = self.config.tui_syntax_highlight_theme.clone();
                let width = self.last_rendered_width.get().unwrap_or(80);
                tokio::spawn(async move {
                    let result = match get_git_diff(&cwd, diff_view, width, &syntax_theme).await {
                        Ok(result) => result,
                        Err(e) => GitDiffResult::Error(format!("Failed to compute diff: {e}")),
                    };
                    tx.send(AppEvent::DiffResult(result));
                });
            }
            SlashCommand::LegendMode => match parse_progress_legend_mode(trimmed) {
                Ok(mode) => {
                    self.app_event_tx
                        .send(AppEvent::SetProgressLegendMode { mode });
                    self.bottom_pane.drain_pending_submission_state();
                }
                Err(err) => {
                    self.add_error_message(err);
                }
            },
            _ => self.dispatch_command(cmd),
        }
    }

    pub(super) fn submit_queued_slash_prompt(
        &mut self,
        mut queued: QueuedUserMessage,
    ) -> QueueDrain {
        if !queued.pending_pastes.is_empty() {
            let (expanded, expanded_elements) = ChatComposer::expand_pending_pastes(
                &queued.text,
                queued.text_elements,
                &queued.pending_pastes,
            );
            queued.text = expanded;
            queued.text_elements = expanded_elements;
            queued.pending_pastes.clear();
        }

        let Some((name, rest, rest_offset)) = parse_slash_name(&queued.text) else {
            self.submit_queued_user_message(queued);
            return QueueDrain::Stop;
        };

        if name.contains('/') {
            self.submit_queued_user_message(queued);
            return QueueDrain::Stop;
        }

        let Some(cmd) = self.find_queued_builtin_command(name) else {
            self.add_info_message(
                format!(
                    r#"Unrecognized command '/{name}'. Type "/" for a list of supported commands."#
                ),
                None,
            );
            return QueueDrain::Continue;
        };

        if rest.is_empty() {
            self.dispatch_command(cmd);
            return self.queued_command_drain_result(cmd);
        }

        if !cmd.supports_inline_args() {
            self.submit_queued_user_message(queued);
            return QueueDrain::Stop;
        }

        let trimmed_start = rest.trim_start();
        let leading_trimmed = rest.len().saturating_sub(trimmed_start.len());
        let trimmed_rest = trimmed_start.trim_end().to_string();
        let args_elements = Self::slash_command_args_elements(
            &trimmed_rest,
            rest_offset + leading_trimmed,
            &queued.text_elements,
        );

        match cmd {
            SlashCommand::Review if !trimmed_rest.is_empty() => {
                self.submit_op(Op::Review {
                    review_request: ReviewRequest {
                        target: ReviewTarget::Custom {
                            instructions: trimmed_rest,
                        },
                        user_facing_hint: None,
                    },
                });
                QueueDrain::Stop
            }
            SlashCommand::Rename if !trimmed_rest.is_empty() => {
                let Some(name) = codex_core::util::normalize_thread_name(&trimmed_rest) else {
                    self.add_error_message("Thread name cannot be empty.".to_string());
                    return QueueDrain::Continue;
                };
                self.app_event_tx
                    .send(AppEvent::CodexOp(Op::SetThreadName { name }));
                QueueDrain::Continue
            }
            SlashCommand::Plan if !trimmed_rest.is_empty() => {
                self.dispatch_command(cmd);
                if self.active_mode_kind() != ModeKind::Plan {
                    return self.queued_command_drain_result(cmd);
                }
                let user_message = UserMessage {
                    text: trimmed_rest,
                    local_images: queued.local_images,
                    text_elements: args_elements,
                    mention_paths: queued.mention_paths,
                };
                self.submit_user_message(user_message);
                QueueDrain::Stop
            }
            SlashCommand::Export if !trimmed_rest.is_empty() => {
                match parse_export_args(&trimmed_rest, &self.config.cwd) {
                    Ok(parsed) => self.start_export(parsed.format, parsed.overrides),
                    Err(message) => self.add_error_message(message),
                }
                QueueDrain::Stop
            }
            SlashCommand::Diff => {
                let diff_view =
                    match diff_view_override_from_args(&trimmed_rest, self.config.diff_view) {
                        Ok(view) => view,
                        Err(message) => {
                            self.add_error_message(message);
                            return QueueDrain::Continue;
                        }
                    };
                self.add_diff_in_progress();
                let tx = self.app_event_tx.clone();
                let cwd = self.config.cwd.clone();
                let syntax_theme = self.config.tui_syntax_highlight_theme.clone();
                let width = self.last_rendered_width.get().unwrap_or(80);
                tokio::spawn(async move {
                    let result = match get_git_diff(&cwd, diff_view, width, &syntax_theme).await {
                        Ok(result) => result,
                        Err(e) => GitDiffResult::Error(format!("Failed to compute diff: {e}")),
                    };
                    tx.send(AppEvent::DiffResult(result));
                });
                QueueDrain::Continue
            }
            SlashCommand::LegendMode => match parse_progress_legend_mode(&trimmed_rest) {
                Ok(mode) => {
                    self.app_event_tx
                        .send(AppEvent::SetProgressLegendMode { mode });
                    QueueDrain::Continue
                }
                Err(err) => {
                    self.add_error_message(err);
                    QueueDrain::Continue
                }
            },
            SlashCommand::Feedback
            | SlashCommand::New
            | SlashCommand::Resume
            | SlashCommand::Session
            | SlashCommand::Archived
            | SlashCommand::Fork
            | SlashCommand::Init
            | SlashCommand::Compact
            | SlashCommand::Pause
            | SlashCommand::Continue
            | SlashCommand::Review
            | SlashCommand::ReviewCompletedTurn
            | SlashCommand::Rename
            | SlashCommand::Export
            | SlashCommand::Model
            | SlashCommand::Personality
            | SlashCommand::Plan
            | SlashCommand::Collab
            | SlashCommand::Agent
            | SlashCommand::Approvals
            | SlashCommand::Permissions
            | SlashCommand::ElevateSandbox
            | SlashCommand::Experimental
            | SlashCommand::Copy
            | SlashCommand::Mention
            | SlashCommand::CopyCodeBlock
            | SlashCommand::CopyMessage
            | SlashCommand::Skills
            | SlashCommand::Status
            | SlashCommand::DebugConfig
            | SlashCommand::Statusline
            | SlashCommand::Legend
            | SlashCommand::Mcp
            | SlashCommand::Apps
            | SlashCommand::Queue
            | SlashCommand::Logout
            | SlashCommand::Quit
            | SlashCommand::Exit
            | SlashCommand::Rollout
            | SlashCommand::Ps
            | SlashCommand::TestApproval => {
                self.submit_queued_user_message(queued);
                QueueDrain::Stop
            }
        }
    }

    fn find_queued_builtin_command(&self, name: &str) -> Option<SlashCommand> {
        #[cfg(target_os = "windows")]
        let allow_elevate_sandbox = {
            let windows_sandbox_level = WindowsSandboxLevel::from_config(&self.config);
            matches!(windows_sandbox_level, WindowsSandboxLevel::RestrictedToken)
        };
        #[cfg(not(target_os = "windows"))]
        let allow_elevate_sandbox = false;

        find_builtin_command(
            name,
            self.collaboration_modes_enabled(),
            self.connectors_enabled(),
            self.config.features.enabled(Feature::Personality),
            allow_elevate_sandbox,
        )
    }

    fn queued_command_drain_result(&self, cmd: SlashCommand) -> QueueDrain {
        if self.is_user_turn_pending_or_running() || !self.bottom_pane.no_modal_or_popup_active() {
            return QueueDrain::Stop;
        }

        match cmd {
            SlashCommand::Status
            | SlashCommand::DebugConfig
            | SlashCommand::LegendMode
            | SlashCommand::Ps
            | SlashCommand::Mcp
            | SlashCommand::Apps
            | SlashCommand::Queue
            | SlashCommand::Rollout
            | SlashCommand::Copy
            | SlashCommand::Diff
            | SlashCommand::Rename
            | SlashCommand::Pause
            | SlashCommand::Continue
            | SlashCommand::TestApproval => QueueDrain::Continue,
            SlashCommand::Feedback
            | SlashCommand::New
            | SlashCommand::Resume
            | SlashCommand::Session
            | SlashCommand::Archived
            | SlashCommand::Fork
            | SlashCommand::Init
            | SlashCommand::Compact
            | SlashCommand::Review
            | SlashCommand::ReviewCompletedTurn
            | SlashCommand::Export
            | SlashCommand::Model
            | SlashCommand::Personality
            | SlashCommand::Plan
            | SlashCommand::Collab
            | SlashCommand::Agent
            | SlashCommand::Approvals
            | SlashCommand::Permissions
            | SlashCommand::ElevateSandbox
            | SlashCommand::Experimental
            | SlashCommand::Mention
            | SlashCommand::CopyCodeBlock
            | SlashCommand::CopyMessage
            | SlashCommand::Skills
            | SlashCommand::Statusline
            | SlashCommand::Legend
            | SlashCommand::Logout
            | SlashCommand::Quit
            | SlashCommand::Exit => QueueDrain::Stop,
        }
    }

    fn slash_command_args_elements(
        rest: &str,
        rest_offset: usize,
        text_elements: &[TextElement],
    ) -> Vec<TextElement> {
        if rest.is_empty() || text_elements.is_empty() {
            return Vec::new();
        }
        text_elements
            .iter()
            .filter_map(|elem| {
                if elem.byte_range.end <= rest_offset {
                    return None;
                }
                let start = elem.byte_range.start.saturating_sub(rest_offset);
                let mut end = elem.byte_range.end.saturating_sub(rest_offset);
                if start >= rest.len() {
                    return None;
                }
                end = end.min(rest.len());
                (start < end).then_some(
                    elem.map_range(|_| codex_protocol::user_input::ByteRange { start, end }),
                )
            })
            .collect()
    }

    pub(super) fn open_export_picker(&mut self) {
        if self.current_rollout_path.is_none() {
            self.add_info_message("Export is not available yet.".to_string(), None);
            return;
        }

        let items = vec![
            SelectionItem {
                name: "Markdown (.md)".to_string(),
                description: Some("Readable transcript for sharing.".to_string()),
                actions: vec![Box::new(|tx| {
                    tx.send(AppEvent::ExportChat {
                        format: Some(ChatExportFormat::Markdown),
                        overrides: ExportOverrides::default(),
                    });
                })],
                dismiss_on_select: true,
                ..Default::default()
            },
            SelectionItem {
                name: "Markdown (.md) in current dir".to_string(),
                description: Some("Creates a file in the current directory.".to_string()),
                actions: vec![Box::new(|tx| {
                    tx.send(AppEvent::ExportChat {
                        format: Some(ChatExportFormat::Markdown),
                        overrides: ExportOverrides {
                            output_dir: Some(PathBuf::from(".")),
                            ..Default::default()
                        },
                    });
                })],
                dismiss_on_select: true,
                ..Default::default()
            },
            SelectionItem {
                name: "Markdown (custom path...)".to_string(),
                description: Some("Choose a destination path.".to_string()),
                actions: vec![Box::new(|tx| {
                    tx.send(AppEvent::OpenExportPathPrompt {
                        format: ChatExportFormat::Markdown,
                    });
                })],
                dismiss_on_select: true,
                ..Default::default()
            },
            SelectionItem {
                name: "JSON (.json)".to_string(),
                description: Some("Structured messages for tooling.".to_string()),
                actions: vec![Box::new(|tx| {
                    tx.send(AppEvent::ExportChat {
                        format: Some(ChatExportFormat::Json),
                        overrides: ExportOverrides::default(),
                    });
                })],
                dismiss_on_select: true,
                ..Default::default()
            },
            SelectionItem {
                name: "JSON (.json) in current dir".to_string(),
                description: Some("Creates a file in the current directory.".to_string()),
                actions: vec![Box::new(|tx| {
                    tx.send(AppEvent::ExportChat {
                        format: Some(ChatExportFormat::Json),
                        overrides: ExportOverrides {
                            output_dir: Some(PathBuf::from(".")),
                            ..Default::default()
                        },
                    });
                })],
                dismiss_on_select: true,
                ..Default::default()
            },
            SelectionItem {
                name: "JSON (custom path...)".to_string(),
                description: Some("Choose a destination path.".to_string()),
                actions: vec![Box::new(|tx| {
                    tx.send(AppEvent::OpenExportPathPrompt {
                        format: ChatExportFormat::Json,
                    });
                })],
                dismiss_on_select: true,
                ..Default::default()
            },
        ];

        self.bottom_pane.show_selection_view(SelectionViewParams {
            title: Some("Export Chat".to_string()),
            subtitle: Some("Creates a file next to the rollout (.jsonl).".to_string()),
            footer_hint: Some(standard_popup_hint_line()),
            items,
            ..Default::default()
        });
        self.request_redraw();
    }

    pub(crate) fn open_export_path_prompt(&mut self, format: ChatExportFormat) {
        let Some(rollout_path) = self.current_rollout_path.clone() else {
            self.add_info_message("Export is not available yet.".to_string(), None);
            return;
        };

        let default_path = match resolve_export_destination(
            &rollout_path,
            Some(format),
            &ExportOverrides::default(),
        ) {
            Ok(destination) => destination.path,
            Err(message) => {
                self.add_error_message(message);
                return;
            }
        };

        let placeholder = format!("Path (default: {})", default_path.display());
        let context_label = Some(format!("Format: {}", format.label()));
        let tx = self.app_event_tx.clone();
        let cwd = self.config.cwd.clone();
        let view = CustomPromptView::new(
            "Export path".to_string(),
            placeholder,
            context_label,
            Box::new(move |input: String| {
                let trimmed = input.trim();
                let overrides = if trimmed.is_empty() {
                    ExportOverrides::default()
                } else {
                    export_overrides_from_path_input(trimmed, &cwd)
                };
                tx.send(AppEvent::ExportChat {
                    format: Some(format),
                    overrides,
                });
            }),
        );
        self.bottom_pane.show_view(Box::new(view));
        self.request_redraw();
    }

    pub(crate) fn start_export(
        &mut self,
        format: Option<ChatExportFormat>,
        overrides: ExportOverrides,
    ) {
        let Some(rollout_path) = self.current_rollout_path.clone() else {
            self.add_info_message("Export is not available yet.".to_string(), None);
            return;
        };

        let destination = match resolve_export_destination(&rollout_path, format, &overrides) {
            Ok(destination) => destination,
            Err(message) => {
                self.add_error_message(message);
                return;
            }
        };

        let out_path = destination.path;
        let format = destination.format;
        let tx = self.app_event_tx.clone();
        tokio::spawn(async move {
            let result = async {
                if let Some(parent) = out_path.parent() {
                    tokio::fs::create_dir_all(parent).await?;
                }
                match format {
                    ChatExportFormat::Markdown => {
                        export_markdown::export_rollout_as_markdown(&rollout_path, &out_path).await
                    }
                    ChatExportFormat::Json => {
                        export_markdown::export_rollout_as_json(&rollout_path, &out_path).await
                    }
                }
            }
            .await;

            match result {
                Ok(messages) => tx.send(AppEvent::ExportResult {
                    path: out_path,
                    messages,
                    error: None,
                    format,
                }),
                Err(error) => tx.send(AppEvent::ExportResult {
                    path: out_path,
                    messages: 0,
                    error: Some(error.to_string()),
                    format,
                }),
            };
        });
    }
}
