use codex_protocol::items::AgentMessageContent;
use codex_protocol::items::TurnItem;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use futures::StreamExt;

use crate::client_common::Prompt;
use crate::client_common::ResponseEvent;
use crate::codex::Session;
use crate::codex::TurnContext;
use crate::error::CodexErr;
use crate::error::Result;
use crate::instructions::SkillInstructions;
use crate::instructions::UserInstructions;
use crate::models_manager::manager::RefreshStrategy;
use crate::parse_turn_item;
use crate::stream_events_utils::last_assistant_message_from_item;
use crate::truncate::TruncationPolicy;
use crate::truncate::approx_token_count;
use crate::truncate::truncate_text;

const THREAD_NAME_INSTRUCTION: &str = "Return a concise 3-6 word thread name for the conversation below. Output only the thread name.";
const MAX_CONTEXT_TOKENS: usize = 40_000;

pub(crate) async fn generate_thread_name(
    session: &Session,
    turn_context: &TurnContext,
) -> Result<Option<String>> {
    let history = session.clone_history().await;
    let items = history.raw_items();
    let mut blocks = collect_message_blocks(items);
    if blocks.is_empty() {
        return Ok(None);
    }

    loop {
        let selected = select_blocks_with_token_budget(&blocks, MAX_CONTEXT_TOKENS);
        if selected.is_empty() {
            return Ok(None);
        }
        let prompt_text = format_thread_name_prompt(&selected);
        match stream_thread_name(session, turn_context, &prompt_text).await {
            Ok(thread_name) => return Ok(thread_name),
            Err(CodexErr::ContextWindowExceeded) => {
                if selected.len() <= 1 {
                    return Err(CodexErr::ContextWindowExceeded);
                }
                blocks = selected.into_iter().skip(1).collect();
            }
            Err(err) => return Err(err),
        }
    }
}

fn collect_message_blocks(items: &[ResponseItem]) -> Vec<String> {
    let mut blocks = Vec::new();

    for item in items {
        if let ResponseItem::Message { role, content, .. } = item
            && role == "user"
            && (UserInstructions::is_user_instructions(content)
                || SkillInstructions::is_skill_instructions(content))
        {
            continue;
        }

        let Some(turn_item) = parse_turn_item(item) else {
            continue;
        };

        match turn_item {
            TurnItem::UserMessage(message) => {
                let text = message.message();
                let trimmed = text.trim();
                if trimmed.is_empty() {
                    continue;
                }
                blocks.push(format!("User:\n{trimmed}"));
            }
            TurnItem::AgentMessage(message) => {
                let text = message
                    .content
                    .iter()
                    .map(|content| match content {
                        AgentMessageContent::Text { text } => text.as_str(),
                    })
                    .collect::<Vec<_>>()
                    .join("");
                let trimmed = text.trim();
                if trimmed.is_empty() {
                    continue;
                }
                blocks.push(format!("Assistant:\n{trimmed}"));
            }
            _ => {}
        }
    }

    blocks
}

fn select_blocks_with_token_budget(blocks: &[String], max_tokens: usize) -> Vec<String> {
    if max_tokens == 0 {
        return Vec::new();
    }

    let mut remaining = max_tokens;
    let mut selected = Vec::new();

    for block in blocks.iter().rev() {
        if remaining == 0 {
            break;
        }

        let tokens = approx_token_count(block);
        if tokens <= remaining {
            selected.push(block.clone());
            remaining = remaining.saturating_sub(tokens);
        } else {
            let truncated = truncate_text(block, TruncationPolicy::Tokens(remaining));
            selected.push(truncated);
            break;
        }
    }

    selected.reverse();
    selected
}

fn format_thread_name_prompt(selected: &[String]) -> String {
    let mut prompt = String::new();
    prompt.push_str(THREAD_NAME_INSTRUCTION);
    prompt.push_str("\n\nConversation:\n");
    prompt.push_str(&selected.join("\n\n"));
    prompt
}

async fn stream_thread_name(
    session: &Session,
    turn_context: &TurnContext,
    prompt_text: &str,
) -> Result<Option<String>> {
    let default_model = session
        .services
        .models_manager
        .get_default_model(
            &turn_context.config.model,
            turn_context.config.as_ref(),
            RefreshStrategy::OnlineIfUncached,
        )
        .await;
    let model_info = if default_model.is_empty() {
        turn_context.model_info.clone()
    } else {
        session
            .services
            .models_manager
            .get_model_info(&default_model, turn_context.config.as_ref())
            .await
    };

    let prompt = Prompt {
        input: vec![ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: prompt_text.to_string(),
            }],
            end_turn: None,
            phase: None,
        }],
        ..Prompt::default()
    };

    let mut client_session = session.services.model_client.new_session();
    let turn_metadata_header = turn_context.resolve_turn_metadata_header().await;
    let mut stream = client_session
        .stream(
            &prompt,
            &model_info,
            &turn_context.otel_manager,
            turn_context.reasoning_effort,
            turn_context.reasoning_summary,
            turn_metadata_header.as_deref(),
        )
        .await?;
    let mut last_message: Option<String> = None;
    let mut output_buffer = String::new();
    let mut completed = false;

    while let Some(event) = stream.next().await {
        match event? {
            ResponseEvent::OutputItemDone(item) => {
                if let Some(message) = last_assistant_message_from_item(&item, false) {
                    last_message = Some(message);
                }
            }
            ResponseEvent::OutputTextDelta(delta) => {
                output_buffer.push_str(&delta);
            }
            ResponseEvent::Completed { .. } => {
                completed = true;
                break;
            }
            _ => {}
        }
    }

    if !completed && last_message.is_none() && output_buffer.trim().is_empty() {
        return Err(CodexErr::Stream(
            "thread name generation did not complete".to_string(),
            None,
        ));
    }

    let raw_thread_name = last_message.or_else(|| {
        let trimmed = output_buffer.trim();
        (!trimmed.is_empty()).then(|| output_buffer.to_string())
    });

    Ok(raw_thread_name
        .as_deref()
        .and_then(normalize_thread_name_output))
}

fn normalize_thread_name_output(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }

    let stripped = strip_wrapping_quotes(trimmed);
    let collapsed = stripped.split_whitespace().collect::<Vec<_>>().join(" ");
    let collapsed = collapsed.trim();
    if collapsed.is_empty() {
        return None;
    }

    let mut thread_name = collapsed.to_string();
    if thread_name.chars().count() > crate::util::MAX_THREAD_NAME_CHARS {
        thread_name = thread_name
            .chars()
            .take(crate::util::MAX_THREAD_NAME_CHARS)
            .collect();
        thread_name = thread_name.trim().to_string();
    }

    if thread_name.is_empty() {
        None
    } else {
        Some(thread_name)
    }
}

fn strip_wrapping_quotes(value: &str) -> &str {
    for quote in ['"', '\'', '`'] {
        if let Some(stripped) = value
            .strip_prefix(quote)
            .and_then(|rest| rest.strip_suffix(quote))
        {
            return stripped;
        }
    }
    value
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::normalize_thread_name_output;

    #[test]
    fn normalize_thread_name_trims_and_collapses_whitespace() {
        assert_eq!(
            normalize_thread_name_output("  Hello   world  "),
            Some("Hello world".to_string())
        );
    }

    #[test]
    fn normalize_thread_name_strips_wrapping_quotes() {
        assert_eq!(
            normalize_thread_name_output("\"Hello world\""),
            Some("Hello world".to_string())
        );
    }

    #[test]
    fn normalize_thread_name_returns_none_for_empty() {
        assert_eq!(normalize_thread_name_output("\n  "), None);
    }
}
