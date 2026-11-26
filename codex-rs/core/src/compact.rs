use std::sync::Arc;

use crate::Prompt;
use crate::client::ModelClientSession;
use crate::client_common::ResponseEvent;
use crate::completion_review::CaptureJournal;
use crate::completion_review::ReviewPromptFrames;
use crate::context_manager::ContextManager;
use crate::context_manager::ToolCallId;
use crate::error::CodexErr;
use crate::error::Result as CodexResult;
use crate::preserved_work_notes::PRESERVED_WORK_NOTES_MESSAGE_PREFIX;
use crate::protocol::EventMsg;
use crate::protocol::TurnStartedEvent;
use crate::protocol::WarningEvent;
use crate::session::session::Session;
use crate::session::turn::get_last_assistant_message_from_turn;
use crate::session::turn_context::TurnContext;
use crate::session_prefix::TURN_ABORTED_OPEN_TAG;
use crate::session_prefix::is_turn_aborted_marker;
use crate::stream_events_utils::last_assistant_message_from_item;
use crate::truncate::TruncationPolicy;
use crate::truncate::approx_token_count;
use crate::truncate::truncate_text;
use crate::util::backoff;
use codex_protocol::items::ContextCompactionItem;
use codex_protocol::items::TurnItem;
use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::user_input::UserInput;
use futures::prelude::*;
use indexmap::IndexSet;
use tracing::error;

pub const SUMMARIZATION_PROMPT: &str = include_str!("../templates/compact/prompt.md");
pub const SUMMARY_PREFIX: &str = include_str!("../templates/compact/summary_prefix.md");
pub(crate) const AUTO_COMPACT_WORK_NOTES_REQUEST_TAG: &str = "<AUTO_COMPACT_WORK_NOTES_REQUEST>";
pub(crate) const AUTO_COMPACT_WORK_NOTES_TAG: &str = "<AUTO_COMPACT_WORK_NOTES>";
pub(crate) const WORK_NOTES_SOURCE_MODEL_PREFIX: &str = "Compaction source model: ";
const COMPACT_USER_MESSAGE_MAX_TOKENS: usize = 20_000;

pub(crate) struct PreparedCompactionInput {
    pub(crate) source_history: ContextManager,
    pub(crate) preserved_work_notes: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) enum InitialContextInjection {
    DoNotInject,
    BeforeLastUserMessage,
}

/// Calibration of the transcript, excluding the advertised tools on both sides.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SourceFit {
    Local,
    Measured { transcript_adjustment: i64 },
}

impl SourceFit {
    pub(crate) fn adjustment(self, tools: &crate::client_common::ToolSurface) -> i64 {
        let transcript = match self {
            Self::Local => 0,
            Self::Measured {
                transcript_adjustment,
            } => transcript_adjustment,
        };
        transcript.saturating_add(tools.tokens())
    }
}

/// Freeze the correction and the route that earned it in the same snapshot.
/// Missing `source_fit` is a legacy decision, distinct from choosing local sizing.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct WorkFit {
    #[serde(default)]
    pub(crate) source_fit: Option<SourceFit>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) fit_route: Option<crate::model_provider_info::CheckpointRoute>,
}

impl WorkFit {
    pub(crate) fn local() -> Self {
        Self {
            source_fit: Some(SourceFit::Local),
            fit_route: None,
        }
    }

    fn adjustment(
        &self,
        actual_endpoint: Option<&str>,
        tools: &crate::client_common::ToolSurface,
    ) -> i64 {
        let fit = self
            .source_fit
            .filter(|_| {
                self.fit_route
                    .as_ref()
                    .is_some_and(|r| r.owns_endpoint(actual_endpoint))
            })
            .unwrap_or(SourceFit::Local);
        fit.adjustment(tools)
    }
}

#[derive(Debug, Clone)]
pub(crate) struct HelperFit {
    budget: HelperBudget,
    work: WorkFit,
}

#[derive(Debug, Clone)]
enum HelperBudget {
    Notes {
        window: Option<i64>,
        turn_id: String,
    },
    V2 {
        limit: Option<i64>,
    },
}

/// Keep the successful V2 wire input, not an independently refitted copy for installation.
pub(crate) struct SentCompactInput {
    pub(crate) items: Vec<ResponseItem>,
    pub(crate) rewritten_outputs: usize,
    pub(crate) estimated_deleted_tokens: i64,
}

impl HelperFit {
    pub(crate) fn apply(
        &self,
        items: &mut Vec<ResponseItem>,
        prompt: &Prompt,
        actual_endpoint: Option<&str>,
    ) -> Option<(usize, i64)> {
        let adjustment = self.work.adjustment(actual_endpoint, &prompt.tools);
        match &self.budget {
            HelperBudget::Notes { window, turn_id } => {
                crate::session::turn::fit_work_notes_input(
                    items,
                    &prompt.base_instructions,
                    *window,
                    adjustment,
                    turn_id,
                );
                None
            }
            HelperBudget::V2 { limit } => {
                let trim = crate::compact_remote::trim_function_call_history_to_fit_context_window(
                    items,
                    *limit,
                    &prompt.base_instructions,
                    adjustment,
                );
                items.push(ResponseItem::CompactionTrigger {});
                Some(trim)
            }
        }
    }
}

pub(crate) struct CompactionContext<'a> {
    pub(crate) request: &'a Arc<TurnContext>,
    pub(crate) replacement: &'a Arc<TurnContext>,
    /// Ordered declarations from this checkpoint's last completed notes request, if any.
    pub(crate) notes_tools: Option<crate::client_common::ToolSurface>,
    pub(crate) source_fit: WorkFit,
    /// Freeze the work frame across capture, retries and the subsequent compact request.
    pub(crate) review_frames: ReviewPromptFrames,
}

impl<'a> CompactionContext<'a> {
    pub(crate) fn new(context: &'a Arc<TurnContext>) -> Self {
        Self {
            request: context,
            replacement: context,
            notes_tools: None,
            source_fit: WorkFit::local(),
            review_frames: ReviewPromptFrames::default(),
        }
    }

    pub(crate) fn review_frame_tokens(&self) -> i64 {
        self.review_frames.tokens()
    }

    pub(crate) fn notes_fit(&self) -> HelperFit {
        self.helper_fit(HelperBudget::Notes {
            window: self.request.model_info.resolved_context_window(),
            turn_id: self.request.sub_id.clone(),
        })
    }

    pub(crate) fn v2_fit(&self) -> HelperFit {
        self.helper_fit(HelperBudget::V2 {
            limit: crate::compact_remote_v2::source_token_limit(self.request),
        })
    }

    fn helper_fit(&self, budget: HelperBudget) -> HelperFit {
        HelperFit {
            budget,
            work: if self
                .source_fit
                .fit_route
                .as_ref()
                .is_some_and(|r| r.provider == self.request.model_provider_id)
            {
                self.source_fit.clone()
            } else {
                WorkFit::local()
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CompactionImplementation {
    Local,
    Remote,
    RemoteV2,
}

impl CompactionImplementation {
    pub(crate) fn metric_label(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Remote => "remote",
            Self::RemoteV2 => "remote_v2",
        }
    }
}

pub(crate) fn compaction_implementation(context: &TurnContext) -> CompactionImplementation {
    use crate::config::types::CompactionMode;
    match (context.provider.is_openai(), context.config.compaction_mode) {
        (false, _) | (true, CompactionMode::Local) => CompactionImplementation::Local,
        (true, CompactionMode::Unary) => CompactionImplementation::Remote,
        (true, CompactionMode::Auto) => CompactionImplementation::RemoteV2,
    }
}

pub(crate) async fn run_inline_auto_compact_task(
    sess: Arc<Session>,
    context: &CompactionContext<'_>,
    preserved_work_notes: Option<String>,
    initial_context_injection: InitialContextInjection,
) -> CodexResult<bool> {
    let prompt = context.request.compact_prompt().to_string();
    let input = vec![UserInput::Text {
        text: prompt,
        // Compaction prompt is synthesized; no UI element ranges to preserve.
        text_elements: Vec::new(),
    }];

    run_compact_task_inner(
        sess,
        context,
        input,
        preserved_work_notes,
        initial_context_injection,
    )
    .await?;
    Ok(true)
}

pub(crate) async fn run_compact_task(
    sess: Arc<Session>,
    context: &CompactionContext<'_>,
    input: Vec<UserInput>,
) -> CodexResult<()> {
    let turn_context = context.request;
    let start_event = EventMsg::TurnStarted(TurnStartedEvent {
        model_context_window: turn_context.model_context_window(),
        collaboration_mode_kind: turn_context.collaboration_mode.mode,
    });
    sess.send_event(turn_context, start_event).await;
    run_compact_task_inner(
        sess.clone(),
        context,
        input,
        None,
        InitialContextInjection::DoNotInject,
    )
    .await
}

async fn run_compact_task_inner(
    sess: Arc<Session>,
    context: &CompactionContext<'_>,
    input: Vec<UserInput>,
    preserved_work_notes: Option<String>,
    initial_context_injection: InitialContextInjection,
) -> CodexResult<()> {
    let turn_context = context.request;
    let compaction_item = TurnItem::ContextCompaction(ContextCompactionItem::new());
    sess.emit_turn_item_started(turn_context, &compaction_item)
        .await;
    let initial_input_for_turn: ResponseInputItem = ResponseInputItem::from(input);

    let PreparedCompactionInput {
        source_history,
        preserved_work_notes,
    } = sess
        .prepare_history_for_compaction(preserved_work_notes, &context.review_frames)
        .await?;
    let mut history = source_history;
    history.record_items(
        &[initial_input_for_turn.into()],
        turn_context.truncation_policy,
    );

    let mut truncated_count = 0usize;

    let max_retries = turn_context.provider.stream_max_retries();
    let mut retries = 0;
    let turn_metadata_header = turn_context.resolve_turn_metadata_header().await;
    let mut client_session = sess
        .services
        .model_client
        .new_session_with_provider(turn_context.provider.clone());
    // Reuse one client session so turn-scoped state (sticky routing, websocket append tracking)
    // survives retries within this compact turn.

    let rollout_item = RolloutItem::TurnContext(turn_context.to_turn_context_item());
    sess.persist_rollout_items(&[rollout_item]).await;

    let completed_output = loop {
        // Clone is required because of the loop
        let turn_input = history
            .clone()
            .for_prompt_with_modalities(&turn_context.model_info.input_modalities);
        let turn_input_len = turn_input.len();
        let frame_tokens = context.review_frames.retained_tokens(&turn_input);
        let prompt = Prompt {
            input: turn_input,
            verbosity: Some(turn_context.verbosity),
            sampled_controls: turn_context.sampled_controls.clone(),
            base_instructions: sess.get_base_instructions_for_turn(turn_context).await,
            personality: turn_context.personality,
            ..Default::default()
        };
        let attempt_result = drain_to_completed(
            &sess,
            turn_context.as_ref(),
            &mut client_session,
            turn_metadata_header.as_deref(),
            &prompt,
            frame_tokens,
        )
        .await;

        match attempt_result {
            Ok(output) => {
                if truncated_count > 0 {
                    sess.notify_background_event(
                        turn_context.as_ref(),
                        format!(
                            "Trimmed {truncated_count} older thread item(s) before compacting so the prompt fits the model context window."
                        ),
                    )
                    .await;
                }
                break output;
            }
            Err(CodexErr::Interrupted) => {
                return Err(CodexErr::Interrupted);
            }
            Err(e @ CodexErr::ContextWindowExceeded) => {
                if turn_input_len > 1 {
                    // Trim from the beginning to preserve cache (prefix-based) and keep recent messages intact.
                    error!(
                        "Context window exceeded while compacting; removing oldest history item. Error: {e}"
                    );
                    history.remove_first_item();
                    truncated_count += 1;
                    retries = 0;
                    continue;
                }
                sess.set_total_tokens_full(turn_context.as_ref()).await;
                let event = EventMsg::Error(e.to_error_event(None));
                sess.send_event(turn_context, event).await;
                return Err(e);
            }
            Err(e) => {
                if retries < max_retries {
                    retries += 1;
                    let delay = backoff(retries);
                    sess.notify_stream_error(
                        turn_context.as_ref(),
                        format!("Reconnecting... {retries}/{max_retries}"),
                        e,
                    )
                    .await;
                    tokio::time::sleep(delay).await;
                    continue;
                } else {
                    let event = EventMsg::Error(e.to_error_event(None));
                    sess.send_event(turn_context, event).await;
                    return Err(e);
                }
            }
        }
    };

    let history_snapshot = sess.clone_history().await;
    let history_items = history_snapshot.raw_items();
    let summary_suffix =
        get_last_assistant_message_from_turn(&completed_output).unwrap_or_default();
    let summary_text = format!("{SUMMARY_PREFIX}\n{summary_suffix}");
    let user_messages = collect_user_messages(history_items);

    let mut new_history = build_compacted_history(Vec::new(), &user_messages, &summary_text);
    let reference_context_item = match initial_context_injection {
        InitialContextInjection::DoNotInject => None,
        InitialContextInjection::BeforeLastUserMessage => {
            let initial_context = sess.build_initial_context(context.replacement).await;
            insert_initial_context_before_last_real_user_message(&mut new_history, initial_context);
            Some(context.replacement.to_turn_context_item())
        }
    };
    if let Some(notes) = preserved_work_notes.as_ref() {
        new_history.push(preserved_work_notes_message(notes));
    }
    let ghost_snapshots: Vec<ResponseItem> = history_items
        .iter()
        .filter(|item| matches!(item, ResponseItem::GhostSnapshot { .. }))
        .cloned()
        .collect();
    new_history.extend(ghost_snapshots);
    client_session.reset_after_compaction();

    sess.replace_compacted_history(
        context.replacement,
        new_history,
        reference_context_item,
        summary_text,
    )
    .await;

    sess.emit_turn_item_completed(turn_context, compaction_item)
        .await;
    let warning = EventMsg::Warning(WarningEvent {
        message: "Heads up: Long threads and multiple compactions can cause the model to be less accurate. Start a new thread when possible to keep threads small and targeted.".to_string(),
    });
    sess.send_event(turn_context, warning).await;
    Ok(())
}

pub fn content_items_to_text(content: &[ContentItem]) -> Option<String> {
    let mut pieces = Vec::new();
    for item in content {
        match item {
            ContentItem::InputText { text } | ContentItem::OutputText { text, .. } => {
                if !text.is_empty() {
                    pieces.push(text.as_str());
                }
            }
            ContentItem::InputImage { .. } => {}
        }
    }
    if pieces.is_empty() {
        None
    } else {
        Some(pieces.join("\n"))
    }
}

pub(crate) fn collect_user_messages(items: &[ResponseItem]) -> Vec<String> {
    items
        .iter()
        .filter_map(|item| match crate::event_mapping::parse_turn_item(item) {
            Some(TurnItem::UserMessage(user)) => {
                if is_summary_message(&user.message())
                    || is_preserved_work_notes_message(&user.message())
                {
                    None
                } else {
                    Some(user.message())
                }
            }
            _ => collect_turn_aborted_marker(item),
        })
        .collect()
}

fn collect_turn_aborted_marker(item: &ResponseItem) -> Option<String> {
    let ResponseItem::Message { role, content, .. } = item else {
        return None;
    };
    if role != "user" {
        return None;
    }

    let text = content_items_to_text(content)?;
    if text
        .trim_start()
        .to_ascii_lowercase()
        .starts_with(TURN_ABORTED_OPEN_TAG)
    {
        Some(text)
    } else {
        None
    }
}

pub(crate) fn is_summary_message(message: &str) -> bool {
    message.starts_with(format!("{SUMMARY_PREFIX}\n").as_str())
}

pub(crate) fn is_preserved_work_notes_message(message: &str) -> bool {
    crate::preserved_work_notes::is_preserved_work_notes_message(message)
}

pub(crate) fn preserved_work_notes_message(work_notes: &str) -> ResponseItem {
    let text = format!("{PRESERVED_WORK_NOTES_MESSAGE_PREFIX}{work_notes}");
    ResponseItem::Message {
        encrypted_content: None,
        status: None,
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText { text }],
        end_turn: None,
        phase: None,
    }
}

pub(crate) fn build_compacted_history(
    initial_context: Vec<ResponseItem>,
    user_messages: &[String],
    summary_text: &str,
) -> Vec<ResponseItem> {
    build_compacted_history_with_limit(
        initial_context,
        user_messages,
        summary_text,
        COMPACT_USER_MESSAGE_MAX_TOKENS,
    )
}

pub(crate) fn insert_initial_context_before_last_real_user_message(
    history: &mut Vec<ResponseItem>,
    initial_context: Vec<ResponseItem>,
) {
    if initial_context.is_empty() {
        return;
    }

    let insert_idx = history
        .iter()
        .rposition(is_real_user_message_for_compaction)
        .or_else(|| {
            history.iter().rposition(|item| {
                matches!(item, ResponseItem::Message { role, content, .. }
                    if role == "user" && content_items_to_text(content).is_some_and(|text| is_summary_message(&text)))
            })
        })
        .or_else(|| {
            history
                .iter()
                .rposition(|item| matches!(item, ResponseItem::Compaction { .. }))
        })
        .unwrap_or(history.len());
    history.splice(insert_idx..insert_idx, initial_context);
}

fn is_real_user_message_for_compaction(item: &ResponseItem) -> bool {
    let ResponseItem::Message { role, content, .. } = item else {
        return false;
    };
    if role != "user" {
        return false;
    }
    !crate::event_mapping::is_contextual_user_message_content(content)
        && !content_items_to_text(content).is_some_and(|text| is_summary_message(&text))
}

fn build_compacted_history_with_limit(
    mut history: Vec<ResponseItem>,
    user_messages: &[String],
    summary_text: &str,
    max_tokens: usize,
) -> Vec<ResponseItem> {
    let mut selected_messages: Vec<String> = Vec::new();
    if max_tokens > 0 {
        let mut remaining = max_tokens;
        for message in user_messages.iter().rev() {
            if remaining == 0 {
                break;
            }
            let tokens = approx_token_count(message);
            if tokens <= remaining {
                selected_messages.push(message.clone());
                remaining = remaining.saturating_sub(tokens);
            } else {
                let truncated = truncate_text(message, TruncationPolicy::Tokens(remaining));
                selected_messages.push(truncated);
                break;
            }
        }
        selected_messages.reverse();
    }

    for message in &selected_messages {
        history.push(ResponseItem::Message {
            encrypted_content: None,
            status: None,
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: message.clone(),
            }],
            end_turn: None,
            phase: None,
        });
    }

    let summary_text = if summary_text.is_empty() {
        "(no summary available)".to_string()
    } else {
        summary_text.to_string()
    };

    history.push(ResponseItem::Message {
        encrypted_content: None,
        status: None,
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText { text: summary_text }],
        end_turn: None,
        phase: None,
    });

    history
}

pub(crate) fn prepare_history_for_compaction(
    mut history: ContextManager,
    preserved_work_notes: Option<String>,
    review_frames: Option<&ReviewPromptFrames>,
    journal: Option<&CaptureJournal>,
) -> CodexResult<PreparedCompactionInput> {
    let mut items = history.raw_items().to_vec();

    // Remove only the identified capture segment. Input acknowledged after a
    // failed attempt belongs to the next attempt's source, not to notes capture.
    let pending_notes = pending_work_notes(&items, journal)?;
    if let Some(pending) = &pending_notes {
        let controls = items[pending.request_index..pending.capture_end]
            .iter()
            .filter(|item| is_capture_control(item))
            .cloned()
            .collect::<Vec<_>>();
        items.splice(pending.request_index..pending.capture_end, controls);
    }

    if let Some(frames) = review_frames {
        frames.insert(
            &mut items,
            pending_notes.as_ref().map(|pending| pending.request_index),
        )?;
    }
    let preserved_work_notes = preserved_work_notes
        .and_then(|notes| (!notes.trim().is_empty()).then_some(notes))
        .or_else(|| pending_notes.and_then(|pending| pending.notes))
        .or_else(|| extract_existing_preserved_work_notes(&items));
    history.replace(items);

    Ok(PreparedCompactionInput {
        source_history: history,
        preserved_work_notes,
    })
}

pub(crate) const WORK_NOTES_CAPTURE_MAX_ATTEMPTS: u8 = 2;
pub(crate) const WORK_NOTES_TOOL_REJECT_REASON: &str =
    "tool use disabled during auto-compact work-notes capture";

#[derive(Debug, PartialEq)]
pub(crate) struct PendingWorkNotes {
    pub(crate) request_index: usize,
    pub(crate) capture_end: usize,
    pub(crate) source_model: Option<String>,
    pub(crate) notes: Option<String>,
    pub(crate) legacy_attempts: u8,
    pub(crate) missing_rejections: Vec<ResponseItem>,
}

pub(crate) fn tagged_work_notes(item: &ResponseItem) -> Option<String> {
    last_assistant_message_from_item(item, false)
        .filter(|text| text.trim_start().starts_with(AUTO_COMPACT_WORK_NOTES_TAG))
}

fn is_capture_control(item: &ResponseItem) -> bool {
    matches!(item, ResponseItem::GhostSnapshot { .. }) || is_turn_aborted_marker(item)
}

pub(crate) fn next_capture_offset(items: &[ResponseItem]) -> Option<usize> {
    let start = items.iter().rposition(is_work_notes_request_item)?;
    Some(
        items[start + 1..]
            .iter()
            .filter(|i| !is_capture_control(i))
            .count(),
    )
}

fn capture_rejection(call: ToolCallId<'_>) -> ResponseItem {
    match call {
        ToolCallId::Function(id) => ResponseItem::FunctionCallOutput {
            call_id: id.to_owned(),
            output: FunctionCallOutputPayload {
                body: FunctionCallOutputBody::Text(WORK_NOTES_TOOL_REJECT_REASON.to_owned()),
                success: Some(false),
            },
        },
        ToolCallId::Custom(id) => ResponseItem::CustomToolCallOutput {
            call_id: id.to_owned(),
            output: WORK_NOTES_TOOL_REJECT_REASON.to_owned(),
        },
    }
}

pub(crate) fn pending_work_notes(
    items: &[ResponseItem],
    journal: Option<&CaptureJournal>,
) -> CodexResult<Option<PendingWorkNotes>> {
    let Some(request_index) = items.iter().rposition(is_work_notes_request_item) else {
        return Ok(None);
    };
    let mut pending = PendingWorkNotes {
        request_index,
        capture_end: items.len(),
        source_model: work_notes_source_model(&items[request_index])?,
        notes: None,
        legacy_attempts: 0,
        missing_rejections: Vec::new(),
    };
    let mut calls = IndexSet::new();
    // Older clients used the ordinary failed-batch reason on interrupted notes streams
    // and wrapped rejections for calls their router recognized as MCP.
    let reasons = [
        WORK_NOTES_TOOL_REJECT_REASON,
        crate::tools::parallel::INCOMPLETE_TOOL_BATCH_REASON,
    ];
    let mcp_reasons = reasons.map(|reason| format!("err: {reason:?}"));
    let notes_remaining = journal.and_then(|journal| journal.notes_remaining);
    for (offset, (index, item)) in items
        .iter()
        .enumerate()
        .skip(request_index + 1)
        .filter(|(_, item)| !is_capture_control(item))
        .enumerate()
    {
        if crate::context_manager::is_user_turn_boundary(item) {
            pending.capture_end = index;
            break;
        }
        // Output items precede the batch's rejections. Never discard unrelated tool activity.
        let rejected_capture_output = match item {
            ResponseItem::FunctionCallOutput {
                call_id, output, ..
            } => {
                output.success != Some(true)
                    && output.text_content().is_some_and(|text| {
                        reasons.contains(&text) || mcp_reasons.iter().any(|r| r == text)
                    })
                    && calls.shift_remove(&ToolCallId::Function(call_id))
            }
            ResponseItem::CustomToolCallOutput {
                call_id, output, ..
            } => {
                reasons.contains(&output.as_str())
                    && calls.shift_remove(&ToolCallId::Custom(call_id))
            }
            _ => false,
        };
        if rejected_capture_output {
            continue;
        }
        if pending.notes.is_some() {
            if let Some(id) = ToolCallId::from_call(item)
                && journal.is_some_and(|journal| journal.owns_call(offset, item))
                && calls.insert(id)
            {
                continue;
            }
            // A response can finish with reasoning and an empty final message
            // after emitting notes in commentary. These are still capture
            // output, not subsequent work.
            let is_capture_tail = match item {
                ResponseItem::Reasoning { .. } => true,
                ResponseItem::WebSearchCall { .. } => notes_remaining.is_some(),
                ResponseItem::Message { role, content, .. } if role == "assistant" => {
                    content.iter().all(|part| {
                        matches!(part, ContentItem::OutputText { text, .. } if text.trim().is_empty())
                    })
                }
                _ => false,
            };
            if is_capture_tail {
                continue;
            }
            return Err(CodexErr::Fatal(
                "Work followed a completed compaction-notes capture; preserving history instead of discarding that work".into(),
            ));
        }
        let is_capture_item = match item {
            ResponseItem::Message { role, .. } if role == "assistant" => {
                pending.notes = tagged_work_notes(item);
                if notes_remaining.is_none() {
                    pending.legacy_attempts = pending.legacy_attempts.saturating_add(1);
                }
                pending.legacy_attempts <= WORK_NOTES_CAPTURE_MAX_ATTEMPTS
            }
            ResponseItem::FunctionCall { .. }
            | ResponseItem::CustomToolCall { .. }
            | ResponseItem::LocalShellCall { .. } => {
                ToolCallId::from_call(item).is_some_and(|id| calls.insert(id))
            }
            // Hosted search is service-produced output, not a local call awaiting a result.
            ResponseItem::Reasoning { .. } | ResponseItem::WebSearchCall { .. } => true,
            _ => false,
        };
        if !is_capture_item {
            return Err(CodexErr::Fatal(
                "Unrecognized work after a compaction-notes request; preserving history instead of truncating the suffix".into(),
            ));
        }
    }
    pending.missing_rejections = calls.into_iter().map(capture_rejection).collect();
    Ok(Some(pending))
}

fn work_notes_source_model(item: &ResponseItem) -> CodexResult<Option<String>> {
    let ResponseItem::Message { content, .. } = item else {
        return Ok(None);
    };
    let text = content_items_to_text(content).unwrap_or_default();
    text.trim_start()
        .lines()
        .nth(1)
        .and_then(|line| line.strip_prefix(WORK_NOTES_SOURCE_MODEL_PREFIX))
        .map(serde_json::from_str)
        .transpose()
        .map_err(|error| CodexErr::Fatal(format!("Invalid compaction capture model: {error}")))
}

pub(crate) fn is_work_notes_request_item(item: &ResponseItem) -> bool {
    let ResponseItem::Message { role, content, .. } = item else {
        return false;
    };
    if role != "developer" {
        return false;
    }

    content
        .iter()
        .filter_map(|part| match part {
            ContentItem::InputText { text } | ContentItem::OutputText { text, .. } => {
                Some(text.trim_start())
            }
            ContentItem::InputImage { .. } => None,
        })
        .find(|text| !text.is_empty())
        .is_some_and(|text| text.starts_with(AUTO_COMPACT_WORK_NOTES_REQUEST_TAG))
}

pub(crate) fn is_preserved_work_notes_item(item: &ResponseItem) -> bool {
    let ResponseItem::Message { role, content, .. } = item else {
        return false;
    };
    if role != "user" {
        return false;
    }
    let Some(text) = content_items_to_text(content) else {
        return false;
    };
    is_preserved_work_notes_message(&text)
}

pub(crate) fn extract_existing_preserved_work_notes(items: &[ResponseItem]) -> Option<String> {
    items.iter().rev().find_map(|item| {
        let ResponseItem::Message { role, content, .. } = item else {
            return None;
        };
        if role != "user" {
            return None;
        }
        let text = content_items_to_text(content)?;
        if !is_preserved_work_notes_message(&text) {
            return None;
        }
        text.strip_prefix(PRESERVED_WORK_NOTES_MESSAGE_PREFIX)
            .map(ToString::to_string)
    })
}

async fn drain_to_completed(
    sess: &Session,
    turn_context: &TurnContext,
    client_session: &mut ModelClientSession,
    turn_metadata_header: Option<&str>,
    prompt: &Prompt,
    frame_tokens: i64,
) -> CodexResult<Vec<ResponseItem>> {
    let mut stream = client_session
        .stream(
            prompt,
            &turn_context.model_info,
            &turn_context.otel_manager,
            turn_context.reasoning_effort,
            turn_context.reasoning_summary,
            turn_context.service_tier.clone(),
            turn_metadata_header,
        )
        .await?;
    // Keep uncommitted summarizer output out of the live notes capture. Each
    // retry gets a fresh buffer; only a completed attempt supplies the summary.
    let mut output = Vec::new();
    let mut server_reasoning_included = false;
    loop {
        let maybe_event = stream.next().await;
        let Some(event) = maybe_event else {
            return Err(CodexErr::Stream(
                "stream closed before response.completed".into(),
                None,
            ));
        };
        match event {
            Ok(ResponseEvent::OutputItemDone(item)) => {
                output.push(item);
            }
            Ok(ResponseEvent::ServerReasoningIncluded(included)) => {
                server_reasoning_included = included;
            }
            Ok(ResponseEvent::RateLimits(snapshot)) => {
                sess.update_rate_limits(turn_context, snapshot).await;
            }
            Ok(ResponseEvent::Completed { token_usage, .. }) => {
                sess.update_token_usage_info_with_request_context(
                    turn_context,
                    stream.usage(token_usage.as_ref()),
                    frame_tokens,
                    server_reasoning_included,
                    0,
                    prompt,
                )
                .await;
                return Ok(output);
            }
            Ok(_) => continue,
            Err(e) => return Err(e),
        }
    }
}

#[cfg(test)]
mod tests {

    use super::*;
    use crate::session_prefix::TURN_ABORTED_OPEN_TAG;
    use pretty_assertions::assert_eq;

    #[test]
    fn content_items_to_text_joins_non_empty_segments() {
        let items = vec![
            ContentItem::InputText {
                text: "hello".to_string(),
            },
            ContentItem::OutputText {
                annotations: None,
                logprobs: None,
                text: String::new(),
            },
            ContentItem::OutputText {
                annotations: None,
                logprobs: None,
                text: "world".to_string(),
            },
        ];

        let joined = content_items_to_text(&items);

        assert_eq!(Some("hello\nworld".to_string()), joined);
    }

    #[test]
    fn content_items_to_text_ignores_image_only_content() {
        let items = vec![ContentItem::InputImage {
            image_url: "file://image.png".to_string(),
        }];

        let joined = content_items_to_text(&items);

        assert_eq!(None, joined);
    }

    #[test]
    fn collect_user_messages_extracts_user_text_only() {
        let items = vec![
            ResponseItem::Message {
                encrypted_content: None,
                status: None,
                id: Some("assistant".to_string()),
                role: "assistant".to_string(),
                content: vec![ContentItem::OutputText {
                    annotations: None,
                    logprobs: None,
                    text: "ignored".to_string(),
                }],
                end_turn: None,
                phase: None,
            },
            ResponseItem::Message {
                encrypted_content: None,
                status: None,
                id: Some("user".to_string()),
                role: "user".to_string(),
                content: vec![ContentItem::InputText {
                    text: "first".to_string(),
                }],
                end_turn: None,
                phase: None,
            },
            ResponseItem::Other,
        ];

        let collected = collect_user_messages(&items);

        assert_eq!(vec!["first".to_string()], collected);
    }

    #[test]
    fn collect_user_messages_filters_session_prefix_entries() {
        let items = vec![
            ResponseItem::Message {
                encrypted_content: None, status: None, id: None,
                role: "user".to_string(),
                content: vec![ContentItem::InputText {
                    text: "# AGENTS.md instructions for project\n\n<INSTRUCTIONS>\ndo things\n</INSTRUCTIONS>"
                        .to_string(),
                }],
                end_turn: None,
            phase: None,
            },
            ResponseItem::Message {
                encrypted_content: None, status: None, id: None,
                role: "user".to_string(),
                content: vec![ContentItem::InputText {
                    text: "<ENVIRONMENT_CONTEXT>cwd=/tmp</ENVIRONMENT_CONTEXT>".to_string(),
                }],
                end_turn: None,
            phase: None,
            },
            ResponseItem::Message {
                encrypted_content: None, status: None, id: None,
                role: "user".to_string(),
                content: vec![ContentItem::InputText {
                    text: "real user message".to_string(),
                }],
                end_turn: None,
            phase: None,
            },
        ];

        let collected = collect_user_messages(&items);

        assert_eq!(vec!["real user message".to_string()], collected);
    }

    #[test]
    fn insert_initial_context_before_last_real_user_message_ignores_summary_and_work_notes() {
        let initial_context = vec![ResponseItem::Message {
            encrypted_content: None,
            status: None,
            id: None,
            role: "developer".to_string(),
            content: vec![ContentItem::InputText {
                text: "context".to_string(),
            }],
            end_turn: None,
            phase: None,
        }];
        let first_user = ResponseItem::Message {
            encrypted_content: None,
            status: None,
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: "first user".to_string(),
            }],
            end_turn: None,
            phase: None,
        };
        let last_user = ResponseItem::Message {
            encrypted_content: None,
            status: None,
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: "last user".to_string(),
            }],
            end_turn: None,
            phase: None,
        };
        let summary = ResponseItem::Message {
            encrypted_content: None,
            status: None,
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: format!("{SUMMARY_PREFIX}\nsummary"),
            }],
            end_turn: None,
            phase: None,
        };
        let work_notes = preserved_work_notes_message("notes");
        let mut history = vec![
            first_user.clone(),
            last_user.clone(),
            summary.clone(),
            work_notes.clone(),
        ];

        insert_initial_context_before_last_real_user_message(&mut history, initial_context.clone());

        assert_eq!(
            history,
            vec![
                first_user,
                initial_context[0].clone(),
                last_user,
                summary,
                work_notes,
            ]
        );
    }

    #[test]
    fn build_token_limited_compacted_history_truncates_overlong_user_messages() {
        // Use a small truncation limit so the test remains fast while still validating
        // that oversized user content is truncated.
        let max_tokens = 16;
        let big = "word ".repeat(200);
        let history = super::build_compacted_history_with_limit(
            Vec::new(),
            std::slice::from_ref(&big),
            "SUMMARY",
            max_tokens,
        );
        assert_eq!(history.len(), 2);

        let truncated_message = &history[0];
        let summary_message = &history[1];

        let truncated_text = match truncated_message {
            ResponseItem::Message { role, content, .. } if role == "user" => {
                content_items_to_text(content).unwrap_or_default()
            }
            other => panic!("unexpected item in history: {other:?}"),
        };

        assert!(
            truncated_text.contains("tokens truncated"),
            "expected truncation marker in truncated user message"
        );
        assert!(
            !truncated_text.contains(&big),
            "truncated user message should not include the full oversized user text"
        );

        let summary_text = match summary_message {
            ResponseItem::Message { role, content, .. } if role == "user" => {
                content_items_to_text(content).unwrap_or_default()
            }
            other => panic!("unexpected item in history: {other:?}"),
        };
        assert_eq!(summary_text, "SUMMARY");
    }

    #[test]
    fn build_token_limited_compacted_history_appends_summary_message() {
        let initial_context: Vec<ResponseItem> = Vec::new();
        let user_messages = vec!["first user message".to_string()];
        let summary_text = "summary text";

        let history = build_compacted_history(initial_context, &user_messages, summary_text);
        assert!(
            !history.is_empty(),
            "expected compacted history to include summary"
        );

        let last = history.last().expect("history should have a summary entry");
        let summary = match last {
            ResponseItem::Message { role, content, .. } if role == "user" => {
                content_items_to_text(content).unwrap_or_default()
            }
            other => panic!("expected summary message, found {other:?}"),
        };
        assert_eq!(summary, summary_text);
    }

    #[test]
    fn build_compacted_history_preserves_turn_aborted_markers() {
        let marker = format!(
            "{TURN_ABORTED_OPEN_TAG}\n  <turn_id>turn-1</turn_id>\n  <reason>interrupted</reason>\n</turn_aborted>"
        );
        let items = vec![
            ResponseItem::Message {
                encrypted_content: None,
                status: None,
                id: None,
                role: "user".to_string(),
                content: vec![ContentItem::InputText {
                    text: marker.clone(),
                }],
                end_turn: None,
                phase: None,
            },
            ResponseItem::Message {
                encrypted_content: None,
                status: None,
                id: None,
                role: "user".to_string(),
                content: vec![ContentItem::InputText {
                    text: "real user message".to_string(),
                }],
                end_turn: None,
                phase: None,
            },
        ];

        let user_messages = collect_user_messages(&items);
        let history = build_compacted_history(Vec::new(), &user_messages, "SUMMARY");

        let found_marker = history.iter().any(|item| match item {
            ResponseItem::Message { role, content, .. } if role == "user" => {
                content_items_to_text(content).is_some_and(|text| text == marker)
            }
            _ => false,
        });
        assert!(
            found_marker,
            "expected compacted history to retain <turn_aborted> marker"
        );
    }

    #[test]
    fn collect_user_messages_filters_preserved_work_notes_messages() {
        let items = vec![
            ResponseItem::Message {
                encrypted_content: None,
                status: None,
                id: None,
                role: "user".to_string(),
                content: vec![ContentItem::InputText {
                    text: "real user message".to_string(),
                }],
                end_turn: None,
                phase: None,
            },
            preserved_work_notes_message("notes"),
        ];

        let collected = collect_user_messages(&items);

        assert_eq!(vec!["real user message".to_string()], collected);
    }

    #[test]
    fn prepare_history_for_compaction_strips_work_notes_request_suffix() {
        let mut history = ContextManager::new();
        history.replace(vec![
            ResponseItem::Message {
                encrypted_content: None,
                status: None,
                id: None,
                role: "user".to_string(),
                content: vec![ContentItem::InputText {
                    text: "keep".to_string(),
                }],
                end_turn: None,
                phase: None,
            },
            preserved_work_notes_message("previous notes"),
            ResponseItem::Message {
                encrypted_content: None,
                status: None,
                id: None,
                role: "developer".to_string(),
                content: vec![ContentItem::InputText {
                    text: format!("{AUTO_COMPACT_WORK_NOTES_REQUEST_TAG}\nrequest"),
                }],
                end_turn: None,
                phase: None,
            },
            ResponseItem::Message {
                encrypted_content: None,
                status: None,
                id: None,
                role: "assistant".to_string(),
                content: vec![ContentItem::OutputText {
                    annotations: None,
                    logprobs: None,
                    text: "transient".to_string(),
                }],
                end_turn: None,
                phase: None,
            },
        ]);

        let work = history.raw_items()[..2].to_vec();
        let prepared = prepare_history_for_compaction(history, None, None, None).unwrap();

        assert_eq!(
            prepared.preserved_work_notes,
            Some("previous notes".to_string())
        );
        assert_eq!(prepared.source_history.raw_items(), work);
        assert_eq!(
            collect_user_messages(prepared.source_history.raw_items()),
            vec!["keep".to_string()]
        );
    }
}
