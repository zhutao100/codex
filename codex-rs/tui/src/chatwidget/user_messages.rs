//! User-message and queue data types for ChatWidget.

use super::*;

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct UserMessage {
    pub(super) text: String,
    pub(super) local_images: Vec<LocalImageAttachment>,
    pub(super) text_elements: Vec<TextElement>,
    pub(super) mention_paths: HashMap<String, String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum UserMessageHistoryRecord {
    UserMessageText,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct PendingSteerCompareKey {
    pub(super) message: String,
    pub(super) image_count: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct PendingSteer {
    pub(super) target_turn_id: String,
    pub(super) user_message: UserMessage,
    pub(super) history_record: UserMessageHistoryRecord,
    pub(super) compare_key: PendingSteerCompareKey,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct UserMessageDisplay {
    pub(super) message: String,
    pub(super) text_elements: Vec<TextElement>,
    pub(super) local_images: Vec<PathBuf>,
}

impl UserMessageDisplay {
    pub(super) fn from_user_message(message: UserMessage) -> Self {
        Self {
            message: message.text,
            text_elements: message.text_elements,
            local_images: message
                .local_images
                .into_iter()
                .map(|image| image.path)
                .collect(),
        }
    }

    pub(super) fn from_event(event: UserMessageEvent) -> Self {
        Self {
            message: event.message,
            text_elements: event.text_elements,
            local_images: event.local_images,
        }
    }

    pub(super) fn has_visible_content(&self) -> bool {
        !self.message.trim().is_empty()
            || !self.text_elements.is_empty()
            || !self.local_images.is_empty()
    }
}

pub(super) fn user_message_for_history(
    message: UserMessage,
    history_record: &UserMessageHistoryRecord,
) -> UserMessage {
    match history_record {
        UserMessageHistoryRecord::UserMessageText => message,
    }
}

pub(super) fn user_message_preview_text(
    message: &UserMessage,
    history_record: Option<&UserMessageHistoryRecord>,
) -> String {
    match history_record {
        Some(UserMessageHistoryRecord::UserMessageText) | None => message.text.clone(),
    }
}

pub(super) fn append_text_with_rebased_elements(
    target_text: &mut String,
    target_text_elements: &mut Vec<TextElement>,
    text: &str,
    text_elements: impl IntoIterator<Item = TextElement>,
) {
    let offset = target_text.len();
    target_text.push_str(text);
    target_text_elements.extend(text_elements.into_iter().map(|mut element| {
        element.byte_range.start += offset;
        element.byte_range.end += offset;
        element
    }));
}

pub(super) fn merge_user_messages(messages: impl IntoIterator<Item = UserMessage>) -> UserMessage {
    let mut combined = UserMessage {
        text: String::new(),
        local_images: Vec::new(),
        text_elements: Vec::new(),
        mention_paths: HashMap::new(),
    };

    for (idx, message) in messages.into_iter().enumerate() {
        if idx > 0 {
            combined.text.push('\n');
        }
        append_text_with_rebased_elements(
            &mut combined.text,
            &mut combined.text_elements,
            &message.text,
            message.text_elements,
        );
        combined.local_images.extend(message.local_images);
        combined.mention_paths.extend(message.mention_paths);
    }

    combined
}

#[derive(Clone, Debug)]
pub(super) struct QueuedUserMessage {
    pub(super) id: u64,
    pub(super) text: String,
    pub(super) local_images: Vec<LocalImageAttachment>,
    pub(super) text_elements: Vec<TextElement>,
    pub(super) mention_paths: HashMap<String, String>,
    pub(super) model_override: Option<String>,
    pub(super) effort_override: Option<Option<ReasoningEffortConfig>>,
}

#[derive(Clone)]
pub(super) struct QueuedComposerSnapshot {
    pub(super) text: String,
    pub(super) text_elements: Vec<TextElement>,
    pub(super) local_images: Vec<LocalImageAttachment>,
    pub(super) mention_paths: HashMap<String, String>,
}

#[derive(Clone)]
pub(super) struct QueuedUserMessageDraft {
    pub(super) text: String,
    pub(super) text_elements: Vec<TextElement>,
    pub(super) local_images: Vec<LocalImageAttachment>,
    pub(super) mention_paths: HashMap<String, String>,
    pub(super) model_override: Option<String>,
    pub(super) effort_override: Option<Option<ReasoningEffortConfig>>,
}

pub(super) struct QueuedEditState {
    pub(super) selected_id: u64,
    pub(super) composer_before_edit: QueuedComposerSnapshot,
    pub(super) drafts: HashMap<u64, QueuedUserMessageDraft>,
}

impl From<String> for UserMessage {
    fn from(text: String) -> Self {
        Self {
            text,
            local_images: Vec::new(),
            // Plain text conversion has no UI element ranges.
            text_elements: Vec::new(),
            mention_paths: HashMap::new(),
        }
    }
}

impl From<&str> for UserMessage {
    fn from(text: &str) -> Self {
        Self {
            text: text.to_string(),
            local_images: Vec::new(),
            // Plain text conversion has no UI element ranges.
            text_elements: Vec::new(),
            mention_paths: HashMap::new(),
        }
    }
}

pub(crate) fn create_initial_user_message(
    text: Option<String>,
    local_image_paths: Vec<PathBuf>,
    text_elements: Vec<TextElement>,
) -> Option<UserMessage> {
    let text = text.unwrap_or_default();
    if text.is_empty() && local_image_paths.is_empty() {
        None
    } else {
        let local_images = local_image_paths
            .into_iter()
            .enumerate()
            .map(|(idx, path)| LocalImageAttachment {
                placeholder: local_image_label_text(idx + 1),
                path,
            })
            .collect();
        Some(UserMessage {
            text,
            local_images,
            text_elements,
            mention_paths: HashMap::new(),
        })
    }
}

impl ChatWidget {
    pub(super) fn pending_steer_compare_key_from_inputs(
        items: &[UserInput],
    ) -> PendingSteerCompareKey {
        let mut message = String::new();
        let mut image_count = 0;

        for item in items {
            match item {
                UserInput::Text { text, .. } => message.push_str(text),
                UserInput::Image { .. } | UserInput::LocalImage { .. } => image_count += 1,
                UserInput::Skill { .. } | UserInput::Mention { .. } => {}
                _ => {}
            }
        }

        PendingSteerCompareKey {
            message,
            image_count,
        }
    }

    pub(super) fn pending_steer_compare_key_from_event(
        event: &UserMessageEvent,
    ) -> PendingSteerCompareKey {
        PendingSteerCompareKey {
            message: event.message.clone(),
            image_count: event.images.as_ref().map_or(0, Vec::len) + event.local_images.len(),
        }
    }
}
