use std::collections::HashSet;

use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseItem;

use crate::util::error_or_panic;
use tracing::info;

pub(crate) fn ensure_call_outputs_present(items: &mut Vec<ResponseItem>) {
    // Collect synthetic outputs to insert immediately after their calls.
    // Store the insertion position (index of call) alongside the item so the
    // vector can be rebuilt once instead of shifting on every insertion.
    let missing_outputs_to_insert = {
        let function_call_output_ids: HashSet<&str> = items
            .iter()
            .filter_map(|item| match item {
                ResponseItem::FunctionCallOutput { call_id, .. } => Some(call_id.as_str()),
                _ => None,
            })
            .collect();
        let custom_tool_call_output_ids: HashSet<&str> = items
            .iter()
            .filter_map(|item| match item {
                ResponseItem::CustomToolCallOutput { call_id, .. } => Some(call_id.as_str()),
                _ => None,
            })
            .collect();

        let mut missing_outputs_to_insert: Vec<(usize, ResponseItem)> = Vec::new();

        for (idx, item) in items.iter().enumerate() {
            match item {
                ResponseItem::FunctionCall { call_id, .. } => {
                    if !function_call_output_ids.contains(call_id.as_str()) {
                        info!("Function call output is missing for call id: {call_id}");
                        missing_outputs_to_insert.push((
                            idx,
                            ResponseItem::FunctionCallOutput {
                                call_id: call_id.clone(),
                                output: FunctionCallOutputPayload {
                                    body: FunctionCallOutputBody::Text("aborted".to_string()),
                                    ..Default::default()
                                },
                            },
                        ));
                    }
                }
                ResponseItem::CustomToolCall { call_id, .. } => {
                    if !custom_tool_call_output_ids.contains(call_id.as_str()) {
                        error_or_panic(format!(
                            "Custom tool call output is missing for call id: {call_id}"
                        ));
                        missing_outputs_to_insert.push((
                            idx,
                            ResponseItem::CustomToolCallOutput {
                                call_id: call_id.clone(),
                                output: "aborted".to_string(),
                            },
                        ));
                    }
                }
                // LocalShellCall is represented in upstream streams by a FunctionCallOutput
                ResponseItem::LocalShellCall {
                    call_id: Some(call_id),
                    ..
                } => {
                    if !function_call_output_ids.contains(call_id.as_str()) {
                        error_or_panic(format!(
                            "Local shell call output is missing for call id: {call_id}"
                        ));
                        missing_outputs_to_insert.push((
                            idx,
                            ResponseItem::FunctionCallOutput {
                                call_id: call_id.clone(),
                                output: FunctionCallOutputPayload {
                                    body: FunctionCallOutputBody::Text("aborted".to_string()),
                                    ..Default::default()
                                },
                            },
                        ));
                    }
                }
                ResponseItem::LocalShellCall { call_id: None, .. }
                | ResponseItem::FunctionCallOutput { .. }
                | ResponseItem::CustomToolCallOutput { .. }
                | ResponseItem::Message { .. }
                | ResponseItem::Reasoning { .. }
                | ResponseItem::WebSearchCall { .. }
                | ResponseItem::Compaction { .. }
                | ResponseItem::GhostSnapshot { .. }
                | ResponseItem::Other => {}
            }
        }

        missing_outputs_to_insert
    };

    if missing_outputs_to_insert.is_empty() {
        return;
    }

    let missing_output_count = missing_outputs_to_insert.len();
    let mut missing_outputs = missing_outputs_to_insert.into_iter().peekable();
    let old_items = std::mem::take(items);
    items.reserve(old_items.len().saturating_add(missing_output_count));
    for (idx, item) in old_items.into_iter().enumerate() {
        items.push(item);
        while matches!(missing_outputs.peek(), Some((missing_idx, _)) if *missing_idx == idx) {
            if let Some((_, output_item)) = missing_outputs.next() {
                items.push(output_item);
            }
        }
    }
}

pub(crate) fn remove_orphan_outputs(items: &mut Vec<ResponseItem>) {
    let mut function_call_ids = HashSet::new();
    let mut custom_tool_call_ids = HashSet::new();
    for item in items.iter() {
        match item {
            ResponseItem::FunctionCall { call_id, .. }
            | ResponseItem::LocalShellCall {
                call_id: Some(call_id),
                ..
            } => {
                function_call_ids.insert(call_id.clone());
            }
            ResponseItem::CustomToolCall { call_id, .. } => {
                custom_tool_call_ids.insert(call_id.clone());
            }
            ResponseItem::LocalShellCall { call_id: None, .. }
            | ResponseItem::FunctionCallOutput { .. }
            | ResponseItem::CustomToolCallOutput { .. }
            | ResponseItem::Message { .. }
            | ResponseItem::Reasoning { .. }
            | ResponseItem::WebSearchCall { .. }
            | ResponseItem::Compaction { .. }
            | ResponseItem::GhostSnapshot { .. }
            | ResponseItem::Other => {}
        }
    }

    items.retain(|item| match item {
        ResponseItem::FunctionCallOutput { call_id, .. } => {
            let has_match = function_call_ids.contains(call_id);
            if !has_match {
                error_or_panic(format!(
                    "Orphan function call output for call id: {call_id}"
                ));
            }
            has_match
        }
        ResponseItem::CustomToolCallOutput { call_id, .. } => {
            let has_match = custom_tool_call_ids.contains(call_id);
            if !has_match {
                error_or_panic(format!(
                    "Orphan custom tool call output for call id: {call_id}"
                ));
            }
            has_match
        }
        _ => true,
    });
}

pub(crate) fn normalize_history(items: &mut Vec<ResponseItem>) {
    ensure_call_outputs_present(items);
    remove_orphan_outputs(items);
}

pub(crate) fn corresponding_position_for(
    items: &[ResponseItem],
    item: &ResponseItem,
) -> Option<usize> {
    match item {
        ResponseItem::FunctionCall { call_id, .. }
        | ResponseItem::LocalShellCall {
            call_id: Some(call_id),
            ..
        } => function_output_position(items, call_id),
        ResponseItem::FunctionCallOutput { call_id, .. } => {
            function_or_shell_call_position(items, call_id)
        }
        ResponseItem::CustomToolCall { call_id, .. } => custom_tool_output_position(items, call_id),
        ResponseItem::CustomToolCallOutput { call_id, .. } => {
            custom_tool_call_position(items, call_id)
        }
        ResponseItem::LocalShellCall { call_id: None, .. }
        | ResponseItem::Message { .. }
        | ResponseItem::Reasoning { .. }
        | ResponseItem::WebSearchCall { .. }
        | ResponseItem::Compaction { .. }
        | ResponseItem::GhostSnapshot { .. }
        | ResponseItem::Other => None,
    }
}

fn function_output_position(items: &[ResponseItem], call_id: &str) -> Option<usize> {
    items.iter().position(|item| {
        matches!(
            item,
            ResponseItem::FunctionCallOutput {
                call_id: existing,
                ..
            } if existing == call_id
        )
    })
}

fn function_or_shell_call_position(items: &[ResponseItem], call_id: &str) -> Option<usize> {
    items
        .iter()
        .position(|item| {
            matches!(
                item,
                ResponseItem::FunctionCall {
                    call_id: existing,
                    ..
                } if existing == call_id
            )
        })
        .or_else(|| {
            items.iter().position(|item| {
                matches!(
                    item,
                    ResponseItem::LocalShellCall {
                        call_id: Some(existing),
                        ..
                    } if existing == call_id
                )
            })
        })
}

fn custom_tool_output_position(items: &[ResponseItem], call_id: &str) -> Option<usize> {
    items.iter().position(|item| {
        matches!(
            item,
            ResponseItem::CustomToolCallOutput {
                call_id: existing,
                ..
            } if existing == call_id
        )
    })
}

fn custom_tool_call_position(items: &[ResponseItem], call_id: &str) -> Option<usize> {
    items.iter().position(|item| {
        matches!(
            item,
            ResponseItem::CustomToolCall {
                call_id: existing,
                ..
            } if existing == call_id
        )
    })
}
