use crate::client_common::ShellOutputFormatter;
use crate::context_manager::normalize;
use crate::event_mapping::is_contextual_user_message_content;
use crate::event_mapping::is_pre_turn_dev_message_content;
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
use codex_protocol::protocol::MeasuredInputSurface;
use codex_protocol::protocol::TokenUsage;
use codex_protocol::protocol::TokenUsageInfo;
use codex_protocol::protocol::TurnContextItem;
use std::ops::Deref;

#[derive(Clone, Copy, Debug)]
pub(crate) struct UsageSource<'a> {
    pub(crate) model: &'a str,
    pub(crate) provider: &'a str,
    pub(crate) endpoint: Option<&'a str>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct HelperProjection {
    pub(crate) images: bool,
    pub(crate) structured_outputs: bool,
}

impl HelperProjection {
    pub(crate) fn plain(images: bool) -> Self {
        Self {
            images,
            structured_outputs: false,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum WorkBudget {
    Helper,
    Review,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct ProjectionCost {
    raw: i64,
    text: i64,
    structured_delta: i64,
}

impl ProjectionCost {
    fn of(item: &ResponseItem, formatter: &mut ShellOutputFormatter) -> Self {
        let raw = estimate_item_token_count(item);
        let has_image = match item {
            ResponseItem::Message { content, .. } => content
                .iter()
                .any(|p| matches!(p, ContentItem::InputImage { .. })),
            ResponseItem::FunctionCallOutput { output, .. } => {
                output.content_items().is_some_and(|parts| {
                    parts
                        .iter()
                        .any(|p| matches!(p, FunctionCallOutputContentItem::InputImage { .. }))
                })
            }
            _ => false,
        };
        let text = if has_image {
            let mut projected = item.clone();
            normalize::strip_images_when_unsupported(
                &[InputModality::Text],
                std::slice::from_mut(&mut projected),
            );
            estimate_item_token_count(&projected)
        } else {
            raw
        };
        let structured_delta = formatter
            .project(item)
            .map_or(0, |wire| estimate_item_token_count(&wire) - raw);
        Self {
            raw,
            text,
            structured_delta,
        }
    }

    fn get(self, projection: HelperProjection) -> i64 {
        let cost = if projection.images {
            self.raw
        } else {
            self.text
        };
        cost + if projection.structured_outputs {
            self.structured_delta
        } else {
            0
        }
    }
    fn add(self, rhs: Self) -> Self {
        Self {
            raw: self.raw.saturating_add(rhs.raw),
            text: self.text.saturating_add(rhs.text),
            structured_delta: self.structured_delta.saturating_add(rhs.structured_delta),
        }
    }
    fn sub(self, rhs: Self) -> Self {
        Self {
            raw: self.raw.saturating_sub(rhs.raw),
            text: self.text.saturating_sub(rhs.text),
            structured_delta: self.structured_delta.saturating_sub(rhs.structured_delta),
        }
    }
}

/// Frozen immediately before recording a capture marker, both live and during rollout replay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorkNotesBaseline {
    index: usize,
    model: String,
    provider: String,
    endpoint: String,
    // [images][structured shell outputs]. Each mode respects the receipt's input floor.
    work: [[i64; 2]; 2],
    items: ProjectionCost,
    measured_base_tokens: Option<i64>,
    pub(crate) measured_tool_tokens: Option<i64>,
}

impl WorkNotesBaseline {
    pub(crate) fn work_tokens(&self, base: &BaseInstructions, projection: HelperProjection) -> i64 {
        self.work[usize::from(projection.images)][usize::from(projection.structured_outputs)]
            .saturating_add(instruction_delta(base, self.measured_base_tokens))
            .max(0)
    }

    pub(crate) fn projected_tokens(
        &self,
        base: &BaseInstructions,
        projection: HelperProjection,
    ) -> i64 {
        self.items
            .get(projection)
            .saturating_add(approx_token_count(&base.text) as i64)
    }
}

fn instruction_delta(base: &BaseInstructions, measured: Option<i64>) -> i64 {
    measured.map_or(0, |cost| {
        (approx_token_count(&base.text) as i64).saturating_sub(cost)
    })
}

/// Transcript of thread history
#[derive(Debug, Clone, Default)]
pub(crate) struct ContextManager {
    /// The oldest items are at the beginning of the vector.
    items: Vec<ResponseItem>,
    item_token_estimates: Vec<ProjectionCost>,
    output_formatter: ShellOutputFormatter,
    total_item_tokens: ProjectionCost,
    original_covered_cost: ProjectionCost,
    /// Model-projected local cost of the prefix covered by a positive provider receipt.
    /// Unlike the current coverage boundary, it is not reduced by continuation cleanup.
    pub(crate) measured_item_tokens: Option<i64>,
    pub(crate) measured_format_delta: Option<i64>,
    pub(crate) measured_base_tokens: Option<i64>,
    pub(crate) measured_tool_tokens: Option<i64>,
    token_info: Option<TokenUsageInfo>,
    pub(crate) measured_surface: Option<MeasuredInputSurface>,
    source_model: Option<String>,
    /// Items at this index and later are not covered by the stored usage baseline.
    last_usage_item_count: usize,
    request_context_tokens: i64,
    pub(crate) work_notes_baseline: Option<WorkNotesBaseline>,
    reference_context_item: Option<TurnContextItem>,
}

impl ContextManager {
    pub(crate) fn new() -> Self {
        Self {
            items: Vec::new(),
            item_token_estimates: Vec::new(),
            output_formatter: ShellOutputFormatter::default(),
            total_item_tokens: ProjectionCost::default(),
            original_covered_cost: ProjectionCost::default(),
            measured_item_tokens: None,
            measured_format_delta: None,
            measured_base_tokens: None,
            measured_tool_tokens: None,
            token_info: TokenUsageInfo::new_or_append(&None, &None, None),
            measured_surface: None,
            source_model: None,
            last_usage_item_count: 0,
            request_context_tokens: 0,
            work_notes_baseline: None,
            reference_context_item: None,
        }
    }

    pub(crate) fn token_info(&self) -> Option<TokenUsageInfo> {
        self.token_info.clone()
    }

    pub(crate) fn set_token_info(&mut self, info: Option<TokenUsageInfo>) {
        let covered_item_count = if info
            .as_ref()
            .is_some_and(|info| info.last_token_usage.total_tokens > 0)
        {
            self.last_model_item_count().unwrap_or(self.items.len())
        } else {
            0
        };
        self.set_token_info_with_boundary(info, covered_item_count);
    }

    pub(crate) fn set_token_info_with_boundary(
        &mut self,
        info: Option<TokenUsageInfo>,
        covered_item_count: usize,
    ) {
        self.token_info = info;
        self.source_model = None;
        self.measured_surface = None;
        self.last_usage_item_count = covered_item_count.min(self.items.len());
        self.original_covered_cost = if self.last_usage_item_count == 0 {
            ProjectionCost::default()
        } else {
            self.total_item_tokens.sub(self.uncovered_cost())
        };
        self.measured_item_tokens = None;
        self.measured_format_delta = None;
        self.measured_base_tokens = None;
        self.measured_tool_tokens = None;
        self.request_context_tokens = 0;
    }

    pub(crate) fn last_usage_item_count(&self) -> usize {
        self.last_usage_item_count
    }

    pub(crate) fn request_context_tokens(&self) -> i64 {
        self.request_context_tokens
    }

    pub(crate) fn mark_measured_projection(
        &mut self,
        provider: &str,
        endpoint: Option<&str>,
        projection: HelperProjection,
    ) {
        let plain = self
            .original_covered_cost
            .get(HelperProjection::plain(projection.images));
        self.measured_item_tokens = Some(plain);
        self.measured_format_delta = Some(self.original_covered_cost.get(projection) - plain);
        self.measured_surface = Some(MeasuredInputSurface {
            provider: provider.to_owned(),
            endpoint_sha256: endpoint.map(str::to_owned),
            images: projection.images,
            structured_outputs: projection.structured_outputs,
        });
    }

    pub(crate) fn base_delta(&self, base: &BaseInstructions) -> i64 {
        instruction_delta(base, self.measured_base_tokens)
    }

    pub(crate) fn usage_source_model(&self) -> Option<&str> {
        self.source_model.as_deref()
    }

    /// After a capture marker, only the frozen pre-marker receipt describes the sibling work.
    pub(crate) fn work_receipt_source(&self, marker: Option<usize>) -> Option<UsageSource<'_>> {
        if let Some(index) = marker {
            let frozen = self
                .work_notes_baseline
                .as_ref()
                .filter(|b| b.index == index)?;
            return Some(UsageSource {
                model: &frozen.model,
                provider: &frozen.provider,
                endpoint: Some(&frozen.endpoint),
            });
        }
        let surface = self.measured_surface.as_ref()?;
        Some(UsageSource {
            model: self.usage_source_model()?,
            provider: &surface.provider,
            endpoint: Some(surface.endpoint_sha256.as_deref()?),
        })
    }

    pub(crate) fn set_measured_context(&mut self, tokens: i64, source_model: Option<String>) {
        self.request_context_tokens = tokens;
        self.source_model = source_model;
    }

    pub(crate) fn work_usage_without_frames(&self, reasoning_included: bool) -> i64 {
        self.get_total_token_usage(reasoning_included)
            .saturating_sub(self.request_context_tokens)
            .max(0)
    }

    /// Fit a helper extending measured input on the same model. The capacity reserve
    /// for older reasoning is not another input delta. A new user boundary can change
    /// which reasoning is rendered, so do not extrapolate across an unmeasured one.
    pub(crate) fn helper_tokens_from_usage(
        &self,
        source: UsageSource<'_>,
        reasoning_included: bool,
        projection: HelperProjection,
    ) -> Option<i64> {
        self.projected_work_usage(source, reasoning_included, projection, WorkBudget::Helper)
    }

    pub(crate) fn review_transcript_tokens(
        &self,
        source: UsageSource<'_>,
        base: &BaseInstructions,
        reasoning_included: bool,
        projection: HelperProjection,
    ) -> Option<i64> {
        self.measured_base_tokens?;
        let tools = self.measured_tool_tokens?;
        Some(
            self.projected_work_usage(source, reasoning_included, projection, WorkBudget::Review)?
                .saturating_add(self.base_delta(base))
                .saturating_sub(tools),
        )
    }

    fn projected_work_usage(
        &self,
        source: UsageSource<'_>,
        reasoning_included: bool,
        projection: HelperProjection,
        budget: WorkBudget,
    ) -> Option<i64> {
        let usage = &self.token_info.as_ref()?.last_token_usage;
        if usage.total_tokens <= 0 {
            return None;
        }
        if usage.input_tokens == 0 {
            // Local replacement estimate, not a measurement of the old history.
            return self.usage_source_model().is_none().then(|| {
                self.work_usage_without_frames(reasoning_included)
                    .saturating_add(
                        self.total_item_tokens.get(projection) - self.total_item_tokens.raw,
                    )
                    .max(0)
            });
        }
        let surface = self.measured_surface.as_ref()?;
        if self.usage_source_model() != Some(source.model)
            || surface.provider != source.provider
            || surface.endpoint_sha256.as_deref()? != source.endpoint?
        {
            return None;
        }
        let previous_projection = HelperProjection {
            images: surface.images,
            structured_outputs: surface.structured_outputs,
        };
        let uncovered = self.uncovered_cost();
        let tail = uncovered.get(projection);
        let retained = self.total_item_tokens.sub(uncovered);
        let retained_then = retained.get(previous_projection);
        let projection_change = retained.get(projection).saturating_sub(retained_then);
        let measured = self
            .measured_item_tokens
            .unwrap_or(
                self.original_covered_cost
                    .get(HelperProjection::plain(previous_projection.images)),
            )
            .saturating_add(self.measured_format_delta.unwrap_or(
                if previous_projection.structured_outputs {
                    self.original_covered_cost.structured_delta
                } else {
                    0
                },
            ));
        // Cap the credit for discarded output, not the cost change on surviving input.
        let change = retained_then
            .saturating_sub(measured)
            .max(-usage.output_tokens.max(0))
            .saturating_add(projection_change);
        let reserve = if !reasoning_included
            && (budget == WorkBudget::Review
                || self.items[self.last_usage_item_count..]
                    .iter()
                    .any(is_user_turn_boundary))
        {
            self.get_non_last_reasoning_items_tokens()
        } else {
            0
        };
        Some(
            usage
                .total_tokens
                .saturating_add(change)
                .saturating_add(tail)
                .saturating_add(reserve)
                .saturating_sub(self.request_context_tokens)
                .max(0),
        )
    }

    pub(crate) fn work_notes_baseline_at(
        &self,
        index: usize,
        source: UsageSource<'_>,
    ) -> Option<&WorkNotesBaseline> {
        let endpoint = source.endpoint?;
        self.work_notes_baseline.as_ref().filter(|b| {
            b.index == index
                && b.model == source.model
                && b.provider == source.provider
                && b.endpoint == endpoint
        })
    }

    pub(crate) fn record_session_item(
        &mut self,
        item: &ResponseItem,
        policy: TruncationPolicy,
        reasoning_included: bool,
    ) {
        if crate::compact::is_work_notes_request_item(item) {
            self.work_notes_baseline = self.work_receipt_source(None).and_then(|source| {
                let work = |images, structured_outputs| {
                    self.helper_tokens_from_usage(
                        source,
                        reasoning_included,
                        HelperProjection {
                            images,
                            structured_outputs,
                        },
                    )
                };
                Some(WorkNotesBaseline {
                    index: self.items.len(),
                    model: source.model.to_owned(),
                    provider: source.provider.to_owned(),
                    endpoint: source.endpoint?.to_owned(),
                    work: [
                        [work(false, false)?, work(false, true)?],
                        [work(true, false)?, work(true, true)?],
                    ],
                    items: self.total_item_tokens,
                    measured_base_tokens: self.measured_base_tokens,
                    measured_tool_tokens: self.measured_tool_tokens,
                })
            });
        }
        self.record_items([item], policy);
    }

    pub(crate) fn reference_context_item(&self) -> Option<TurnContextItem> {
        self.reference_context_item.clone()
    }

    pub(crate) fn set_reference_context_item(&mut self, item: Option<TurnContextItem>) {
        self.reference_context_item = item;
    }

    pub(crate) fn set_token_usage_full(&mut self, context_window: i64) {
        self.source_model = None;
        self.measured_surface = None;
        self.measured_item_tokens = None;
        self.measured_format_delta = None;
        self.measured_base_tokens = None;
        self.measured_tool_tokens = None;
        self.last_usage_item_count = self.items.len();
        self.request_context_tokens = 0;
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
        items.retain(|item| {
            !matches!(
                item,
                ResponseItem::GhostSnapshot { .. } | ResponseItem::CompactionTrigger {}
            )
        });
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

        Some(base_tokens.saturating_add(self.total_item_tokens.raw))
    }

    pub(crate) fn projected_tokens(
        &self,
        base: &BaseInstructions,
        projection: HelperProjection,
    ) -> i64 {
        self.projected_prefix_tokens(self.items.len(), projection)
            .saturating_add(i64::try_from(approx_token_count(&base.text)).unwrap_or(i64::MAX))
    }

    pub(crate) fn projected_prefix_tokens(&self, end: usize, projection: HelperProjection) -> i64 {
        self.total_item_tokens.get(projection).saturating_sub(
            self.item_token_estimates[end..]
                .iter()
                .fold(0i64, |sum, cost| sum.saturating_add(cost.get(projection))),
        )
    }

    /// Start coverage at the installed history, preserving lifetime usage, not the last request.
    pub(crate) fn reestimate_usage(&mut self, base: &BaseInstructions, window: Option<i64>) {
        let Some(estimated) = self.estimate_token_count_with_base_instructions(base) else {
            return;
        };
        let mut info = self.token_info.take().unwrap_or(TokenUsageInfo {
            total_token_usage: TokenUsage::default(),
            last_token_usage: TokenUsage::default(),
            model_context_window: None,
        });
        info.last_token_usage = TokenUsage {
            total_tokens: estimated.max(0),
            ..Default::default()
        };
        info.model_context_window = window.or(info.model_context_window);
        self.set_token_info_with_boundary(Some(info), self.items.len());
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

    #[cfg(test)]
    pub(crate) fn remove_last_item(&mut self) -> bool {
        if let Some(removed) = self.pop_item() {
            self.remove_corresponding_for(&removed);
            true
        } else {
            false
        }
    }

    pub(crate) fn replace(&mut self, items: Vec<ResponseItem>) {
        self.work_notes_baseline = None;
        self.replace_items(items);
        self.set_token_info(self.token_info.clone());
    }

    pub(crate) fn replace_for_continuation(&mut self, items: Vec<ResponseItem>) {
        let unchanged_prefix = self
            .items
            .iter()
            .zip(&items)
            .take_while(|(old, new)| old == new)
            .count();
        // Continuation only removes items. Keep all surviving members of the measured
        // prefix covered, including the completed sibling of an earlier unmatched call.
        let mut covered = self.last_usage_item_count.min(unchanged_prefix);
        for original in &self.items[covered..self.last_usage_item_count] {
            if items.get(covered) == Some(original) {
                covered += 1;
            }
        }
        self.last_usage_item_count = covered;
        self.invalidate_work_notes_from(unchanged_prefix);
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
        input_item_count: usize,
    ) {
        let info = TokenUsageInfo::new_or_append(
            &self.token_info,
            &Some(usage.clone()),
            model_context_window,
        );
        let covered_item_count = self
            .last_model_item_count()
            .unwrap_or(0)
            .max(input_item_count);
        self.set_token_info_with_boundary(info, covered_item_count);
    }

    fn get_non_last_reasoning_items_tokens(&self) -> i64 {
        // Get reasoning items excluding all the ones after the last user turn boundary.
        let Some(last_user_index) = self.items.iter().rposition(is_user_turn_boundary) else {
            return 0;
        };

        self.items
            .iter()
            .zip(&self.item_token_estimates)
            // Unreported reasoning is already charged in the tail estimate.
            .take(last_user_index.min(self.last_usage_item_count))
            .filter(|item| {
                matches!(
                    item.0,
                    ResponseItem::Reasoning {
                        encrypted_content: Some(_),
                        ..
                    }
                )
            })
            .fold(0i64, |acc, (_, estimate)| acc.saturating_add(estimate.raw))
    }

    fn last_model_item_count(&self) -> Option<usize> {
        self.items
            .iter()
            .rposition(is_model_generated_item)
            .map(|index| index + 1)
    }

    /// When true, the server already accounted for past reasoning tokens and
    /// the client should not re-estimate them.
    pub(crate) fn get_total_token_usage(&self, server_reasoning_included: bool) -> i64 {
        let last_tokens = self
            .token_info
            .as_ref()
            .map(|info| info.last_token_usage.total_tokens)
            .unwrap_or(0);
        last_tokens.saturating_add(self.get_unreported_token_usage(server_reasoning_included))
    }

    fn uncovered_item_tokens(&self) -> i64 {
        self.uncovered_cost().raw
    }

    fn uncovered_cost(&self) -> ProjectionCost {
        self.item_token_estimates[self.last_usage_item_count..]
            .iter()
            .copied()
            .fold(ProjectionCost::default(), ProjectionCost::add)
    }

    pub(crate) fn get_unreported_token_usage(&self, server_reasoning_included: bool) -> i64 {
        let unreported_tokens = self.uncovered_item_tokens();
        if server_reasoning_included {
            unreported_tokens
        } else {
            self.get_non_last_reasoning_items_tokens()
                .saturating_add(unreported_tokens)
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
                    if role == "developer" && is_pre_turn_dev_message_content(content) =>
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
        self.last_usage_item_count = 0;
        self.rebuild_token_estimates();
    }

    fn push_item(&mut self, item: ResponseItem) {
        let token_estimate = ProjectionCost::of(&item, &mut self.output_formatter);
        self.total_item_tokens = self.total_item_tokens.add(token_estimate);
        self.item_token_estimates.push(token_estimate);
        self.items.push(item);
    }

    #[cfg(test)]
    fn pop_item(&mut self) -> Option<ResponseItem> {
        let index = self.items.len().checked_sub(1)?;
        Some(self.remove_item_at(index))
    }

    fn remove_item_at(&mut self, index: usize) -> ResponseItem {
        self.invalidate_work_notes_from(index);
        let token_estimate = self.item_token_estimates.remove(index);
        let item = self.items.remove(index);
        if let Some(call) = super::ToolCallId::from_call(&item) {
            let (super::ToolCallId::Function(id) | super::ToolCallId::Custom(id)) = call;
            self.output_formatter.forget(id);
        }
        self.total_item_tokens = self.total_item_tokens.sub(token_estimate);
        if index < self.last_usage_item_count {
            self.last_usage_item_count -= 1;
        }
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
        self.invalidate_work_notes_from(len);
        self.items.truncate(len);
        self.last_usage_item_count = self.last_usage_item_count.min(self.items.len());
        self.request_context_tokens = 0;
        self.rebuild_token_estimates();
    }

    fn invalidate_work_notes_from(&mut self, index: usize) {
        if self
            .work_notes_baseline
            .as_ref()
            .is_some_and(|b| b.index >= index)
        {
            self.work_notes_baseline = None;
        }
    }

    fn update_item_token_estimate(&mut self, index: usize) {
        self.invalidate_work_notes_from(index);
        let previous = self.item_token_estimates[index];
        // Image substitution changes a multipart result; it is not a shell text envelope.
        let updated = ProjectionCost::of(&self.items[index], &mut ShellOutputFormatter::default());
        self.item_token_estimates[index] = updated;
        self.total_item_tokens = self.total_item_tokens.sub(previous).add(updated);
    }

    fn rebuild_token_estimates(&mut self) {
        self.output_formatter = ShellOutputFormatter::default();
        self.item_token_estimates = self
            .items
            .iter()
            .map(|item| ProjectionCost::of(item, &mut self.output_formatter))
            .collect();
        self.total_item_tokens = self
            .item_token_estimates
            .iter()
            .copied()
            .fold(ProjectionCost::default(), ProjectionCost::add);
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
            | ResponseItem::CompactionTrigger {}
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
        ResponseItem::CompactionTrigger {} | ResponseItem::Other => false,
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

pub(crate) fn estimate_image_token_count() -> usize {
    usize::try_from(approx_tokens_from_byte_count(
        RESIZED_IMAGE_BYTES_ESTIMATE as usize,
    ))
    .unwrap_or(usize::MAX)
}

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
        // Sampled output can carry a second, encrypted representation of its tokens.
        // Charge the visible output once, not ciphertext, annotations or logprobs.
        ResponseItem::Message {
            role,
            content,
            encrypted_content,
            ..
        } if role == "assistant"
            || encrypted_content.is_some()
            || content
                .iter()
                .any(|part| matches!(part, ContentItem::OutputText { .. })) =>
        {
            content
                .iter()
                .map(|part| match part {
                    ContentItem::InputText { text } | ContentItem::OutputText { text, .. } => {
                        text.len() as i64
                    }
                    ContentItem::InputImage { image_url } => {
                        if parse_base64_image_data_url(image_url).is_some() {
                            RESIZED_IMAGE_BYTES_ESTIMATE
                        } else {
                            image_url.len() as i64
                        }
                    }
                })
                .sum()
        }
        ResponseItem::FunctionCall {
            name,
            arguments: input,
            ..
        }
        | ResponseItem::CustomToolCall { name, input, .. } => (name.len() + input.len()) as i64,
        ResponseItem::LocalShellCall { action, .. } => {
            serde_json::to_vec(action).map_or(0, |json| json.len() as i64)
        }
        ResponseItem::WebSearchCall { action, .. } => action
            .as_ref()
            .and_then(|action| serde_json::to_vec(action).ok())
            .map_or(0, |bytes| bytes.len() as i64),
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
        | ResponseItem::CompactionTrigger {}
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
