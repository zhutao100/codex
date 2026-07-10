//! Key routing, paste, rename, and copy UI behavior for ChatWidget.

use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CopyableRole {
    Response,
    User,
}

impl CopyableRole {
    pub(super) fn label(self) -> &'static str {
        match self {
            CopyableRole::Response => "Response",
            CopyableRole::User => "User",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct CopyableMessage {
    pub(super) role: CopyableRole,
    pub(super) text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CodeBlockScope {
    LastResponse,
    AllResponses,
}

impl CodeBlockScope {
    pub(super) fn label(self) -> &'static str {
        match self {
            CodeBlockScope::LastResponse => "Last response",
            CodeBlockScope::AllResponses => "All responses",
        }
    }

    pub(super) fn description(self) -> &'static str {
        match self {
            CodeBlockScope::LastResponse => "Show blocks from the latest response only.",
            CodeBlockScope::AllResponses => "Show blocks from all responses in this chat.",
        }
    }

    pub(super) fn toggle(self) -> Self {
        match self {
            CodeBlockScope::LastResponse => CodeBlockScope::AllResponses,
            CodeBlockScope::AllResponses => CodeBlockScope::LastResponse,
        }
    }
}

impl From<CopyCodeBlockScope> for CodeBlockScope {
    fn from(value: CopyCodeBlockScope) -> Self {
        match value {
            CopyCodeBlockScope::LastResponse => CodeBlockScope::LastResponse,
            CopyCodeBlockScope::AllResponses => CodeBlockScope::AllResponses,
        }
    }
}

impl From<CodeBlockScope> for CopyCodeBlockScope {
    fn from(value: CodeBlockScope) -> Self {
        match value {
            CodeBlockScope::LastResponse => CopyCodeBlockScope::LastResponse,
            CodeBlockScope::AllResponses => CopyCodeBlockScope::AllResponses,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MessageFilter {
    Responses,
    User,
    Both,
}

impl MessageFilter {
    pub(super) fn label(self) -> &'static str {
        match self {
            MessageFilter::Responses => "Responses",
            MessageFilter::User => "User messages",
            MessageFilter::Both => "Responses + user messages",
        }
    }

    pub(super) fn description(self) -> &'static str {
        match self {
            MessageFilter::Responses => "Only assistant responses",
            MessageFilter::User => "Only your messages",
            MessageFilter::Both => "Both response and user messages",
        }
    }

    pub(super) fn next(self) -> Self {
        match self {
            MessageFilter::Responses => MessageFilter::User,
            MessageFilter::User => MessageFilter::Both,
            MessageFilter::Both => MessageFilter::Responses,
        }
    }

    pub(super) fn includes(self, role: CopyableRole) -> bool {
        match self {
            MessageFilter::Responses => role == CopyableRole::Response,
            MessageFilter::User => role == CopyableRole::User,
            MessageFilter::Both => true,
        }
    }
}

impl From<CopyMessageFilter> for MessageFilter {
    fn from(value: CopyMessageFilter) -> Self {
        match value {
            CopyMessageFilter::Responses => MessageFilter::Responses,
            CopyMessageFilter::User => MessageFilter::User,
            CopyMessageFilter::Both => MessageFilter::Both,
        }
    }
}

impl From<MessageFilter> for CopyMessageFilter {
    fn from(value: MessageFilter) -> Self {
        match value {
            MessageFilter::Responses => CopyMessageFilter::Responses,
            MessageFilter::User => CopyMessageFilter::User,
            MessageFilter::Both => CopyMessageFilter::Both,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct CopyCodeBlockCandidate {
    pub(super) id: String,
    pub(super) label: String,
    pub(super) preview: String,
    pub(super) search_value: String,
    pub(super) content: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct CopyMessageCandidate {
    pub(super) id: String,
    pub(super) label: String,
    pub(super) preview: String,
    pub(super) search_value: String,
    pub(super) content: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct CopyCodeUiState {
    pub(super) scope: CodeBlockScope,
    pub(super) ui_mode: CopyUiMode,
    pub(super) multi_select: bool,
    pub(super) selected_id: Option<String>,
    pub(super) selected_ids: BTreeSet<String>,
}

impl CopyCodeUiState {
    pub(super) fn new(scope: CodeBlockScope, ui_mode: CopyUiMode) -> Self {
        Self {
            scope,
            ui_mode,
            multi_select: false,
            selected_id: None,
            selected_ids: BTreeSet::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct CopyMessageUiState {
    pub(super) filter: MessageFilter,
    pub(super) ui_mode: CopyUiMode,
    pub(super) multi_select: bool,
    pub(super) selected_id: Option<String>,
    pub(super) selected_ids: BTreeSet<String>,
}

impl CopyMessageUiState {
    pub(super) fn new(filter: MessageFilter, ui_mode: CopyUiMode) -> Self {
        Self {
            filter,
            ui_mode,
            multi_select: false,
            selected_id: None,
            selected_ids: BTreeSet::new(),
        }
    }
}

pub(super) fn copy_ui_mode_label(mode: CopyUiMode) -> &'static str {
    match mode {
        CopyUiMode::Picker => "picker",
        CopyUiMode::Navigator => "navigator",
    }
}

pub(super) fn selected_index_for_candidates<T>(
    top_rows: usize,
    candidates: &[T],
    selected_id: Option<&str>,
) -> Option<usize>
where
    T: CopyCandidateId,
{
    selected_id.and_then(|id| {
        candidates
            .iter()
            .position(|candidate| candidate.candidate_id() == id)
            .map(|idx| top_rows + idx)
    })
}

pub(super) trait CopyCandidateId {
    fn candidate_id(&self) -> &str;
}

impl CopyCandidateId for CopyCodeBlockCandidate {
    fn candidate_id(&self) -> &str {
        &self.id
    }
}

impl CopyCandidateId for CopyMessageCandidate {
    fn candidate_id(&self) -> &str {
        &self.id
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct FencedCodeBlock {
    pub(super) language: Option<String>,
    pub(super) content: String,
}

pub(super) fn first_non_empty_preview(content: &str) -> String {
    let snippet = content
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("")
        .trim();
    if snippet.is_empty() {
        "(empty)".to_string()
    } else {
        truncate_text(snippet, 80)
    }
}

pub(super) fn extract_fenced_code_blocks(markdown: &str) -> Vec<FencedCodeBlock> {
    #[derive(Debug)]
    struct OpenFence {
        fence_char: char,
        fence_len: usize,
        language: Option<String>,
        content_lines: Vec<String>,
    }

    fn parse_fence(line: &str) -> Option<(char, usize, &str)> {
        let trimmed = line.trim_start();
        let fence_char = match trimmed.chars().next() {
            Some('`') => '`',
            Some('~') => '~',
            _ => return None,
        };
        let fence_len = trimmed.chars().take_while(|ch| *ch == fence_char).count();
        if fence_len < 3 {
            return None;
        }
        Some((fence_char, fence_len, trimmed[fence_len..].trim()))
    }

    let mut blocks = Vec::new();
    let mut open: Option<OpenFence> = None;

    for line in markdown.lines() {
        if let Some(state) = open.as_mut() {
            let trimmed = line.trim_start();
            let close_len = trimmed
                .chars()
                .take_while(|ch| *ch == state.fence_char)
                .count();
            if close_len >= state.fence_len && trimmed[close_len..].trim().is_empty() {
                if let Some(state) = open.take() {
                    blocks.push(FencedCodeBlock {
                        language: state.language,
                        content: state.content_lines.join("\n"),
                    });
                }
                continue;
            }
            state.content_lines.push(line.to_string());
            continue;
        }

        let Some((fence_char, fence_len, rest)) = parse_fence(line) else {
            continue;
        };
        let language = rest.split_whitespace().next().map(ToString::to_string);
        open = Some(OpenFence {
            fence_char,
            fence_len,
            language,
            content_lines: Vec::new(),
        });
    }

    blocks
}

impl ChatWidget {
    pub(crate) fn handle_key_event(&mut self, key_event: KeyEvent) {
        match key_event {
            KeyEvent {
                code: KeyCode::Char(c),
                modifiers,
                kind: KeyEventKind::Press,
                ..
            } if modifiers.contains(KeyModifiers::CONTROL) && c.eq_ignore_ascii_case(&'c') => {
                self.on_ctrl_c();
                return;
            }
            KeyEvent {
                code: KeyCode::Char(c),
                modifiers,
                kind: KeyEventKind::Press,
                ..
            } if modifiers.contains(KeyModifiers::CONTROL) && c.eq_ignore_ascii_case(&'d') => {
                if self.on_ctrl_d() {
                    return;
                }
                self.bottom_pane.clear_quit_shortcut_hint();
                self.quit_shortcut_expires_at = None;
                self.quit_shortcut_key = None;
            }
            key_event
                if key_event.kind == KeyEventKind::Press
                    && self
                        .keybindings
                        .paste
                        .iter()
                        .any(|binding| binding.matches(&key_event)) =>
            {
                self.paste_from_clipboard();
                return;
            }
            key_event
                if key_event.kind == KeyEventKind::Press
                    && self
                        .keybindings
                        .copy_prompt
                        .iter()
                        .any(|binding| binding.matches(&key_event)) =>
            {
                self.copy_prompt_to_clipboard();
                return;
            }
            key_event
                if key_event.kind == KeyEventKind::Press
                    && self
                        .keybindings
                        .copy_last_output
                        .iter()
                        .any(|binding| binding.matches(&key_event)) =>
            {
                self.copy_last_output_to_clipboard();
                return;
            }
            key_event
                if key_event.kind == KeyEventKind::Press
                    && self
                        .keybindings
                        .copy_code_block
                        .iter()
                        .any(|binding| binding.matches(&key_event)) =>
            {
                self.open_copy_code_block_picker();
                return;
            }
            other if other.kind == KeyEventKind::Press => {
                self.bottom_pane.clear_quit_shortcut_hint();
                self.quit_shortcut_expires_at = None;
                self.quit_shortcut_key = None;
            }
            _ => {}
        }

        if self.queued_edit_state.is_some()
            && self.bottom_pane.no_modal_or_popup_active()
            && self.handle_queue_edit_key_event(key_event)
        {
            return;
        }

        match key_event {
            KeyEvent {
                code: KeyCode::Char('o' | 'O'),
                modifiers: KeyModifiers::CONTROL,
                kind: KeyEventKind::Press,
                ..
            }
            | KeyEvent {
                code: KeyCode::Char('\u{000f}'),
                modifiers: KeyModifiers::NONE,
                kind: KeyEventKind::Press,
                ..
            } if self.bottom_pane.no_modal_or_popup_active()
                && !self.queued_user_messages.is_empty()
                && self.queued_edit_state.is_none() =>
            {
                self.open_queue_popup();
            }
            KeyEvent {
                code: KeyCode::Char('y' | 'Y'),
                modifiers: KeyModifiers::CONTROL,
                kind: KeyEventKind::Press,
                ..
            }
            | KeyEvent {
                code: KeyCode::Char('\u{0019}'),
                modifiers: KeyModifiers::NONE,
                kind: KeyEventKind::Press,
                ..
            } if self.bottom_pane.no_modal_or_popup_active()
                && !self.is_user_turn_pending_or_running()
                && !self.queued_user_messages.is_empty()
                && self.queued_edit_state.is_none() =>
            {
                self.send_next_queued_user_message();
            }
            KeyEvent {
                code: KeyCode::Right,
                modifiers,
                kind: KeyEventKind::Press | KeyEventKind::Repeat,
                ..
            } if modifiers == (KeyModifiers::CONTROL | KeyModifiers::SHIFT)
                && self.bottom_pane.no_modal_or_popup_active() =>
            {
                self.cycle_model_shortcut(1);
            }
            KeyEvent {
                code: KeyCode::Left,
                modifiers,
                kind: KeyEventKind::Press | KeyEventKind::Repeat,
                ..
            } if modifiers == (KeyModifiers::CONTROL | KeyModifiers::SHIFT)
                && self.bottom_pane.no_modal_or_popup_active() =>
            {
                self.cycle_model_shortcut(-1);
            }
            KeyEvent {
                code: KeyCode::Down,
                modifiers,
                kind: KeyEventKind::Press | KeyEventKind::Repeat,
                ..
            } if modifiers == (KeyModifiers::CONTROL | KeyModifiers::SHIFT)
                && self.bottom_pane.no_modal_or_popup_active() =>
            {
                self.cycle_reasoning_effort_shortcut(-1);
            }
            KeyEvent {
                code: KeyCode::Up,
                modifiers,
                kind: KeyEventKind::Press | KeyEventKind::Repeat,
                ..
            } if modifiers == (KeyModifiers::CONTROL | KeyModifiers::SHIFT)
                && self.bottom_pane.no_modal_or_popup_active() =>
            {
                self.cycle_reasoning_effort_shortcut(1);
            }
            KeyEvent {
                code: KeyCode::BackTab,
                kind: KeyEventKind::Press,
                ..
            } if self.collaboration_modes_enabled()
                && !self.bottom_pane.is_task_running()
                && self.bottom_pane.no_modal_or_popup_active() =>
            {
                self.cycle_collaboration_mode();
            }
            KeyEvent {
                code: KeyCode::Char('q' | 'Q'),
                modifiers: KeyModifiers::ALT,
                kind: KeyEventKind::Press,
                ..
            } if !self.queued_user_messages.is_empty() && self.queued_edit_state.is_none() => {
                self.open_queue_popup();
            }
            KeyEvent {
                code: KeyCode::Up,
                modifiers: KeyModifiers::ALT,
                kind: KeyEventKind::Press,
                ..
            } if !self.queued_user_messages.is_empty() => {
                self.begin_queue_edit_most_recent();
            }
            _ => match self.bottom_pane.handle_key_event(key_event) {
                InputResult::Submitted {
                    text,
                    text_elements,
                } => {
                    let user_message = UserMessage {
                        text,
                        local_images: self
                            .bottom_pane
                            .take_recent_submission_images_with_placeholders(),
                        text_elements,
                        mention_paths: self.bottom_pane.take_mention_paths(),
                    };
                    let should_submit_now =
                        self.is_session_configured() && !self.is_plan_streaming_in_tui();
                    if should_submit_now {
                        if self.only_user_shell_commands_running()
                            && !user_message.text.starts_with('!')
                        {
                            self.queue_user_message(user_message);
                            return;
                        }
                        // Submitted is only emitted when steer is enabled (Enter sends immediately).
                        // Reset any reasoning header only when we are actually submitting a turn.
                        self.reasoning_buffer.clear();
                        self.reasoning_summary_parts.clear();
                        self.set_status_header(String::from("Working"));
                        self.submit_user_message(user_message);
                    } else {
                        self.queue_user_message(user_message);
                    }
                }
                InputResult::Queued {
                    text,
                    text_elements,
                    action,
                    pending_pastes,
                } => {
                    let user_message = UserMessage {
                        text,
                        local_images: self
                            .bottom_pane
                            .take_recent_submission_images_with_placeholders(),
                        text_elements,
                        mention_paths: self.bottom_pane.take_mention_paths(),
                    };
                    self.queue_user_message_with_action(user_message, action, pending_pastes);
                }
                InputResult::Command(cmd) => {
                    self.dispatch_command(cmd);
                }
                InputResult::CommandWithArgs(cmd, args, text_elements) => {
                    self.dispatch_command_with_args(cmd, args, text_elements);
                }
                InputResult::None => {}
            },
        }
    }

    /// Attach a local image to the composer when the active model supports image inputs.
    ///
    /// When the model does not advertise image support, we keep the draft unchanged and surface a
    /// warning event so users can switch models or remove attachments.
    pub(crate) fn attach_image(&mut self, path: PathBuf) {
        if !self.current_model_supports_images() {
            self.add_to_history(history_cell::new_warning_event(
                self.image_inputs_not_supported_message(),
            ));
            self.request_redraw();
            return;
        }
        tracing::info!("attach_image path={path:?}");
        self.bottom_pane.attach_image(path);
        self.request_redraw();
    }

    pub(crate) fn composer_text_with_pending(&self) -> String {
        self.bottom_pane.composer_text_with_pending()
    }

    pub(crate) fn apply_external_edit(&mut self, text: String) {
        self.bottom_pane.apply_external_edit(text);
        self.request_redraw();
    }

    pub(crate) fn external_editor_state(&self) -> ExternalEditorState {
        self.external_editor_state
    }

    pub(crate) fn set_external_editor_state(&mut self, state: ExternalEditorState) {
        self.external_editor_state = state;
    }

    pub(crate) fn set_footer_hint_override(&mut self, items: Option<Vec<(String, String)>>) {
        self.bottom_pane.set_footer_hint_override(items);
    }

    pub(crate) fn show_selection_view(&mut self, params: SelectionViewParams) {
        self.bottom_pane.show_selection_view(params);
        self.request_redraw();
    }

    pub(crate) fn can_launch_external_editor(&self) -> bool {
        self.bottom_pane.can_launch_external_editor()
    }

    pub(super) fn open_rename_thread_view(&mut self) {
        let mut items = Vec::new();
        items.push(SelectionItem {
            name: "Rename manually".to_string(),
            description: Some("Set a custom name.".to_string()),
            actions: vec![Box::new(|tx| {
                tx.send(AppEvent::OpenRenameThreadPrompt);
            })],
            dismiss_on_select: true,
            ..Default::default()
        });

        if self.has_completed_assistant_message {
            items.push(SelectionItem {
                name: "Generate thread name".to_string(),
                description: Some("Generate a name automatically.".to_string()),
                actions: vec![Box::new(|tx| {
                    tx.send(AppEvent::CodexOp(Op::AutoRenameThread));
                })],
                dismiss_on_select: true,
                ..Default::default()
            });
        }

        self.bottom_pane.show_selection_view(SelectionViewParams {
            title: Some("Rename thread".to_string()),
            footer_hint: Some(standard_popup_hint_line()),
            items,
            ..Default::default()
        });
        self.request_redraw();
    }

    pub(crate) fn show_rename_prompt(&mut self) {
        let tx = self.app_event_tx.clone();
        let has_name = self
            .thread_name
            .as_ref()
            .is_some_and(|name| !name.is_empty());
        let title = if has_name {
            "Rename thread"
        } else {
            "Name thread"
        };
        let view = CustomPromptView::new(
            title.to_string(),
            "Type a name and press Enter".to_string(),
            None,
            Box::new(move |name: String| {
                let Some(name) = codex_core::util::normalize_thread_name(&name) else {
                    tx.send(AppEvent::InsertHistoryCell(Box::new(
                        history_cell::new_error_event("Thread name cannot be empty.".to_string()),
                    )));
                    return;
                };
                tx.send(AppEvent::CodexOp(Op::SetThreadName { name }));
            }),
        );

        self.bottom_pane.show_view(Box::new(view));
    }

    pub(crate) fn handle_paste(&mut self, text: String) {
        if text.is_empty() {
            // Some terminals (like VS Code) route Cmd+V through terminal paste, which can
            // yield an empty payload for images. Fall back to reading the clipboard.
            self.paste_from_clipboard();
            return;
        }
        self.bottom_pane.handle_paste(text);
    }

    pub(crate) fn handle_paste_burst_tick(&mut self, frame_requester: FrameRequester) -> bool {
        if self.bottom_pane.flush_paste_burst_if_due() {
            // A paste just flushed; request an immediate redraw and skip this frame.
            self.request_redraw();
            true
        } else if self.bottom_pane.is_in_paste_burst() {
            // While capturing a burst, schedule a follow-up tick and skip this frame
            // to avoid redundant renders between ticks.
            frame_requester.schedule_frame_in(
                crate::bottom_pane::ChatComposer::recommended_paste_flush_delay(),
            );
            true
        } else {
            false
        }
    }

    pub(super) fn paste_from_clipboard(&mut self) {
        let active_view = !self.bottom_pane.no_modal_or_popup_active();

        let mut image_error: Option<PasteImageError> = None;
        if !active_view {
            match paste_image_to_temp_png() {
                Ok((path, info)) => {
                    tracing::debug!(
                        "pasted image size={}x{} format={}",
                        info.width,
                        info.height,
                        info.encoded_format.label()
                    );
                    self.attach_image(path);
                    return;
                }
                Err(err) => {
                    image_error = Some(err);
                }
            }
        }

        match paste_text_from_clipboard() {
            Ok(text) => {
                self.bottom_pane.handle_paste(text);
            }
            Err(err) => {
                let message = if let Some(img_err) = image_error {
                    format!("Failed to paste from clipboard: {img_err}; {err}")
                } else {
                    format!("Failed to paste from clipboard: {err}")
                };
                self.add_to_history(history_cell::new_error_event(message));
                self.request_redraw();
            }
        }
    }

    pub(super) fn copy_prompt_to_clipboard(&mut self) {
        if !self.bottom_pane.no_modal_or_popup_active() {
            return;
        }

        let text = self.bottom_pane.composer_text();
        if text.trim().is_empty() {
            return;
        }

        if let Err(err) = copy_text_to_clipboard(&text) {
            self.add_to_history(history_cell::new_error_event(format!(
                "Failed to copy prompt to clipboard: {err}",
            )));
            self.request_redraw();
        }
    }

    pub(super) fn copy_last_output_to_clipboard(&mut self) {
        if !self.bottom_pane.no_modal_or_popup_active() {
            return;
        }

        let Some(markdown) = self
            .last_assistant_output_markdown
            .as_deref()
            .map(str::trim)
            .filter(|text| !text.is_empty())
        else {
            self.add_to_history(history_cell::new_info_event(
                "No output to copy.".to_string(),
                None,
            ));
            self.request_redraw();
            return;
        };

        if let Err(err) = copy_text_to_clipboard(markdown) {
            self.add_to_history(history_cell::new_error_event(format!(
                "Failed to copy last output to clipboard: {err}",
            )));
            self.request_redraw();
            return;
        }

        self.add_to_history(history_cell::new_info_event(
            "Copied last output to clipboard.".to_string(),
            None,
        ));
        self.request_redraw();
    }

    pub(super) fn open_copy_code_block_picker(&mut self) {
        if !self.bottom_pane.no_modal_or_popup_active() {
            return;
        }
        self.open_copy_code_block_picker_with_scope(CopyCodeBlockScope::LastResponse);
    }

    pub(crate) fn open_copy_code_block_picker_with_scope(&mut self, scope: CopyCodeBlockScope) {
        let scope = scope.into();
        self.copy_code_ui_state = Some(CopyCodeUiState::new(
            scope,
            self.config.tui_copy_code_ui_mode,
        ));
        self.show_copy_code_block_view();
    }

    pub(crate) fn set_copy_code_block_scope(&mut self, scope: CopyCodeBlockScope) {
        let scope = scope.into();
        if let Some(state) = self.copy_code_ui_state.as_mut() {
            state.scope = scope;
            state.selected_id = None;
            state.selected_ids.clear();
        } else {
            self.copy_code_ui_state = Some(CopyCodeUiState::new(
                scope,
                self.config.tui_copy_code_ui_mode,
            ));
        }
        self.show_copy_code_block_view();
    }

    pub(crate) fn toggle_copy_code_block_ui_mode(&mut self) {
        if let Some(state) = self.copy_code_ui_state.as_mut() {
            state.ui_mode = match state.ui_mode {
                CopyUiMode::Picker => CopyUiMode::Navigator,
                CopyUiMode::Navigator => CopyUiMode::Picker,
            };
        }
        self.show_copy_code_block_view();
    }

    pub(crate) fn toggle_copy_code_block_multi_select_mode(&mut self) {
        if let Some(state) = self.copy_code_ui_state.as_mut() {
            state.multi_select = !state.multi_select;
            if !state.multi_select {
                state.selected_ids.clear();
            }
        }
        self.show_copy_code_block_view();
    }

    pub(crate) fn toggle_copy_code_block_selection(&mut self, id: String) {
        if let Some(state) = self.copy_code_ui_state.as_mut() {
            state.selected_id = Some(id.clone());
            if !state.selected_ids.insert(id.clone()) {
                state.selected_ids.remove(&id);
            }
        }
        self.show_copy_code_block_view();
    }

    pub(crate) fn copy_selected_code_blocks(&mut self) {
        let Some(state) = self.copy_code_ui_state.as_ref() else {
            return;
        };
        if state.selected_ids.is_empty() {
            self.add_to_history(history_cell::new_info_event(
                "No code blocks selected.".to_string(),
                None,
            ));
            self.request_redraw();
            return;
        }
        let selected = state.selected_ids.clone();
        let contents = self
            .code_block_candidates_for_scope(state.scope)
            .into_iter()
            .filter(|candidate| selected.contains(&candidate.id))
            .map(|candidate| candidate.content)
            .collect::<Vec<_>>();
        if contents.is_empty() {
            self.add_to_history(history_cell::new_info_event(
                "No code blocks selected.".to_string(),
                None,
            ));
            self.request_redraw();
            return;
        }
        self.copy_joined_items_to_clipboard(contents, "code block", "code blocks");
    }

    pub(super) fn show_copy_code_block_view(&mut self) {
        let Some(state) = self.copy_code_ui_state.clone() else {
            return;
        };
        let has_any_response = self
            .copyable_messages
            .iter()
            .any(|message| message.role == CopyableRole::Response);
        let candidates = self.code_block_candidates_for_scope(state.scope);
        if candidates.is_empty() && !has_any_response {
            let message = match state.scope {
                CodeBlockScope::LastResponse => {
                    if self.last_response_markdown().is_some() {
                        "No code blocks to copy."
                    } else {
                        "No output to scan for code blocks."
                    }
                }
                CodeBlockScope::AllResponses => "No response output to scan for code blocks.",
            };
            self.add_to_history(history_cell::new_info_event(message.to_string(), None));
            self.request_redraw();
            return;
        }

        let mut items = vec![
            SelectionItem {
                name: format!("View: {}", copy_ui_mode_label(state.ui_mode)),
                description: Some(
                    "Switch between picker and navigator (session only).".to_string(),
                ),
                actions: vec![Box::new(|tx| {
                    tx.send(AppEvent::ToggleCopyCodeBlockUiMode);
                })],
                dismiss_on_select: true,
                ..Default::default()
            },
            SelectionItem {
                name: format!("Scope: {}", state.scope.label()),
                description: Some(state.scope.description().to_string()),
                actions: vec![Box::new(move |tx| {
                    tx.send(AppEvent::SetCopyCodeBlockScope {
                        scope: state.scope.toggle().into(),
                    });
                })],
                dismiss_on_select: true,
                ..Default::default()
            },
            SelectionItem {
                name: format!(
                    "Selection: {}",
                    if state.multi_select {
                        "multi"
                    } else {
                        "single"
                    }
                ),
                description: Some("Toggle multi-select mode.".to_string()),
                actions: vec![Box::new(|tx| {
                    tx.send(AppEvent::ToggleCopyCodeBlockMultiSelect);
                })],
                dismiss_on_select: true,
                ..Default::default()
            },
        ];

        if state.multi_select {
            let selected_count = state.selected_ids.len();
            items.push(SelectionItem {
                name: format!("Copy selected ({selected_count})"),
                description: Some("Copy all selected code blocks.".to_string()),
                is_disabled: selected_count == 0,
                actions: vec![Box::new(|tx| {
                    tx.send(AppEvent::CopySelectedCodeBlocks);
                })],
                dismiss_on_select: true,
                ..Default::default()
            });
        }

        let top_rows = items.len();
        let has_candidates = !candidates.is_empty();
        if !has_candidates {
            items.push(SelectionItem {
                name: "No code blocks in this scope".to_string(),
                is_disabled: true,
                ..Default::default()
            });
        }

        for candidate in &candidates {
            let id = candidate.id.clone();
            let content = candidate.content.clone();
            let selected = state.selected_ids.contains(&id);
            let label = if state.multi_select {
                format!("[{}] {}", if selected { "x" } else { " " }, candidate.label)
            } else {
                candidate.label.clone()
            };
            items.push(SelectionItem {
                name: label,
                description: Some(candidate.preview.clone()),
                search_value: Some(candidate.search_value.clone()),
                actions: if state.multi_select {
                    vec![Box::new(move |tx| {
                        tx.send(AppEvent::ToggleCopyCodeBlockSelection { id: id.clone() });
                    })]
                } else {
                    vec![Box::new(move |tx| match copy_text_to_clipboard(&content) {
                        Ok(()) => tx.send(AppEvent::InsertHistoryCell(Box::new(
                            history_cell::new_info_event(
                                "Copied code block to clipboard.".to_string(),
                                None,
                            ),
                        ))),
                        Err(err) => tx.send(AppEvent::InsertHistoryCell(Box::new(
                            history_cell::new_error_event(format!(
                                "Failed to copy code block to clipboard: {err}",
                            )),
                        ))),
                    })]
                },
                dismiss_on_select: true,
                ..Default::default()
            });
        }

        let initial_selected_idx =
            selected_index_for_candidates(top_rows, &candidates, state.selected_id.as_deref())
                .or_else(|| has_candidates.then_some(top_rows))
                .or(Some(0));
        let (subtitle, searchable, search_placeholder) = match state.ui_mode {
            CopyUiMode::Picker => (
                format!("Choose a block to copy · {}", state.scope.label()),
                true,
                Some("Type to search code blocks".to_string()),
            ),
            CopyUiMode::Navigator => (
                format!("Navigate blocks in chat order · {}", state.scope.label()),
                false,
                None,
            ),
        };

        self.bottom_pane.show_selection_view(SelectionViewParams {
            title: Some("Copy code block".to_string()),
            subtitle: Some(subtitle),
            footer_hint: Some(standard_popup_hint_line()),
            is_searchable: searchable,
            search_placeholder,
            items,
            initial_selected_idx,
            ..Default::default()
        });
        self.request_redraw();
    }

    pub(crate) fn open_copy_message_picker(&mut self, filter: CopyMessageFilter) {
        let filter = filter.into();
        self.copy_message_ui_state = Some(CopyMessageUiState::new(
            filter,
            self.config.tui_copy_message_ui_mode,
        ));
        self.show_copy_message_view();
    }

    pub(crate) fn set_copy_message_filter(&mut self, filter: CopyMessageFilter) {
        let filter = filter.into();
        if let Some(state) = self.copy_message_ui_state.as_mut() {
            state.filter = filter;
            state.selected_id = None;
            state.selected_ids.clear();
        } else {
            self.copy_message_ui_state = Some(CopyMessageUiState::new(
                filter,
                self.config.tui_copy_message_ui_mode,
            ));
        }
        self.show_copy_message_view();
    }

    pub(crate) fn toggle_copy_message_ui_mode(&mut self) {
        if let Some(state) = self.copy_message_ui_state.as_mut() {
            state.ui_mode = match state.ui_mode {
                CopyUiMode::Picker => CopyUiMode::Navigator,
                CopyUiMode::Navigator => CopyUiMode::Picker,
            };
        }
        self.show_copy_message_view();
    }

    pub(crate) fn toggle_copy_message_multi_select_mode(&mut self) {
        if let Some(state) = self.copy_message_ui_state.as_mut() {
            state.multi_select = !state.multi_select;
            if !state.multi_select {
                state.selected_ids.clear();
            }
        }
        self.show_copy_message_view();
    }

    pub(crate) fn toggle_copy_message_selection(&mut self, id: String) {
        if let Some(state) = self.copy_message_ui_state.as_mut() {
            state.selected_id = Some(id.clone());
            if !state.selected_ids.insert(id.clone()) {
                state.selected_ids.remove(&id);
            }
        }
        self.show_copy_message_view();
    }

    pub(crate) fn copy_selected_messages(&mut self) {
        let Some(state) = self.copy_message_ui_state.as_ref() else {
            return;
        };
        if state.selected_ids.is_empty() {
            self.add_to_history(history_cell::new_info_event(
                "No messages selected.".to_string(),
                None,
            ));
            self.request_redraw();
            return;
        }
        let selected = state.selected_ids.clone();
        let contents = self
            .message_candidates_for_filter(state.filter)
            .into_iter()
            .filter(|candidate| selected.contains(&candidate.id))
            .map(|candidate| candidate.content)
            .collect::<Vec<_>>();
        if contents.is_empty() {
            self.add_to_history(history_cell::new_info_event(
                "No messages selected.".to_string(),
                None,
            ));
            self.request_redraw();
            return;
        }
        self.copy_joined_items_to_clipboard(contents, "message", "messages");
    }

    pub(super) fn show_copy_message_view(&mut self) {
        if self.copyable_messages.is_empty() {
            self.add_to_history(history_cell::new_info_event(
                "No messages to copy.".to_string(),
                None,
            ));
            self.request_redraw();
            return;
        }
        let Some(state) = self.copy_message_ui_state.clone() else {
            return;
        };
        let candidates = self.message_candidates_for_filter(state.filter);
        let mut items = vec![
            SelectionItem {
                name: format!("View: {}", copy_ui_mode_label(state.ui_mode)),
                description: Some(
                    "Switch between picker and navigator (session only).".to_string(),
                ),
                actions: vec![Box::new(|tx| {
                    tx.send(AppEvent::ToggleCopyMessageUiMode);
                })],
                dismiss_on_select: true,
                ..Default::default()
            },
            SelectionItem {
                name: format!("Filter: {}", state.filter.label()),
                description: Some(state.filter.description().to_string()),
                actions: vec![Box::new(move |tx| {
                    tx.send(AppEvent::SetCopyMessageFilter {
                        filter: state.filter.next().into(),
                    });
                })],
                dismiss_on_select: true,
                ..Default::default()
            },
            SelectionItem {
                name: format!(
                    "Selection: {}",
                    if state.multi_select {
                        "multi"
                    } else {
                        "single"
                    }
                ),
                description: Some("Toggle multi-select mode.".to_string()),
                actions: vec![Box::new(|tx| {
                    tx.send(AppEvent::ToggleCopyMessageMultiSelect);
                })],
                dismiss_on_select: true,
                ..Default::default()
            },
        ];
        if state.multi_select {
            let selected_count = state.selected_ids.len();
            items.push(SelectionItem {
                name: format!("Copy selected ({selected_count})"),
                description: Some("Copy all selected messages.".to_string()),
                is_disabled: selected_count == 0,
                actions: vec![Box::new(|tx| {
                    tx.send(AppEvent::CopySelectedMessages);
                })],
                dismiss_on_select: true,
                ..Default::default()
            });
        }

        let top_rows = items.len();
        let has_candidates = !candidates.is_empty();
        if !has_candidates {
            items.push(SelectionItem {
                name: "No messages in this filter".to_string(),
                is_disabled: true,
                ..Default::default()
            });
        }
        for candidate in &candidates {
            let id = candidate.id.clone();
            let content = candidate.content.clone();
            let selected = state.selected_ids.contains(&id);
            let label = if state.multi_select {
                format!("[{}] {}", if selected { "x" } else { " " }, candidate.label)
            } else {
                candidate.label.clone()
            };
            items.push(SelectionItem {
                name: label,
                description: Some(candidate.preview.clone()),
                search_value: Some(candidate.search_value.clone()),
                actions: if state.multi_select {
                    vec![Box::new(move |tx| {
                        tx.send(AppEvent::ToggleCopyMessageSelection { id: id.clone() });
                    })]
                } else {
                    vec![Box::new(move |tx| match copy_text_to_clipboard(&content) {
                        Ok(()) => tx.send(AppEvent::InsertHistoryCell(Box::new(
                            history_cell::new_info_event(
                                "Copied message to clipboard.".to_string(),
                                None,
                            ),
                        ))),
                        Err(err) => tx.send(AppEvent::InsertHistoryCell(Box::new(
                            history_cell::new_error_event(format!(
                                "Failed to copy message to clipboard: {err}",
                            )),
                        ))),
                    })]
                },
                dismiss_on_select: true,
                ..Default::default()
            });
        }

        let initial_selected_idx =
            selected_index_for_candidates(top_rows, &candidates, state.selected_id.as_deref())
                .or_else(|| has_candidates.then_some(top_rows))
                .or(Some(0));
        let (subtitle, searchable, search_placeholder) = match state.ui_mode {
            CopyUiMode::Picker => (
                format!("Choose a message to copy · {}", state.filter.label()),
                true,
                Some("Type to search messages".to_string()),
            ),
            CopyUiMode::Navigator => (
                format!("Navigate messages in chat order · {}", state.filter.label()),
                false,
                None,
            ),
        };
        self.bottom_pane.show_selection_view(SelectionViewParams {
            title: Some("Copy message".to_string()),
            subtitle: Some(subtitle),
            footer_hint: Some(standard_popup_hint_line()),
            items,
            is_searchable: searchable,
            search_placeholder,
            initial_selected_idx,
            ..Default::default()
        });
        self.request_redraw();
    }

    pub(super) fn copy_joined_items_to_clipboard(
        &mut self,
        contents: Vec<String>,
        singular: &str,
        plural: &str,
    ) {
        let text = contents.join("\n\n");
        match copy_text_to_clipboard(&text) {
            Ok(()) => self.add_to_history(history_cell::new_info_event(
                format!(
                    "Copied {} {} to clipboard.",
                    contents.len(),
                    if contents.len() == 1 {
                        singular
                    } else {
                        plural
                    }
                ),
                None,
            )),
            Err(err) => self.add_to_history(history_cell::new_error_event(format!(
                "Failed to copy selected {plural} to clipboard: {err}",
            ))),
        }
        self.request_redraw();
    }

    pub(super) fn message_candidates_for_filter(
        &self,
        filter: MessageFilter,
    ) -> Vec<CopyMessageCandidate> {
        self.copyable_messages
            .iter()
            .enumerate()
            .rev()
            .filter(|(_, message)| filter.includes(message.role))
            .enumerate()
            .map(|(display_idx, (message_idx, message))| {
                let snippet = message
                    .text
                    .lines()
                    .find(|line| !line.trim().is_empty())
                    .unwrap_or("")
                    .trim();
                let preview = if snippet.is_empty() {
                    "(empty message)".to_string()
                } else {
                    truncate_text(snippet, 80)
                };
                let role_label = message.role.label();
                let content = message.text.clone();
                CopyMessageCandidate {
                    id: format!("msg-{message_idx}"),
                    label: format!("#{} · {}", display_idx + 1, role_label),
                    preview,
                    search_value: format!("{role_label} {content}"),
                    content,
                }
            })
            .collect()
    }

    pub(super) fn code_block_candidates_for_scope(
        &self,
        scope: CodeBlockScope,
    ) -> Vec<CopyCodeBlockCandidate> {
        let mut candidates = Vec::new();
        match scope {
            CodeBlockScope::LastResponse => {
                let Some(markdown) = self.last_response_markdown() else {
                    return candidates;
                };
                let extracted = extract_fenced_code_blocks(markdown);
                for (idx, candidate) in extracted.into_iter().enumerate() {
                    let label = match candidate.language {
                        Some(language) => format!("#{} · {}", idx + 1, language),
                        None => format!("#{} · plain text", idx + 1),
                    };
                    let preview = first_non_empty_preview(&candidate.content);
                    let search_value = format!("{label} {preview}");
                    candidates.push(CopyCodeBlockCandidate {
                        id: format!("last-{}", idx + 1),
                        label,
                        preview,
                        search_value,
                        content: candidate.content,
                    });
                }
            }
            CodeBlockScope::AllResponses => {
                for (response_idx, message) in self
                    .copyable_messages
                    .iter()
                    .rev()
                    .filter(|message| message.role == CopyableRole::Response)
                    .enumerate()
                {
                    let extracted = extract_fenced_code_blocks(&message.text);
                    for (block_idx, candidate) in extracted.into_iter().enumerate() {
                        let lang = candidate
                            .language
                            .unwrap_or_else(|| "plain text".to_string());
                        let label = format!("R{} #{} · {lang}", response_idx + 1, block_idx + 1);
                        let preview = first_non_empty_preview(&candidate.content);
                        let search_value = format!("{label} {preview}");
                        candidates.push(CopyCodeBlockCandidate {
                            id: format!("all-{}-{}", response_idx + 1, block_idx + 1),
                            label,
                            preview,
                            search_value,
                            content: candidate.content,
                        });
                    }
                }
            }
        }
        candidates
    }

    pub(super) fn last_response_markdown(&self) -> Option<&str> {
        self.last_assistant_output_markdown
            .as_deref()
            .map(str::trim)
            .filter(|text| !text.is_empty())
    }

    pub(super) fn push_copyable_message(&mut self, role: CopyableRole, text: &str) {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return;
        }
        self.copyable_messages.push(CopyableMessage {
            role,
            text: trimmed.to_string(),
        });
    }

    pub(super) fn last_copyable_response_is(&self, text: &str) -> bool {
        let text = text.trim();
        self.copyable_messages
            .last()
            .is_some_and(|message| message.role == CopyableRole::Response && message.text == text)
    }

    /// Handles a Ctrl+C press at the chat-widget layer.
    ///
    /// The first press arms a time-bounded quit shortcut and shows a footer hint via the bottom
    /// pane. If cancellable work is active, Ctrl+C also submits `Op::Interrupt` after the shortcut
    /// is armed.
    ///
    /// If the same quit shortcut is pressed again before expiry, this requests a shutdown-first
    /// quit.
    pub(super) fn on_ctrl_c(&mut self) {
        let key = key_hint::ctrl(KeyCode::Char('c'));
        let modal_or_popup_active = !self.bottom_pane.no_modal_or_popup_active();
        if self.bottom_pane.on_ctrl_c() == CancellationEvent::Handled {
            if DOUBLE_PRESS_QUIT_SHORTCUT_ENABLED {
                if modal_or_popup_active {
                    self.quit_shortcut_expires_at = None;
                    self.quit_shortcut_key = None;
                    self.bottom_pane.clear_quit_shortcut_hint();
                } else {
                    self.arm_quit_shortcut(key);
                }
            }
            return;
        }

        if !DOUBLE_PRESS_QUIT_SHORTCUT_ENABLED {
            if self.is_cancellable_work_active() {
                self.submit_op(Op::Interrupt);
            } else {
                self.request_quit_without_confirmation();
            }
            return;
        }

        if self.quit_shortcut_active_for(key) {
            self.quit_shortcut_expires_at = None;
            self.quit_shortcut_key = None;
            self.request_quit_without_confirmation();
            return;
        }

        self.arm_quit_shortcut(key);

        if self.is_cancellable_work_active() {
            self.submit_op(Op::Interrupt);
        }
    }

    /// Handles a Ctrl+D press at the chat-widget layer.
    ///
    /// Ctrl-D only participates in quit when the composer is empty and no modal/popup is active.
    /// Otherwise it should be routed to the active view and not attempt to quit.
    pub(super) fn on_ctrl_d(&mut self) -> bool {
        let key = key_hint::ctrl(KeyCode::Char('d'));
        if !DOUBLE_PRESS_QUIT_SHORTCUT_ENABLED {
            if !self.bottom_pane.composer_is_empty() || !self.bottom_pane.no_modal_or_popup_active()
            {
                return false;
            }

            self.request_quit_without_confirmation();
            return true;
        }

        if self.quit_shortcut_active_for(key) {
            self.quit_shortcut_expires_at = None;
            self.quit_shortcut_key = None;
            self.request_quit_without_confirmation();
            return true;
        }

        if !self.bottom_pane.composer_is_empty() || !self.bottom_pane.no_modal_or_popup_active() {
            return false;
        }

        self.arm_quit_shortcut(key);
        true
    }

    /// True if `key` matches the armed quit shortcut and the window has not expired.
    pub(super) fn quit_shortcut_active_for(&self, key: KeyBinding) -> bool {
        self.quit_shortcut_key == Some(key)
            && self
                .quit_shortcut_expires_at
                .is_some_and(|expires_at| Instant::now() < expires_at)
    }

    /// Arm the double-press quit shortcut and show the footer hint.
    ///
    /// This keeps the state machine (`quit_shortcut_*`) in `ChatWidget`, since
    /// it is the component that interprets Ctrl+C vs Ctrl+D and decides whether
    /// quitting is currently allowed, while delegating rendering to `BottomPane`.
    pub(super) fn arm_quit_shortcut(&mut self, key: KeyBinding) {
        self.quit_shortcut_expires_at = Instant::now()
            .checked_add(QUIT_SHORTCUT_TIMEOUT)
            .or_else(|| Some(Instant::now()));
        self.quit_shortcut_key = Some(key);
        self.bottom_pane.show_quit_shortcut_hint(key);
    }

    pub(super) fn is_cancellable_work_active(&self) -> bool {
        self.bottom_pane.is_task_running() || self.is_review_mode
    }
}
