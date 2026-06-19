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
    pub(super) client_user_message_id: Option<String>,
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

fn build_placeholder_mapping(
    local_images: Vec<LocalImageAttachment>,
    next_label: &mut usize,
) -> (HashMap<String, String>, Vec<LocalImageAttachment>) {
    let mut mapping = HashMap::new();
    let mut remapped_images = Vec::new();
    for attachment in local_images {
        let new_placeholder = local_image_label_text(*next_label);
        *next_label += 1;
        mapping.insert(attachment.placeholder.clone(), new_placeholder.clone());
        remapped_images.push(LocalImageAttachment {
            placeholder: new_placeholder,
            path: attachment.path,
        });
    }
    (mapping, remapped_images)
}

fn remap_placeholders_in_text(
    text: String,
    text_elements: Vec<TextElement>,
    mapping: &HashMap<String, String>,
) -> (String, Vec<TextElement>) {
    if mapping.is_empty() {
        return (text, text_elements);
    }

    let mut elements = text_elements;
    elements.sort_by_key(|element| element.byte_range.start);

    let mut cursor = 0usize;
    let mut rebuilt = String::new();
    let mut rebuilt_elements = Vec::new();
    for mut element in elements {
        let start = element.byte_range.start.min(text.len());
        let end = element.byte_range.end.min(text.len());
        if let Some(segment) = text.get(cursor..start) {
            rebuilt.push_str(segment);
        }

        let original = text.get(start..end).unwrap_or("");
        let placeholder = element.placeholder(&text);
        let replacement = placeholder
            .and_then(|placeholder| mapping.get(placeholder))
            .map(String::as_str)
            .unwrap_or(original);

        let element_start = rebuilt.len();
        rebuilt.push_str(replacement);
        let element_end = rebuilt.len();

        if let Some(remapped) = placeholder.and_then(|placeholder| mapping.get(placeholder)) {
            element.set_placeholder(Some(remapped.clone()));
        }
        element.byte_range = (element_start..element_end).into();
        rebuilt_elements.push(element);
        cursor = end;
    }
    if let Some(segment) = text.get(cursor..) {
        rebuilt.push_str(segment);
    }

    (rebuilt, rebuilt_elements)
}

fn remap_placeholders_for_message(message: UserMessage, next_label: &mut usize) -> UserMessage {
    let UserMessage {
        text,
        local_images,
        text_elements,
        mention_paths,
    } = message;
    let (mapping, local_images) = build_placeholder_mapping(local_images, next_label);
    let (text, text_elements) = remap_placeholders_in_text(text, text_elements, &mapping);

    UserMessage {
        text,
        local_images,
        text_elements,
        mention_paths,
    }
}

pub(super) fn merge_user_messages(messages: impl IntoIterator<Item = UserMessage>) -> UserMessage {
    let mut combined = UserMessage {
        text: String::new(),
        local_images: Vec::new(),
        text_elements: Vec::new(),
        mention_paths: HashMap::new(),
    };

    let mut next_image_label = 1;
    for (idx, message) in messages.into_iter().enumerate() {
        let message = remap_placeholders_for_message(message, &mut next_image_label);
        if idx > 0 {
            combined.text.push('\n');
        }
        let UserMessage {
            text,
            local_images,
            text_elements,
            mention_paths,
        } = message;
        append_text_with_rebased_elements(
            &mut combined.text,
            &mut combined.text_elements,
            &text,
            text_elements,
        );
        combined.local_images.extend(local_images);
        for (name, path) in mention_paths {
            // Mention paths are keyed by visible name, so keep the first path in text order.
            combined.mention_paths.entry(name).or_insert(path);
        }
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
    pub(super) action: QueuedInputAction,
    pub(super) pending_pastes: Vec<(String, String)>,
}

#[derive(Clone)]
pub(super) struct QueuedComposerSnapshot {
    pub(super) text: String,
    pub(super) text_elements: Vec<TextElement>,
    pub(super) local_images: Vec<LocalImageAttachment>,
    pub(super) mention_paths: HashMap<String, String>,
    pub(super) pending_pastes: Vec<(String, String)>,
}

#[derive(Clone)]
pub(super) struct QueuedUserMessageDraft {
    pub(super) text: String,
    pub(super) text_elements: Vec<TextElement>,
    pub(super) local_images: Vec<LocalImageAttachment>,
    pub(super) mention_paths: HashMap<String, String>,
    pub(super) model_override: Option<String>,
    pub(super) effort_override: Option<Option<ReasoningEffortConfig>>,
    pub(super) action: QueuedInputAction,
    pub(super) pending_pastes: Vec<(String, String)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum QueueDrain {
    Continue,
    Stop,
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
