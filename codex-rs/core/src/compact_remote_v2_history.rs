//! Retention for the streaming compaction protocol, without durable control items.

use crate::compact::content_items_to_text;
use crate::compact::is_summary_message;
use crate::context_manager::estimate_image_token_count;
use crate::context_manager::estimate_item_token_count;
use crate::event_mapping::is_contextual_user_message_content;
use crate::session_prefix::TURN_ABORTED_OPEN_TAG;
use crate::truncate::TruncationPolicy;
use crate::truncate::approx_token_count;
use crate::truncate::truncate_text;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::models::is_image_close_tag_text;
use codex_protocol::models::is_image_open_tag_text;
use codex_protocol::models::is_local_image_close_tag_text;
use codex_protocol::models::is_local_image_open_tag_text;

pub(super) const RETAINED_MESSAGE_TOKEN_BUDGET: usize = 64_000;

fn content_tokens(item: &ContentItem) -> usize {
    match item {
        ContentItem::InputText { text } | ContentItem::OutputText { text } => {
            approx_token_count(text)
        }
        ContentItem::InputImage { .. } => estimate_image_token_count(),
    }
}

pub(super) fn retained_tokens(item: &ResponseItem) -> usize {
    let estimated = usize::try_from(estimate_item_token_count(item)).unwrap_or(usize::MAX);
    let ResponseItem::Message { content, .. } = item else {
        return estimated;
    };
    // History already discounts inline base64. Also charge remote image URLs;
    // their short transport representation says nothing about model cost.
    estimated
        .max(content.iter().map(content_tokens).sum::<usize>())
        .max(1)
}

fn is_retained(item: &ResponseItem) -> bool {
    let ResponseItem::Message { role, content, .. } = item else {
        return false;
    };
    role == "user"
        && (!is_contextual_user_message_content(content)
            || content_items_to_text(content).is_some_and(|text| {
                is_summary_message(&text) || text.trim_start().starts_with(TURN_ABORTED_OPEN_TAG)
            }))
}

pub(super) fn retain_messages(input: &[ResponseItem], budget: usize) -> Vec<ResponseItem> {
    let mut remaining = budget.min(RETAINED_MESSAGE_TOKEN_BUDGET);
    let mut retained = Vec::new();
    for item in input.iter().rev().filter(|item| is_retained(item)) {
        if remaining == 0 {
            break;
        }
        let cost = retained_tokens(item);
        if cost <= remaining {
            retained.push(item.clone());
            remaining -= cost;
        } else {
            if let Some(item) = truncate_message(item.clone(), remaining) {
                retained.push(item);
            }
            // An image boundary that cannot fit must not resurrect older input.
            break;
        }
    }
    retained.reverse();
    retained
}

fn truncate_message(mut item: ResponseItem, budget: usize) -> Option<ResponseItem> {
    let ResponseItem::Message { content, .. } = &mut item else {
        return None;
    };
    let original = std::mem::take(content);
    let overhead = retained_tokens(&item);
    let mut content_budget = budget.saturating_sub(overhead);
    loop {
        let content = truncate_content(original.clone(), content_budget);
        if content.is_empty() {
            return None;
        }
        let ResponseItem::Message {
            content: target, ..
        } = &mut item
        else {
            unreachable!();
        };
        *target = content;
        let cost = retained_tokens(&item);
        if cost <= budget {
            return Some(item);
        }
        // JSON escaping and the middle-truncation notice also cost tokens.
        let adjusted = content_budget
            .saturating_sub(cost - budget)
            .saturating_sub(1);
        if adjusted == content_budget {
            return None;
        }
        content_budget = adjusted;
    }
}

fn truncate_content(mut content: Vec<ContentItem>, budget: usize) -> Vec<ContentItem> {
    let has_images = content
        .iter()
        .any(|item| matches!(item, ContentItem::InputImage { .. }));
    if !has_images {
        let mut remaining = budget;
        content.retain_mut(|item| truncate_text_item(item, &mut remaining));
        return content;
    }

    let mut remaining = budget;
    let mut retained = Vec::new();
    while !content.is_empty() && remaining > 0 {
        let last = content.len() - 1;
        let image_index = match &content[last] {
            ContentItem::InputImage { .. } => Some(last),
            ContentItem::InputText { text }
                if (is_image_close_tag_text(text) || is_local_image_close_tag_text(text))
                    && last > 0
                    && matches!(content[last - 1], ContentItem::InputImage { .. }) =>
            {
                Some(last - 1)
            }
            _ => None,
        };
        if let Some(image_index) = image_index {
            let has_open_tag = image_index > 0
                && matches!(&content[image_index - 1], ContentItem::InputText { text }
                    if is_image_open_tag_text(text) || is_local_image_open_tag_text(text));
            let start = image_index - usize::from(has_open_tag);
            let cost = content[start..].iter().map(content_tokens).sum::<usize>();
            if cost > remaining {
                break;
            }
            retained.extend(content.drain(start..).rev());
            remaining -= cost;
        } else if let Some(mut item) = content.pop()
            && truncate_text_item(&mut item, &mut remaining)
        {
            retained.push(item);
        }
    }
    retained.reverse();
    retained
}

fn truncate_text_item(item: &mut ContentItem, remaining: &mut usize) -> bool {
    match item {
        ContentItem::InputText { text } | ContentItem::OutputText { text } => {
            if *remaining == 0 {
                return false;
            }
            let cost = approx_token_count(text);
            if cost > *remaining {
                let limit = *remaining;
                *remaining = 0;
                let mut budget = limit;
                loop {
                    let truncated = truncate_text(text, TruncationPolicy::Tokens(budget));
                    let cost = approx_token_count(&truncated);
                    if cost <= limit {
                        *text = truncated;
                        break;
                    }
                    budget = budget.saturating_sub(cost - limit).saturating_sub(1);
                    if budget == 0 {
                        return false;
                    }
                }
            } else {
                *remaining -= cost;
            }
            !text.is_empty()
        }
        ContentItem::InputImage { .. } => unreachable!("images are retained with their labels"),
    }
}

#[cfg(test)]
#[path = "compact_remote_v2_history_tests.rs"]
mod tests;
