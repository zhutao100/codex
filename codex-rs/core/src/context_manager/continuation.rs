use std::collections::HashMap;
use std::collections::HashSet;

use codex_protocol::models::ResponseItem;

#[derive(Eq, Hash, PartialEq)]
pub(crate) enum ToolCallId<'a> {
    Function(&'a str),
    Custom(&'a str),
}

impl<'a> ToolCallId<'a> {
    pub(crate) fn from_call(item: &'a ResponseItem) -> Option<Self> {
        match item {
            ResponseItem::FunctionCall { call_id, .. } => Some(Self::Function(call_id)),
            ResponseItem::CustomToolCall { call_id, .. } => Some(Self::Custom(call_id)),
            ResponseItem::LocalShellCall { call_id, id, .. } => {
                call_id.as_deref().or(id.as_deref()).map(Self::Function)
            }
            _ => None,
        }
    }
}

/// Positions can be history indices or rollout line numbers; the matching rules are identical.
pub(crate) struct IncompleteToolBatch {
    first_unmatched: usize,
    completed: HashSet<usize>,
}

impl IncompleteToolBatch {
    pub(crate) fn keep(&self, position: usize) -> bool {
        position < self.first_unmatched || self.completed.contains(&position)
    }
}

pub(crate) fn incomplete_tool_batch<'a>(
    tail: impl Iterator<Item = (usize, &'a ResponseItem)>,
) -> Option<IncompleteToolBatch> {
    let mut open = HashMap::new();
    let mut completed = HashSet::new();
    let mut unidentified = None;
    for (pos, item) in tail {
        if crate::compact::is_work_notes_request_item(item) {
            // Capture has its own rejection and retry protocol.
            return None;
        }
        if let Some(id) = ToolCallId::from_call(item) {
            open.entry(id).or_insert(pos);
            continue;
        }
        let output = match item {
            ResponseItem::LocalShellCall { .. } => {
                unidentified.get_or_insert(pos);
                continue;
            }
            ResponseItem::FunctionCallOutput { call_id, .. } => ToolCallId::Function(call_id),
            ResponseItem::CustomToolCallOutput { call_id, .. } => ToolCallId::Custom(call_id),
            _ => continue,
        };
        if let Some(call) = open.remove(&output) {
            completed.extend([call, pos]);
        }
    }
    let first_unmatched = open.into_values().chain(unidentified).min()?;
    Some(IncompleteToolBatch {
        first_unmatched,
        completed,
    })
}
