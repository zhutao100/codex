use crate::context_manager::normalize;
use crate::event_mapping::is_contextual_dev_message_content;
use crate::event_mapping::is_contextual_user_message_content;
use crate::session::turn_context::TurnContext;
use crate::truncate::TruncationPolicy;
use crate::truncate::approx_token_count;
use crate::truncate::approx_tokens_from_byte_count;
use crate::truncate::truncate_function_output_items_with_policy;
use crate::truncate::truncate_text;
use codex_protocol::models::BaseInstructions;
use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::InputModality;
use codex_protocol::protocol::TokenUsage;
use codex_protocol::protocol::TokenUsageInfo;
use codex_protocol::protocol::TurnContextItem;
use std::ops::Deref;

/// Transcript of thread history
#[derive(Debug, Clone, Default)]
pub(crate) struct ContextManager {
    /// The oldest items are at the beginning of the vector.
    items: Vec<ResponseItem>,
    item_token_estimates: Vec<i64>,
    total_item_tokens: i64,
    token_info: Option<TokenUsageInfo>,
    reference_context_item: Option<TurnContextItem>,
}

impl ContextManager {
    pub(crate) fn new() -> Self {
        Self {
            items: Vec::new(),
            item_token_estimates: Vec::new(),
            total_item_tokens: 0,
            token_info: TokenUsageInfo::new_or_append(&None, &None, None),
            reference_context_item: None,
        }
    }

    pub(crate) fn token_info(&self) -> Option<TokenUsageInfo> {
        self.token_info.clone()
    }

    pub(crate) fn set_token_info(&mut self, info: Option<TokenUsageInfo>) {
        self.token_info = info;
    }

    pub(crate) fn reference_context_item(&self) -> Option<TurnContextItem> {
        self.reference_context_item.clone()
    }

    pub(crate) fn set_reference_context_item(&mut self, item: Option<TurnContextItem>) {
        self.reference_context_item = item;
    }

    pub(crate) fn set_token_usage_full(&mut self, context_window: i64) {
        match &mut self.token_info {
            Some(info) => info.fill_to_context_window(context_window),
            None => {
                self.token_info = Some(TokenUsageInfo::full_context_window(context_window));
            }
        }
    }

    /// `items` is ordered from oldest to newest.
    pub(crate) fn record_items<I>(&mut self, items: I, policy: TruncationPolicy)
    where
        I: IntoIterator,
        I::Item: std::ops::Deref<Target = ResponseItem>,
    {
        for item in items {
            let item_ref = item.deref();
            let is_ghost_snapshot = matches!(item_ref, ResponseItem::GhostSnapshot { .. });
            if !is_api_message(item_ref) && !is_ghost_snapshot {
                continue;
            }

            let processed = self.process_item(item_ref, policy);
            self.push_item(processed);
        }
    }

    /// Returns the history prepared for sending to the model. This applies a proper
    /// normalization and drops un-suited items.
    #[cfg(test)]
    pub(crate) fn for_prompt(self) -> Vec<ResponseItem> {
        Self::prepare_items_for_prompt(self.items)
    }

    #[cfg(test)]
    pub(crate) fn prepare_items_for_prompt(items: Vec<ResponseItem>) -> Vec<ResponseItem> {
        Self::prepare_items_for_prompt_with_modalities(
            items,
            &[InputModality::Text, InputModality::Image],
        )
    }

    pub(crate) fn for_prompt_with_modalities(
        self,
        input_modalities: &[InputModality],
    ) -> Vec<ResponseItem> {
        Self::prepare_items_for_prompt_with_modalities(self.items, input_modalities)
    }

    pub(crate) fn prepare_items_for_prompt_with_modalities(
        mut items: Vec<ResponseItem>,
        input_modalities: &[InputModality],
    ) -> Vec<ResponseItem> {
        normalize::normalize_history(&mut items);
        normalize::strip_images_when_unsupported(input_modalities, &mut items);
        items.retain(|item| !matches!(item, ResponseItem::GhostSnapshot { .. }));
        items
    }

    /// Returns raw items in the history.
    pub(crate) fn raw_items(&self) -> &[ResponseItem] {
        &self.items
    }

    // Estimate token usage using byte-based heuristics from the truncation helpers.
    // This is a coarse lower bound, not a tokenizer-accurate count.
    pub(crate) fn estimate_token_count(&self, turn_context: &TurnContext) -> Option<i64> {
        let base_instructions = BaseInstructions {
            text: turn_context.effective_model_instructions(),
        };
        self.estimate_token_count_with_base_instructions(&base_instructions)
    }

    pub(crate) fn estimate_token_count_with_base_instructions(
        &self,
        base_instructions: &BaseInstructions,
    ) -> Option<i64> {
        let base_tokens =
            i64::try_from(approx_token_count(&base_instructions.text)).unwrap_or(i64::MAX);

        Some(base_tokens.saturating_add(self.total_item_tokens))
    }

    pub(crate) fn remove_first_item(&mut self) {
        if !self.items.is_empty() {
            // Remove the oldest item (front of the list). Items are ordered from
            // oldest → newest, so index 0 is the first entry recorded.
            let removed = self.remove_item_at(0);
            // If the removed item participates in a call/output pair, also remove
            // its corresponding counterpart to keep the invariants intact without
            // running a full normalization pass.
            self.remove_corresponding_for(&removed);
        }
    }

    pub(crate) fn remove_last_item(&mut self) -> bool {
        if let Some(removed) = self.pop_item() {
            self.remove_corresponding_for(&removed);
            true
        } else {
            false
        }
    }

    pub(crate) fn replace(&mut self, items: Vec<ResponseItem>) {
        self.replace_items(items);
    }

    /// Replace image content in the last turn if it originated from a tool output.
    /// Returns true when a tool image was replaced, false otherwise.
    pub(crate) fn replace_last_turn_images(&mut self, placeholder: &str) -> bool {
        let Some(index) = self.items.iter().rposition(|item| {
            matches!(item, ResponseItem::FunctionCallOutput { .. }) || is_user_turn_boundary(item)
        }) else {
            return false;
        };

        match &mut self.items[index] {
            ResponseItem::FunctionCallOutput { output, .. } => {
                let Some(content_items) = output.content_items_mut() else {
                    return false;
                };
                let mut replaced = false;
                let placeholder = placeholder.to_string();
                for item in content_items.iter_mut() {
                    if matches!(item, FunctionCallOutputContentItem::InputImage { .. }) {
                        *item = FunctionCallOutputContentItem::InputText {
                            text: placeholder.clone(),
                        };
                        replaced = true;
                    }
                }
                if replaced {
                    self.update_item_token_estimate(index);
                }
                replaced
            }
            _ => false,
        }
    }

    /// Drop the last `num_turns` user turns from this history.
    ///
    /// "User turns" are identified as `ResponseItem::Message` entries whose role is `"user"`.
    ///
    /// This mirrors thread-rollback semantics:
    /// - `num_turns == 0` is a no-op
    /// - if there are no user turns, this is a no-op
    /// - if `num_turns` exceeds the number of user turns, all user turns are dropped while
    ///   preserving any items that occurred before the first user message.
    pub(crate) fn drop_last_n_user_turns(&mut self, num_turns: u32) {
        if num_turns == 0 {
            return;
        }

        let user_positions = user_message_positions(&self.items);
        let Some(&first_user_idx) = user_positions.first() else {
            return;
        };

        let n_from_end = usize::try_from(num_turns).unwrap_or(usize::MAX);
        let cut_idx = if n_from_end >= user_positions.len() {
            first_user_idx
        } else {
            user_positions[user_positions.len() - n_from_end]
        };

        let cut_idx = self.trim_pre_turn_context_updates(first_user_idx, cut_idx);
        self.truncate_items(cut_idx);
    }

    pub(crate) fn update_token_info(
        &mut self,
        usage: &TokenUsage,
        model_context_window: Option<i64>,
    ) {
        self.token_info = TokenUsageInfo::new_or_append(
            &self.token_info,
            &Some(usage.clone()),
            model_context_window,
        );
    }

    fn get_non_last_reasoning_items_tokens(&self) -> i64 {
        // Get reasoning items excluding all the ones after the last user turn boundary.
        let Some(last_user_index) = self.items.iter().rposition(is_user_turn_boundary) else {
            return 0;
        };

        self.items
            .iter()
            .zip(&self.item_token_estimates)
            .take(last_user_index)
            .filter(|item| {
                matches!(
                    item.0,
                    ResponseItem::Reasoning {
                        encrypted_content: Some(_),
                        ..
                    }
                )
            })
            .fold(0i64, |acc, (_, estimate)| acc.saturating_add(*estimate))
    }

    fn items_after_last_model_generated_item(&self) -> &[ResponseItem] {
        let start = self
            .items
            .iter()
            .rposition(is_model_generated_item)
            .map_or(self.items.len(), |index| index.saturating_add(1));
        &self.items[start..]
    }

    /// When true, the server already accounted for past reasoning tokens and
    /// the client should not re-estimate them.
    pub(crate) fn get_total_token_usage(&self, server_reasoning_included: bool) -> i64 {
        let last_tokens = self
            .token_info
            .as_ref()
            .map(|info| info.last_token_usage.total_tokens)
            .unwrap_or(0);
        let after_last_model_generated_start =
            self.items.len() - self.items_after_last_model_generated_item().len();
        let items_after_last_model_generated_tokens = self.item_token_estimates
            [after_last_model_generated_start..]
            .iter()
            .fold(0i64, |acc, estimate| acc.saturating_add(*estimate));
        if server_reasoning_included {
            last_tokens.saturating_add(items_after_last_model_generated_tokens)
        } else {
            last_tokens
                .saturating_add(self.get_non_last_reasoning_items_tokens())
                .saturating_add(items_after_last_model_generated_tokens)
        }
    }

    /// Walk backward from a rollback cut and trim contiguous pre-turn context-update items.
    fn trim_pre_turn_context_updates(
        &self,
        first_user_turn_idx: usize,
        mut cut_idx: usize,
    ) -> usize {
        while cut_idx > first_user_turn_idx {
            match &self.items[cut_idx - 1] {
                ResponseItem::Message { role, content, .. }
                    if role == "developer" && is_contextual_dev_message_content(content) =>
                {
                    cut_idx -= 1;
                }
                ResponseItem::Message { role, content, .. }
                    if role == "user" && is_contextual_user_message_content(content) =>
                {
                    cut_idx -= 1;
                }
                _ => break,
            }
        }
        cut_idx
    }

    /// This function enforces a couple of invariants on the in-memory history:
    /// 1. every call (function/custom) has a corresponding output entry
    /// 2. every output has a corresponding call entry
    #[cfg(test)]
    fn normalize_history(&mut self) {
        normalize::normalize_history(&mut self.items);
        self.rebuild_token_estimates();
    }

    fn push_item(&mut self, item: ResponseItem) {
        let token_estimate = estimate_item_token_count(&item);
        self.total_item_tokens = self.total_item_tokens.saturating_add(token_estimate);
        self.item_token_estimates.push(token_estimate);
        self.items.push(item);
    }

    fn pop_item(&mut self) -> Option<ResponseItem> {
        let index = self.items.len().checked_sub(1)?;
        let token_estimate = self.item_token_estimates.remove(index);
        let item = self.items.remove(index);
        self.total_item_tokens = self.total_item_tokens.saturating_sub(token_estimate);
        Some(item)
    }

    fn remove_item_at(&mut self, index: usize) -> ResponseItem {
        let token_estimate = self.item_token_estimates.remove(index);
        let item = self.items.remove(index);
        self.total_item_tokens = self.total_item_tokens.saturating_sub(token_estimate);
        item
    }

    fn remove_corresponding_for(&mut self, item: &ResponseItem) {
        if let Some(pos) = normalize::corresponding_position_for(&self.items, item) {
            self.remove_item_at(pos);
        }
    }

    fn replace_items(&mut self, items: Vec<ResponseItem>) {
        self.items = items;
        self.rebuild_token_estimates();
    }

    fn truncate_items(&mut self, len: usize) {
        self.items.truncate(len);
        self.item_token_estimates.truncate(len);
        self.total_item_tokens = self
            .item_token_estimates
            .iter()
            .fold(0i64, |acc, estimate| acc.saturating_add(*estimate));
    }

    fn update_item_token_estimate(&mut self, index: usize) {
        let previous = self.item_token_estimates[index];
        let updated = estimate_item_token_count(&self.items[index]);
        self.item_token_estimates[index] = updated;
        self.total_item_tokens = self
            .total_item_tokens
            .saturating_sub(previous)
            .saturating_add(updated);
    }

    fn rebuild_token_estimates(&mut self) {
        self.item_token_estimates = self.items.iter().map(estimate_item_token_count).collect();
        self.total_item_tokens = self
            .item_token_estimates
            .iter()
            .fold(0i64, |acc, estimate| acc.saturating_add(*estimate));
    }

    fn process_item(&self, item: &ResponseItem, policy: TruncationPolicy) -> ResponseItem {
        let policy_with_serialization_budget = policy * 1.2;
        match item {
            ResponseItem::FunctionCallOutput { call_id, output } => {
                let body = match &output.body {
                    FunctionCallOutputBody::Text(content) => FunctionCallOutputBody::Text(
                        truncate_text(content, policy_with_serialization_budget),
                    ),
                    FunctionCallOutputBody::ContentItems(items) => {
                        FunctionCallOutputBody::ContentItems(
                            truncate_function_output_items_with_policy(
                                items,
                                policy_with_serialization_budget,
                            ),
                        )
                    }
                };
                ResponseItem::FunctionCallOutput {
                    call_id: call_id.clone(),
                    output: FunctionCallOutputPayload {
                        body,
                        success: output.success,
                    },
                }
            }
            ResponseItem::CustomToolCallOutput { call_id, output } => {
                let truncated = truncate_text(output, policy_with_serialization_budget);
                ResponseItem::CustomToolCallOutput {
                    call_id: call_id.clone(),
                    output: truncated,
                }
            }
            ResponseItem::Message { .. }
            | ResponseItem::Reasoning { .. }
            | ResponseItem::LocalShellCall { .. }
            | ResponseItem::FunctionCall { .. }
            | ResponseItem::WebSearchCall { .. }
            | ResponseItem::CustomToolCall { .. }
            | ResponseItem::Compaction { .. }
            | ResponseItem::GhostSnapshot { .. }
            | ResponseItem::Other => item.clone(),
        }
    }
}

/// API messages include every non-system item (user/assistant messages, reasoning,
/// tool calls, tool outputs, shell calls, and web-search calls).
fn is_api_message(message: &ResponseItem) -> bool {
    match message {
        ResponseItem::Message { role, .. } => role.as_str() != "system",
        ResponseItem::FunctionCallOutput { .. }
        | ResponseItem::FunctionCall { .. }
        | ResponseItem::CustomToolCall { .. }
        | ResponseItem::CustomToolCallOutput { .. }
        | ResponseItem::LocalShellCall { .. }
        | ResponseItem::Reasoning { .. }
        | ResponseItem::WebSearchCall { .. }
        | ResponseItem::Compaction { .. } => true,
        ResponseItem::GhostSnapshot { .. } => false,
        ResponseItem::Other => false,
    }
}

fn estimate_reasoning_length(encoded_len: usize) -> usize {
    encoded_len
        .saturating_mul(3)
        .checked_div(4)
        .unwrap_or(0)
        .saturating_sub(650)
}

pub(crate) fn estimate_item_token_count(item: &ResponseItem) -> i64 {
    let model_visible_bytes = estimate_response_item_model_visible_bytes(item);
    let model_visible_bytes = usize::try_from(model_visible_bytes).unwrap_or(usize::MAX);
    i64::try_from(approx_tokens_from_byte_count(model_visible_bytes)).unwrap_or(i64::MAX)
}

/// Approximate model-visible byte cost for one image input.
///
/// Inline base64 transport bytes are not model-visible text. Replace each
/// base64 image payload with a fixed image estimate while preserving the data
/// URL prefix and JSON wrapper bytes already counted in the serialized item.
const RESIZED_IMAGE_BYTES_ESTIMATE: i64 = 7373;

fn estimate_response_item_model_visible_bytes(item: &ResponseItem) -> i64 {
    match item {
        ResponseItem::GhostSnapshot { .. } => 0,
        ResponseItem::Reasoning {
            encrypted_content: Some(content),
            ..
        }
        | ResponseItem::Compaction {
            encrypted_content: content,
        } => i64::try_from(estimate_reasoning_length(content.len())).unwrap_or(i64::MAX),
        item => {
            let raw = serde_json::to_string(item)
                .map(|serialized| i64::try_from(serialized.len()).unwrap_or(i64::MAX))
                .unwrap_or_default();
            let (payload_bytes, replacement_bytes) = image_data_url_estimate_adjustment(item);
            if payload_bytes == 0 || replacement_bytes == 0 {
                raw
            } else {
                raw.saturating_sub(payload_bytes)
                    .saturating_add(replacement_bytes)
            }
        }
    }
}

/// Returns the base64 payload for inline image data URLs that are eligible for
/// token-estimation discounting.
fn parse_base64_image_data_url(url: &str) -> Option<&str> {
    if !url
        .get(.."data:".len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("data:"))
    {
        return None;
    }

    let comma_index = url.find(',')?;
    let metadata = &url[..comma_index];
    let payload = &url[comma_index + 1..];
    let metadata_without_scheme = &metadata["data:".len()..];
    let mut metadata_parts = metadata_without_scheme.split(';');
    let mime_type = metadata_parts.next().unwrap_or_default();
    let has_base64_marker = metadata_parts.any(|part| part.eq_ignore_ascii_case("base64"));
    if !mime_type
        .get(.."image/".len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("image/"))
    {
        return None;
    }
    if !has_base64_marker {
        return None;
    }
    Some(payload)
}

/// Scans one response item for discount-eligible inline image data URLs and
/// returns:
/// - total base64 payload bytes to subtract from raw serialized size
/// - total replacement byte estimate for those images
fn image_data_url_estimate_adjustment(item: &ResponseItem) -> (i64, i64) {
    let mut payload_bytes = 0i64;
    let mut replacement_bytes = 0i64;

    let mut accumulate = |image_url: &str| {
        if let Some(payload_len) = parse_base64_image_data_url(image_url).map(str::len) {
            payload_bytes =
                payload_bytes.saturating_add(i64::try_from(payload_len).unwrap_or(i64::MAX));
            replacement_bytes = replacement_bytes.saturating_add(RESIZED_IMAGE_BYTES_ESTIMATE);
        }
    };

    match item {
        ResponseItem::Message { content, .. } => {
            for content_item in content {
                if let ContentItem::InputImage { image_url } = content_item {
                    accumulate(image_url);
                }
            }
        }
        ResponseItem::FunctionCallOutput { output, .. } => {
            if let FunctionCallOutputBody::ContentItems(items) = &output.body {
                for content_item in items {
                    if let FunctionCallOutputContentItem::InputImage { image_url } = content_item {
                        accumulate(image_url);
                    }
                }
            }
        }
        ResponseItem::CustomToolCallOutput { .. }
        | ResponseItem::GhostSnapshot { .. }
        | ResponseItem::Reasoning { .. }
        | ResponseItem::LocalShellCall { .. }
        | ResponseItem::FunctionCall { .. }
        | ResponseItem::WebSearchCall { .. }
        | ResponseItem::CustomToolCall { .. }
        | ResponseItem::Compaction { .. }
        | ResponseItem::Other => {}
    }

    (payload_bytes, replacement_bytes)
}

fn is_model_generated_item(item: &ResponseItem) -> bool {
    matches!(
        item,
        ResponseItem::Message { role, .. } if role == "assistant"
    ) || matches!(
        item,
        ResponseItem::Reasoning { .. }
            | ResponseItem::LocalShellCall { .. }
            | ResponseItem::FunctionCall { .. }
            | ResponseItem::WebSearchCall { .. }
            | ResponseItem::CustomToolCall { .. }
            | ResponseItem::Compaction { .. }
    )
}

pub(crate) fn is_codex_generated_item(item: &ResponseItem) -> bool {
    matches!(
        item,
        ResponseItem::FunctionCallOutput { .. } | ResponseItem::CustomToolCallOutput { .. }
    ) || matches!(item, ResponseItem::Message { role, .. } if role == "developer")
}

pub(crate) fn is_user_turn_boundary(item: &ResponseItem) -> bool {
    let ResponseItem::Message { role, content, .. } = item else {
        return false;
    };

    role == "user" && !is_contextual_user_message_content(content)
}

fn user_message_positions(items: &[ResponseItem]) -> Vec<usize> {
    let mut positions = Vec::new();
    for (idx, item) in items.iter().enumerate() {
        if is_user_turn_boundary(item) {
            positions.push(idx);
        }
    }
    positions
}

#[cfg(test)]
#[path = "history_tests.rs"]
mod tests;
