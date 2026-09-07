use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;

/// Helpers for identifying model-visible "session prefix" messages.
///
/// A session prefix is a user-role message that carries configuration or state needed by
/// follow-up turns (e.g. `<environment_context>`, `<turn_aborted>`). These items are persisted in
/// history so the model can see them, but they are not user intent and must not create user-turn
/// boundaries.
pub(crate) const ENVIRONMENT_CONTEXT_OPEN_TAG: &str = "<environment_context>";
pub(crate) const TURN_ABORTED_OPEN_TAG: &str = "<turn_aborted>";

/// Returns true if `text` starts with a session prefix marker (case-insensitive).
pub(crate) fn is_session_prefix(text: &str) -> bool {
    let trimmed = text.trim_start();
    let lowered = trimmed.to_ascii_lowercase();
    lowered.starts_with(ENVIRONMENT_CONTEXT_OPEN_TAG) || lowered.starts_with(TURN_ABORTED_OPEN_TAG)
}

pub(crate) fn is_turn_aborted_marker(item: &ResponseItem) -> bool {
    let ResponseItem::Message { role, content, .. } = item else {
        return false;
    };

    role == "user"
        && content.iter().any(|content_item| {
            matches!(
                content_item,
                ContentItem::InputText { text } if text.starts_with(TURN_ABORTED_OPEN_TAG)
            )
        })
}
