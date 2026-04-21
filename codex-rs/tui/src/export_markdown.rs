use std::fmt::Write as _;
use std::path::Path;

use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::ENVIRONMENT_CONTEXT_OPEN_TAG;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::RolloutLine;
use serde::Serialize;

const USER_INSTRUCTIONS_PREFIX: &str = "# AGENTS.md instructions for ";
const USER_INSTRUCTIONS_OPEN_TAG_LEGACY: &str = "<user_instructions>";
const SKILL_INSTRUCTIONS_PREFIX: &str = "<skill";

pub(crate) async fn export_rollout_as_markdown(
    rollout_path: &Path,
    out_path: &Path,
) -> std::io::Result<usize> {
    let lines = read_rollout_lines(rollout_path).await?;

    let markdown = rollout_lines_to_markdown(&lines);
    tokio::fs::write(out_path, markdown).await?;

    Ok(count_exported_messages(&lines))
}

pub(crate) async fn export_rollout_as_json(
    rollout_path: &Path,
    out_path: &Path,
) -> std::io::Result<usize> {
    let lines = read_rollout_lines(rollout_path).await?;

    let messages = rollout_lines_to_export_messages(&lines);
    let json = serde_json::to_string_pretty(&messages)
        .map_err(|e| std::io::Error::other(format!("failed to serialize JSON: {e}")))?;

    tokio::fs::write(out_path, json).await?;

    Ok(messages.len())
}

async fn read_rollout_lines(path: &Path) -> std::io::Result<Vec<RolloutLine>> {
    let text = tokio::fs::read_to_string(path).await?;

    let mut lines = Vec::new();
    for raw in text.lines() {
        if raw.trim().is_empty() {
            continue;
        }

        let line: RolloutLine = match serde_json::from_str(raw) {
            Ok(line) => line,
            Err(e) => {
                tracing::warn!("failed to parse rollout line as JSON: {e}");
                continue;
            }
        };

        lines.push(line);
    }

    Ok(lines)
}

fn is_injected_user_message(text: &str) -> bool {
    text.starts_with(USER_INSTRUCTIONS_PREFIX)
        || text.starts_with(USER_INSTRUCTIONS_OPEN_TAG_LEGACY)
        || text.starts_with(ENVIRONMENT_CONTEXT_OPEN_TAG)
        || text.starts_with(SKILL_INSTRUCTIONS_PREFIX)
}

fn content_to_markdown(content: &[ContentItem]) -> String {
    let mut parts = Vec::new();

    for item in content {
        match item {
            ContentItem::InputText { text } | ContentItem::OutputText { text } => {
                if !text.trim().is_empty() {
                    parts.push(text.clone());
                }
            }
            ContentItem::InputImage { image_url } => {
                if image_url.starts_with("data:") {
                    parts.push("_[image omitted: embedded data URL]_".to_string());
                } else if !image_url.trim().is_empty() {
                    parts.push(format!("![image]({image_url})"));
                }
            }
        }
    }

    parts.join("\n\n")
}

fn rollout_lines_to_markdown(lines: &[RolloutLine]) -> String {
    let mut out = String::new();

    let _ = writeln!(&mut out, "# Codex chat export");
    let _ = writeln!(&mut out);

    for line in lines {
        let RolloutItem::ResponseItem(ResponseItem::Message { role, content, .. }) = &line.item
        else {
            continue;
        };

        let message = content_to_markdown(content);
        if message.is_empty() {
            continue;
        }

        if role == "user" && is_injected_user_message(&message) {
            continue;
        }

        let header = match role.as_str() {
            "user" => "## User",
            "assistant" => "## Codex",
            _ => continue,
        };

        let _ = writeln!(&mut out, "{header}");
        let _ = writeln!(&mut out);
        let _ = writeln!(&mut out, "{message}");
        let _ = writeln!(&mut out);
    }

    out
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct ExportMessage {
    pub timestamp: String,
    pub role: String,
    pub content: Vec<ExportContentItem>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum ExportContentItem {
    Text { text: String },
    ImageUrl { url: String },
    ImageOmitted { reason: String },
}

fn rollout_lines_to_export_messages(lines: &[RolloutLine]) -> Vec<ExportMessage> {
    let mut out = Vec::new();

    for line in lines {
        let RolloutItem::ResponseItem(ResponseItem::Message { role, content, .. }) = &line.item
        else {
            continue;
        };

        let message = content_to_markdown(content);
        if message.is_empty() {
            continue;
        }

        if role == "user" && is_injected_user_message(&message) {
            continue;
        }

        let role = match role.as_str() {
            "user" | "assistant" => role.clone(),
            _ => continue,
        };

        let mut items = Vec::new();
        for item in content {
            match item {
                ContentItem::InputText { text } | ContentItem::OutputText { text } => {
                    if !text.trim().is_empty() {
                        items.push(ExportContentItem::Text { text: text.clone() });
                    }
                }
                ContentItem::InputImage { image_url } => {
                    if image_url.trim().is_empty() {
                        continue;
                    }

                    if image_url.starts_with("data:") {
                        items.push(ExportContentItem::ImageOmitted {
                            reason: "embedded_data_url".to_string(),
                        });
                    } else {
                        items.push(ExportContentItem::ImageUrl {
                            url: image_url.clone(),
                        });
                    }
                }
            }
        }

        out.push(ExportMessage {
            timestamp: line.timestamp.clone(),
            role,
            content: items,
        });
    }

    out
}

fn count_exported_messages(lines: &[RolloutLine]) -> usize {
    let mut count = 0;

    for line in lines {
        let RolloutItem::ResponseItem(ResponseItem::Message { role, content, .. }) = &line.item
        else {
            continue;
        };

        let message = content_to_markdown(content);
        if message.is_empty() {
            continue;
        }

        if role == "user" && is_injected_user_message(&message) {
            continue;
        }

        if role == "user" || role == "assistant" {
            count += 1;
        }
    }

    count
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn line(item: RolloutItem) -> RolloutLine {
        RolloutLine {
            timestamp: "2025-01-01T00:00:00.000Z".to_string(),
            item,
        }
    }

    #[test]
    fn export_skips_injected_user_messages() {
        let lines = vec![
            line(RolloutItem::ResponseItem(ResponseItem::Message {
                id: None,
                role: "user".to_string(),
                content: vec![ContentItem::InputText {
                    text: format!(
                        "{ENVIRONMENT_CONTEXT_OPEN_TAG}\n  <cwd>/tmp</cwd>\n</environment_context>"
                    ),
                }],
                end_turn: None,
                phase: None,
            })),
            line(RolloutItem::ResponseItem(ResponseItem::Message {
                id: None,
                role: "user".to_string(),
                content: vec![ContentItem::InputText {
                    text: "Hello".to_string(),
                }],
                end_turn: None,
                phase: None,
            })),
            line(RolloutItem::ResponseItem(ResponseItem::Message {
                id: None,
                role: "assistant".to_string(),
                content: vec![ContentItem::OutputText {
                    text: "Hi there".to_string(),
                }],
                end_turn: None,
                phase: None,
            })),
        ];

        let markdown = rollout_lines_to_markdown(&lines);
        assert_eq!(
            markdown,
            "# Codex chat export\n\n## User\n\nHello\n\n## Codex\n\nHi there\n\n"
        );
    }

    #[test]
    fn json_export_includes_only_user_and_assistant_messages() {
        let lines = vec![
            line(RolloutItem::ResponseItem(ResponseItem::Message {
                id: None,
                role: "user".to_string(),
                content: vec![ContentItem::InputText {
                    text: format!(
                        "{ENVIRONMENT_CONTEXT_OPEN_TAG}\n  <cwd>/tmp</cwd>\n</environment_context>"
                    ),
                }],
                end_turn: None,
                phase: None,
            })),
            line(RolloutItem::ResponseItem(ResponseItem::Message {
                id: None,
                role: "assistant".to_string(),
                content: vec![ContentItem::OutputText {
                    text: "Hello".to_string(),
                }],
                end_turn: None,
                phase: None,
            })),
            line(RolloutItem::ResponseItem(ResponseItem::Message {
                id: None,
                role: "tool".to_string(),
                content: vec![ContentItem::OutputText {
                    text: "ignored".to_string(),
                }],
                end_turn: None,
                phase: None,
            })),
        ];

        let exported = rollout_lines_to_export_messages(&lines);
        assert_eq!(exported.len(), 1);
        assert_eq!(exported[0].role, "assistant");
        assert_eq!(
            exported[0].content,
            vec![ExportContentItem::Text {
                text: "Hello".to_string()
            }]
        );
    }
}
