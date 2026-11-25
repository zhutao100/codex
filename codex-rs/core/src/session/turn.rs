use super::*;
use crate::compact::CompactionContext;
use crate::compact::WORK_NOTES_SOURCE_MODEL_PREFIX;
use crate::compact::WORK_NOTES_TOOL_REJECT_REASON;
use crate::compact::pending_work_notes;
use crate::compact_remote_v2::CompactionReason;
use crate::completion_review::AcceptedReviewInput;
use crate::completion_review::DeferredCaptureSource;
use crate::completion_review::ReviewSampling;
use crate::completion_review::WorkNotesCheckpoint;
use crate::context_manager::estimate_item_token_count;
use crate::session_prefix::is_turn_aborted_marker;
use crate::truncate::approx_token_count;
use crate::truncate::truncate_function_output_items_with_policy;
use crate::truncate::truncate_text;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputContentItem;
use std::sync::atomic::Ordering;

#[derive(Clone, Copy, Debug)]
enum FollowUpCompaction {
    Idle,
    Pending,
    AwaitingNotes,
    /// Recheck the projected prompt before sampling; do not compact it again without model progress.
    Compacted,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SamplingPurpose {
    Work,
    ExplorerReport,
    WorkNotes,
}

impl SamplingPurpose {
    fn tool_execution(self) -> ToolCallExecutionMode {
        match self {
            Self::Work => ToolCallExecutionMode::Normal,
            Self::ExplorerReport => ToolCallExecutionMode::RejectAll {
                reason: "Exploration evidence budget exhausted; report partial coverage.",
            },
            Self::WorkNotes => ToolCallExecutionMode::RejectAll {
                reason: WORK_NOTES_TOOL_REJECT_REASON,
            },
        }
    }

    fn cache_operation(self) -> crate::client::CacheOperation {
        use crate::client::CacheOperation;
        match self {
            Self::Work | Self::ExplorerReport => CacheOperation::Work,
            Self::WorkNotes => CacheOperation::Notes,
        }
    }
}

const AUTO_COMPACT_WORK_NOTES_OUTPUT_TOKEN_RESERVE_MAX: i64 = 4_096;
const AUTO_COMPACT_WORK_NOTES_OUTPUT_TOKEN_RESERVE_FRACTION: i64 = 20;
const AUTO_COMPACT_WORK_NOTES_MIN_TOOL_OUTPUT_TOKENS: usize = 128;

fn work_notes_request_item(turn_context: &TurnContext) -> ResponseItem {
    let source_model = serde_json::Value::String(turn_context.model_info.slug.clone());
    let request = format!(
        "{AUTO_COMPACT_WORK_NOTES_REQUEST_TAG}\n\
{WORK_NOTES_SOURCE_MODEL_PREFIX}{source_model}\n\
Token limit is approaching and follow-up work is still needed.\n\
\n\
Respond with exactly one assistant message and do not call tools.\n\
Put the notes in your final answer, not in commentary.\n\
Produce structured SESSION WORK NOTES for the next model after compaction.\n\
This is a checkpoint, not a replacement answer. Treat earlier notes as historical; prefer later evidence.\n\
\n\
Required sections:\n\
- Objective\n\
- Current status\n\
- Validated findings\n\
- Ruled-out hypotheses / dead ends\n\
- Open hypotheses / unresolved questions\n\
- Relevant files / why\n\
- Irrelevant files / why skip\n\
- Edits made\n\
- Edits in progress / intended edits\n\
- Next best step\n\
\n\
Keep it concise but loss-resistant.\n\
Begin with: {AUTO_COMPACT_WORK_NOTES_TAG}\n\
End with: </AUTO_COMPACT_WORK_NOTES>\n\
Put all notes inside these tags and do not add text outside them.\n\
</AUTO_COMPACT_WORK_NOTES_REQUEST>"
    );
    DeveloperInstructions::new(request).into()
}

async fn inject_pre_compact_work_notes_request(sess: &Session, turn_context: &TurnContext) {
    let message = work_notes_request_item(turn_context);
    sess.record_conversation_items(turn_context, std::slice::from_ref(&message))
        .await;
}

async fn captured_pre_compact_work_notes(
    sess: &Session,
    turn_context: &TurnContext,
) -> CodexResult<(Option<String>, u8)> {
    let (pending, remaining) = {
        let state = sess.state.lock().await;
        (
            pending_work_notes(state.history.raw_items()),
            state
                .completion_review
                .pending_compaction
                .as_ref()
                .and_then(|d| d.notes_remaining)
                .unwrap_or_default(),
        )
    };
    if let Err(error) = &pending {
        sess.send_event(turn_context, EventMsg::Error(error.to_error_event(None)))
            .await;
    }
    pending.map(|pending| (pending.and_then(|pending| pending.notes), remaining))
}

async fn sampling_input_for_pre_compact_work_notes(
    sess: &Session,
    context: &CompactionContext<'_>,
) -> CodexResult<Vec<ResponseItem>> {
    let turn_context = context.request;
    let mut input = sess.prompt_history(turn_context).await;
    let capture_start = input.iter().rposition(compact::is_work_notes_request_item);
    context.review_frames.insert(&mut input, capture_start)?;
    let base_instructions = sess.get_base_instructions_for_turn(turn_context).await;
    if let Some(stats) = trim_pre_compact_work_notes_input_to_headroom(
        &mut input,
        &base_instructions,
        // Use the literal context window here; work_notes_input_token_target
        // already reserves output headroom for the final work notes.
        turn_context.model_info.resolved_context_window(),
        context.source_token_adjustment,
    ) {
        info!(
            turn_id = %turn_context.sub_id,
            estimated_tokens_before = stats.estimated_tokens_before,
            estimated_tokens_after = stats.estimated_tokens_after,
            target_input_tokens = stats.target_input_tokens,
            "trimmed auto-compact work-notes prompt to reserve output headroom"
        );
    }
    Ok(input)
}

pub(crate) async fn compaction_source_token_adjustment(
    sess: &Session,
    turn_context: &TurnContext,
) -> i64 {
    let base = sess.get_base_instructions_for_turn(turn_context).await;
    let state = sess.state.lock().await;
    let capture_start = state
        .history
        .raw_items()
        .iter()
        .rposition(compact::is_work_notes_request_item);
    if let Some(start) = capture_start
        && let Some(baseline) = state
            .history
            .work_notes_baseline_at(start, &turn_context.model_info.slug)
    {
        return baseline.adjustment(&base);
    }
    // A response to an already fitted capture prompt does not measure the original work.
    if capture_start.is_none_or(|start| state.history.last_usage_item_count() <= start)
        && state
            .token_info()
            .is_some_and(|info| info.last_token_usage.input_tokens > 0)
        && let Some(tokens) = state.helper_tokens_from_usage(&turn_context.model_info.slug)
        && let Some(local) = state
            .history
            .estimate_token_count_with_base_instructions(&base)
    {
        tokens.saturating_sub(local)
    } else {
        0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WorkNotesPromptTrimStats {
    estimated_tokens_before: i64,
    estimated_tokens_after: i64,
    target_input_tokens: i64,
}

fn trim_pre_compact_work_notes_input_to_headroom(
    input: &mut [ResponseItem],
    base_instructions: &BaseInstructions,
    context_window: Option<i64>,
    source_token_adjustment: i64,
) -> Option<WorkNotesPromptTrimStats> {
    let context_window = context_window?;
    let target_input_tokens = work_notes_input_token_target(context_window)?;
    let estimated_tokens_before = estimate_work_notes_prompt_tokens(input, base_instructions)
        .saturating_add(source_token_adjustment);
    if estimated_tokens_before <= target_input_tokens {
        return None;
    }

    let trim_end = input
        .iter()
        .rposition(compact::is_work_notes_request_item)
        .unwrap_or(input.len());
    let mut estimated_tokens_after = estimated_tokens_before;
    for item in input[..trim_end].iter_mut().rev() {
        let excess = estimated_tokens_after - target_input_tokens;
        if excess <= 0 {
            break;
        }
        if let Some((shortened, savings)) = fit_work_notes_output(item, excess) {
            *item = shortened;
            estimated_tokens_after -= savings;
        }
    }

    (estimated_tokens_after < estimated_tokens_before).then_some(WorkNotesPromptTrimStats {
        estimated_tokens_before,
        estimated_tokens_after,
        target_input_tokens,
    })
}

fn work_notes_input_token_target(context_window: i64) -> Option<i64> {
    if context_window <= 1 {
        return None;
    }
    let reserve = if context_window < 1_024 {
        (context_window / 4).max(1)
    } else {
        (context_window / AUTO_COMPACT_WORK_NOTES_OUTPUT_TOKEN_RESERVE_FRACTION)
            .clamp(256, AUTO_COMPACT_WORK_NOTES_OUTPUT_TOKEN_RESERVE_MAX)
    };
    Some(context_window.saturating_sub(reserve).max(1))
}

/// Only an unsampled frame is optional in a checkpoint. Admit it only when both helper
/// projections still fit, including the marker/trigger that ordinary work does not send.
pub(super) fn unsampled_review_token_limit(context: &TurnContext) -> i64 {
    let compact_suffix = if compact::compaction_implementation(context)
        == compact::CompactionImplementation::RemoteV2
    {
        estimate_item_token_count(&ResponseItem::CompactionTrigger {})
    } else {
        0
    };
    let compact = context
        .model_context_window()
        .unwrap_or(i64::MAX)
        .saturating_sub(compact_suffix);
    if context.final_output_json_schema.is_none()
        && let Some(limit) = context
            .model_info
            .resolved_context_window()
            .and_then(work_notes_input_token_target)
    {
        compact
            .min(limit.saturating_sub(estimate_item_token_count(&work_notes_request_item(context))))
    } else {
        compact
    }
}

pub(super) fn estimate_work_notes_prompt_tokens(
    input: &[ResponseItem],
    base_instructions: &BaseInstructions,
) -> i64 {
    let base_tokens =
        i64::try_from(approx_token_count(&base_instructions.text)).unwrap_or(i64::MAX);
    input
        .iter()
        .map(estimate_item_token_count)
        .fold(base_tokens, i64::saturating_add)
}

fn fit_work_notes_output(item: &ResponseItem, excess: i64) -> Option<(ResponseItem, i64)> {
    let text_tokens = match item {
        ResponseItem::FunctionCallOutput { output, .. } => match &output.body {
            FunctionCallOutputBody::Text(text) => approx_token_count(text),
            FunctionCallOutputBody::ContentItems(items) => items
                .iter()
                .map(|item| match item {
                    FunctionCallOutputContentItem::InputText { text } => approx_token_count(text),
                    FunctionCallOutputContentItem::InputImage { .. } => 0,
                })
                .sum(),
        },
        ResponseItem::CustomToolCallOutput { output, .. } => approx_token_count(output),
        _ => return None,
    };
    if text_tokens <= AUTO_COMPACT_WORK_NOTES_MIN_TOOL_OUTPUT_TOKENS {
        return None;
    }
    let before = estimate_item_token_count(item);
    let mut budget = text_tokens
        .saturating_sub(usize::try_from(excess).unwrap_or(usize::MAX))
        .max(1);
    loop {
        // Fit the editable text, then charge the actual serialized delta including escapes and notices.
        let policy = TruncationPolicy::Tokens(budget);
        let mut shortened = item.clone();
        match &mut shortened {
            ResponseItem::FunctionCallOutput { output, .. } => match &mut output.body {
                FunctionCallOutputBody::Text(text) => *text = truncate_text(text, policy),
                FunctionCallOutputBody::ContentItems(items) => {
                    *items = truncate_function_output_items_with_policy(items, policy);
                }
            },
            ResponseItem::CustomToolCallOutput { output, .. } => {
                *output = truncate_text(output, policy)
            }
            _ => unreachable!(),
        }
        let savings = before - estimate_item_token_count(&shortened);
        if savings >= excess || budget == 1 {
            return (savings > 0).then_some((shortened, savings));
        }
        budget = budget
            .saturating_sub(usize::try_from(excess.saturating_sub(savings)).unwrap_or(usize::MAX))
            .max(1);
    }
}

/// Takes a user message as input and runs a loop where, at each sampling request, the model
/// replies with either:
///
/// - requested function calls
/// - an assistant message
///
/// While it is possible for the model to return multiple of these items in a
/// single sampling request, in practice, we generally one item per sampling request:
///
/// - If the model requests a function call, we execute it and send the output
///   back to the model in the next sampling request.
/// - If the model sends only an assistant message, we record it in the
///   conversation history and consider the turn complete.
///
pub(crate) async fn run_turn(
    sess: Arc<Session>,
    turn_context: Arc<TurnContext>,
    input: Vec<UserInput>,
    cancellation_token: CancellationToken,
) -> Option<String> {
    if input.is_empty() {
        return None;
    }

    run_turn_inner(sess, turn_context, input, None, cancellation_token).await
}

pub(crate) async fn continue_turn(
    sess: Arc<Session>,
    turn_context: Arc<TurnContext>,
    checkpoint: PendingContinuation,
    cancellation_token: CancellationToken,
) -> Option<String> {
    run_turn_inner(
        sess,
        turn_context,
        Vec::new(),
        Some(checkpoint),
        cancellation_token,
    )
    .await
}

async fn run_turn_inner(
    sess: Arc<Session>,
    turn_context: Arc<TurnContext>,
    input: Vec<UserInput>,
    continuation: Option<PendingContinuation>,
    cancellation_token: CancellationToken,
) -> Option<String> {
    use crate::agent::role::EXPLORER_DEADLINE;
    use crate::agent::role::EXPLORER_MAX_ACQUISITIONS;
    use crate::agent::role::EXPLORER_REPORT_RESERVE;
    use tokio::time::Instant;

    let explorer_deadline = turn_context
        .config
        .bounded_explorer
        .then(|| Instant::now() + EXPLORER_DEADLINE);
    if !sess
        .start_completion_review_main_task(&turn_context.sub_id, &turn_context)
        .await
    {
        sess.send_event(
            &turn_context,
            EventMsg::Error(
                CodexErr::Fatal(
                    "Main work could not persist its recovery owner. Repair persistence and resume from the last durable rollout. No model request was sent."
                        .into(),
                )
                .to_error_event(None),
            ),
        )
        .await;
        return None;
    }
    let incoming = turn_context;
    let incoming_limit = incoming
        .model_info
        .auto_compact_token_limit()
        .unwrap_or(i64::MAX);
    let owed = {
        let state = sess.state.lock().await;
        state.completion_review.owed_settings.clone().filter(|_| {
            state
                .completion_review
                .pending_inputs
                .iter()
                .any(AcceptedReviewInput::is_uninstalled_steering)
        })
    };
    let owed_context = if let Some(settings) = owed {
        match sess
            .turn_context_for_task_settings(&incoming, &settings)
            .await
        {
            Ok(context) => {
                let state = sess.state.lock().await;
                let review = &state.completion_review;
                let limit = context
                    .model_info
                    .auto_compact_token_limit()
                    .unwrap_or(i64::MAX)
                    .min(incoming_limit);
                (review.awaiting_compact_followup
                    || review.pending_compaction.is_some()
                    || review.defer_new_prompt_frame
                    || state.get_total_token_usage(state.server_reasoning_included()) >= limit
                    || state
                        .history
                        .raw_items()
                        .iter()
                        .rev()
                        .any(compact::is_work_notes_request_item))
                .then_some(context)
            }
            Err(error) => {
                sess.send_event(&incoming, EventMsg::Error(error.to_error_event(None)))
                    .await;
                return None;
            }
        }
    } else {
        None
    };
    let mut turn_context = owed_context.as_ref().unwrap_or(&incoming);
    let mut auto_compact_limit = turn_context
        .model_info
        .auto_compact_token_limit()
        .unwrap_or(i64::MAX);

    let event = EventMsg::TurnStarted(TurnStartedEvent {
        model_context_window: turn_context.model_context_window(),
        collaboration_mode_kind: turn_context.collaboration_mode.mode,
    });
    sess.send_event(turn_context, event).await;
    let turn_state = sess.turn_state_for_sub_id(&turn_context.sub_id).await;
    let mut recovery_revisions = sess.completion_review_input_recovery_revisions().await;
    if let Some(checkpoint) = continuation.as_ref() {
        let remove_interrupted_abort = checkpoint.source == TurnContinuationSource::Interrupted;
        sess.prepare_history_for_continuation(remove_interrupted_abort)
            .await;
        sess.clean_rollout_for_continuation(remove_interrupted_abort)
            .await;
        sess.send_event_raw_flushed(Event {
            id: turn_context.sub_id.clone(),
            msg: EventMsg::TurnContinued(TurnContinuedEvent {
                continued_from_turn_id: checkpoint.continued_from_turn_id.clone(),
                source: checkpoint.source,
            }),
        })
        .await;
    }
    let mut compacted_on_entry = false;
    let pending_notes = match pending_work_notes(sess.clone_history().await.raw_items()) {
        Ok(pending) => pending,
        Err(error) => {
            sess.send_event(turn_context, EventMsg::Error(error.to_error_event(None)))
                .await;
            return None;
        }
    };
    let (saved_source, queued_steering, fresh_before_admission) = {
        let state = sess.state.lock().await;
        let review = &state.completion_review;
        (
            review.recovery_capture_source(&turn_context.model_info.slug),
            review
                .pending_inputs
                .iter()
                .any(AcceptedReviewInput::is_uninstalled_steering),
            review
                .pending_inputs
                .iter()
                .any(AcceptedReviewInput::awaits_pre_turn_check),
        )
    };
    let injection = if continuation.is_some() || queued_steering {
        InitialContextInjection::BeforeLastUserMessage
    } else {
        InitialContextInjection::DoNotInject
    };
    if pending_notes.is_some() || saved_source.is_some() {
        // Selection is durable before the capture marker. Finish either checkpoint before
        // admitting pending input or re-evaluating the candidate against a wider window.
        let marker_model = pending_notes
            .as_ref()
            .and_then(|p| p.source_model.as_deref());
        let source = match &saved_source {
            Some(DeferredCaptureSource::Settings(source)) => {
                sess.turn_context_for_task_settings(turn_context, source)
                    .await
            }
            Some(DeferredCaptureSource::LegacyModel(model)) => {
                work_notes_request_context(&sess, turn_context, marker_model.or(Some(model))).await
            }
            None => work_notes_request_context(&sess, turn_context, marker_model).await,
        };
        let request_context = match source {
            Ok(context) => context,
            Err(error) => {
                sess.send_event(turn_context, EventMsg::Error(error.to_error_event(None)))
                    .await;
                return None;
            }
        };
        if let Err(error) = run_pre_turn_compact(
            &sess,
            &request_context,
            turn_context,
            injection,
            &cancellation_token,
        )
        .await
        {
            info!("Recovering compaction notes failed: {error:#}");
            return None;
        }
        compacted_on_entry = true;
    }
    // Reconcile on explicit continuation before measuring its source. Pending input has
    // precedence over an undelivered job and must not be appended to the work being captured.
    let resumed_job = if !compacted_on_entry
        && continuation.is_some()
        && recovery_revisions.is_empty()
        && turn_context.tools_config.exec_job_completion
        && !cancellation_token.is_cancelled()
        && let Some(state) = turn_state.as_ref()
    {
        crate::unified_exec::jobs::reconcile(&sess, turn_context, state, &cancellation_token).await
    } else {
        false
    };
    let total_usage_tokens = sess.get_total_token_usage().await;
    // An older steer may be waiting even before any compaction decision was journaled.
    // Check both the outgoing work and incoming capacity before choosing the first sampler.
    let at_limit = total_usage_tokens >= auto_compact_limit
        || (owed_context.is_some() && total_usage_tokens >= incoming_limit);
    let (over_limit_work, review_active, installed, admission_checked) = {
        let state = sess.state.lock().await;
        (
            at_limit
                && !state.completion_review.awaiting_compact_followup
                && (continuation.is_none()
                    || fresh_before_admission
                    || queued_steering
                    || resumed_job
                    || history_needs_continuation(state.history.raw_items())),
            state.completion_review.replacement_answer_required,
            state.completion_review.awaiting_compact_followup,
            continuation.is_some()
                && !fresh_before_admission
                && state.completion_review.unsampled_fresh_model.as_deref()
                    == Some(turn_context.model_info.slug.as_str()),
        )
    };
    // Until the destination actually samples work, even a review-budget checkpoint below
    // its regular trigger belongs to the larger model which measured the outgoing prefix.
    let check_outgoing = !compacted_on_entry && !installed && !admission_checked;
    let mut source_context = if check_outgoing && (at_limit || review_active) {
        match work_notes_request_context(&sess, turn_context, None).await {
            Ok(source) => source,
            Err(error) => {
                sess.send_event(turn_context, EventMsg::Error(error.to_error_event(None)))
                    .await;
                return None;
            }
        }
    } else {
        Arc::clone(turn_context)
    };
    let downshift = should_compact_with_previous_model(
        &source_context.model_info.slug,
        &turn_context.model_info.slug,
        total_usage_tokens,
        auto_compact_limit,
        source_context.model_context_window(),
        turn_context.model_context_window(),
    );
    if check_outgoing && (downshift || over_limit_work) {
        let injection = if fresh_before_admission && !queued_steering {
            InitialContextInjection::DoNotInject
        } else {
            injection
        };
        if let Err(error) = run_pre_turn_compact(
            &sess,
            &source_context,
            turn_context,
            injection,
            &cancellation_token,
        )
        .await
        {
            info!("Compaction before input admission failed: {error:#}");
            return None;
        }
        source_context = Arc::clone(turn_context);
        compacted_on_entry = true;
    }
    let mut compaction = if compacted_on_entry || installed {
        FollowUpCompaction::Compacted
    } else {
        if owed_context.is_some() {
            // No replacement means no intervening response: admit steer and fresh prompt together.
            turn_context = &incoming;
            source_context = Arc::clone(turn_context);
            auto_compact_limit = incoming_limit;
        }
        FollowUpCompaction::Idle
    };
    let (mut after_compaction, awaiting_compact_followup) = {
        let state = sess.state.lock().await;
        let barrier = state
            .completion_review
            .pending_inputs
            .iter()
            .find(|input| {
                state.completion_review.awaiting_compact_followup && input.is_uninstalled_steering()
            })
            .map(|i| i.revision);
        // A later accepted revision cannot overtake an earlier deferred one.
        let after = barrier.map_or_else(Vec::new, |revision| {
            recovery_revisions.split_off(recovery_revisions.partition_point(|r| *r < revision))
        });
        (after, state.completion_review.awaiting_compact_followup)
    };
    if !compacted_on_entry {
        sess.activate_mcp_context(turn_context).await;
    }
    admit_recovered_inputs(
        &sess,
        turn_context,
        &recovery_revisions,
        &cancellation_token,
    )
    .await?;

    let mut initial_input = (!input.is_empty()).then_some(input);
    let mut skill_names = if initial_input.is_none() || !after_compaction.is_empty() {
        connector_skill_names(&sess, turn_context).await
    } else {
        HashMap::new()
    };

    let mut check_advisory = true;
    let mut activate_before_admission = false;
    let mut last_agent_message: Option<String> = None;
    let mut can_drain_pending_input = initial_input.is_none() && !awaiting_compact_followup;
    // Although from the perspective of codex.rs, TurnDiffTracker has the lifecycle of a Task which contains
    // many turns, from the perspective of the user, it is a single turn.
    let turn_diff_tracker = Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new()));

    let mut turn_metadata_header = turn_context.resolve_turn_metadata_header().await;
    // `ModelClientSession` is turn-scoped and caches WebSocket + sticky routing state, so we reuse
    // one instance across retries within this turn.
    let mut client_session = sess
        .services
        .model_client
        .new_session_with_provider(turn_context.provider.clone());

    let mut explorer_rounds = 0;
    let mut explorer_limited = false;

    let mut compact_context = CompactionContext::new(turn_context);
    loop {
        if matches!(compaction, FollowUpCompaction::Pending) {
            if !Arc::ptr_eq(&source_context, turn_context)
                && RegularTaskSettings::from_turn(&source_context)
                    != RegularTaskSettings::from_turn(turn_context)
            {
                if let Err(error) = run_pre_turn_compact(
                    &sess,
                    &source_context,
                    turn_context,
                    InitialContextInjection::BeforeLastUserMessage,
                    &cancellation_token,
                )
                .await
                {
                    info!("Review checkpoint on the outgoing model failed: {error:#}");
                    return None;
                }
                source_context = Arc::clone(turn_context);
                compaction = FollowUpCompaction::Compacted;
                continue;
            }
            // Tool execution may have invalidated the sampled scope (e.g. human clarification).
            // Freeze the authoritative selection at the checkpoint, not the last loop iteration.
            compact_context.review_frames = match sess.compaction_review_frames(turn_context).await
            {
                Ok(frames) => frames,
                Err(error) => {
                    sess.send_event(turn_context, EventMsg::Error(error.to_error_event(None)))
                        .await;
                    return None;
                }
            };
            compact_context.source_token_adjustment =
                compaction_source_token_adjustment(&sess, turn_context).await;
            compaction = match begin_follow_up_compaction(
                &sess,
                &compact_context,
                &mut client_session,
                &cancellation_token,
            )
            .await
            {
                Ok(state) => state,
                Err(error) => {
                    info!("Auto-compaction failed during follow-up: {error:#}");
                    return None;
                }
            };
        }
        let capturing_notes = matches!(compaction, FollowUpCompaction::AwaitingNotes);
        // Neither capture nor the first ordinary sample of a replacement admits steering.
        // A final-output schema reaches Compacted directly, without an intervening notes round.
        can_drain_pending_input &= matches!(compaction, FollowUpCompaction::Idle);
        if can_drain_pending_input {
            if activate_before_admission {
                sess.activate_mcp_context(turn_context).await;
                skill_names = connector_skill_names(&sess, turn_context).await;
                activate_before_admission = false;
            }
            admit_recovered_inputs(&sess, turn_context, &after_compaction, &cancellation_token)
                .await?;
            after_compaction.clear();
        }
        if !capturing_notes
            && after_compaction.is_empty()
            && let Some(input) = initial_input.take()
        {
            let revision = sess
                .initial_completion_review_input_revision(&turn_context.sub_id)
                .await;
            skill_names =
                admit_new_turn_input(&sess, turn_context, &input, revision, &cancellation_token)
                    .await?;
            can_drain_pending_input = false;
        }
        let pending_input = if can_drain_pending_input {
            match turn_state.as_ref() {
                Some(turn_state) => sess.take_pending_input_for_turn_state(turn_state).await,
                None => Vec::new(),
            }
        } else {
            Vec::new()
        };
        if !pending_input.is_empty() {
            for pending_input_item in pending_input {
                if !sess
                    .install_pending_completion_review_input(turn_context, pending_input_item, true)
                    .await
                {
                    return None;
                }
            }
        }

        if check_advisory && !capturing_notes {
            match sess
                .admit_completion_review_advisory(
                    turn_context,
                    &source_context,
                    matches!(compaction, FollowUpCompaction::Idle),
                )
                .await
            {
                Ok(true) => check_advisory = false,
                Ok(false) => {
                    compaction = FollowUpCompaction::Pending;
                    continue;
                }
                Err(error) => {
                    sess.send_event(turn_context, EventMsg::Error(error.to_error_event(None)))
                        .await;
                    return None;
                }
            }
        }
        // Construct the input that we will send to the model.
        let mut sampling_request_input = if capturing_notes {
            match sampling_input_for_pre_compact_work_notes(&sess, &compact_context).await {
                Ok(input) => input,
                Err(error) => {
                    sess.send_event(turn_context, EventMsg::Error(error.to_error_event(None)))
                        .await;
                    return None;
                }
            }
        } else {
            sess.prompt_history(turn_context.as_ref()).await
        };
        let work_end = sampling_request_input.len();
        if let Some(deadline) = explorer_deadline
            && !capturing_notes
        {
            if Instant::now() >= deadline {
                explorer_limited = true;
                break;
            }
            explorer_rounds += 1;
            explorer_limited = explorer_rounds > EXPLORER_MAX_ACQUISITIONS
                || turn_context.exploration.exhausted()
                || Instant::now() + EXPLORER_REPORT_RESERVE >= deadline;
            if explorer_limited {
                sampling_request_input.push(
                    DeveloperInstructions::new(
                        "Exploration budget reached. Do not call tools. Report partial findings, source locators, and unchecked scope now.".to_string(),
                    ).into(),
                );
            }
        }
        if !capturing_notes {
            match sess
                .review_sampling(
                    turn_context,
                    &source_context,
                    &sampling_request_input[..work_end],
                    matches!(compaction, FollowUpCompaction::Idle),
                )
                .await
            {
                Ok(ReviewSampling::Ready(frames)) => {
                    if let Err(error) = frames.insert(&mut sampling_request_input, None) {
                        sess.send_event(turn_context, EventMsg::Error(error.to_error_event(None)))
                            .await;
                        return None;
                    }
                    compact_context.review_frames = frames;
                }
                Ok(ReviewSampling::Compact) => {
                    compaction = FollowUpCompaction::Pending;
                    continue;
                }
                Err(error) => {
                    sess.send_event(turn_context, EventMsg::Error(error.to_error_event(None)))
                        .await;
                    return None;
                }
            }
        }
        let request_context_tokens = compact_context.review_frame_tokens();

        let sampling_request_input_messages = sampling_request_input
            .iter()
            .filter_map(|item| match parse_turn_item(item) {
                Some(TurnItem::UserMessage(user_message)) => Some(user_message),
                _ => None,
            })
            .map(|user_message| user_message.message())
            .collect::<Vec<String>>();
        let purpose = if capturing_notes {
            SamplingPurpose::WorkNotes
        } else if explorer_limited {
            SamplingPurpose::ExplorerReport
        } else {
            SamplingPurpose::Work
        };
        let sampling_cancel = cancellation_token.child_token();
        let sampling = run_sampling_request(
            Arc::clone(&sess),
            Arc::clone(turn_context),
            turn_state.clone(),
            Arc::clone(&turn_diff_tracker),
            &mut client_session,
            turn_metadata_header.as_deref(),
            sampling_request_input,
            request_context_tokens,
            &skill_names,
            purpose,
            sampling_cancel.clone(),
        );
        let result = if let Some(deadline) = explorer_deadline {
            let cutoff = if explorer_limited && !capturing_notes {
                deadline
            } else {
                deadline - EXPLORER_REPORT_RESERVE
            };
            tokio::pin!(sampling);
            tokio::select! {
                result = &mut sampling => result,
                _ = tokio::time::sleep_until(cutoff) => {
                    // Keep polling the request after canceling it so recorded calls
                    // still receive their aborted results before the child reports.
                    sampling_cancel.cancel();
                    let _ = sampling.await;
                    if cancellation_token.is_cancelled() {
                        break;
                    }
                    if capturing_notes {
                        Err(CodexErr::Stream("Explorer work-notes deadline reached".into(), None))
                    } else if explorer_limited {
                        explorer_limited = true;
                        break;
                    } else {
                        continue;
                    }
                }
            }
        } else {
            sampling.await
        };
        match result {
            Ok(sampling_request_output) => {
                let SamplingRequestResult {
                    needs_follow_up,
                    last_agent_message: sampling_request_last_agent_message,
                } = sampling_request_output;
                let needs_follow_up =
                    needs_follow_up || !after_compaction.is_empty() || initial_input.is_some();
                if !capturing_notes && !Arc::ptr_eq(&source_context, turn_context) {
                    source_context = Arc::clone(turn_context);
                }
                if explorer_limited && !capturing_notes {
                    last_agent_message = sampling_request_last_agent_message;
                    break;
                }
                can_drain_pending_input = true;
                if needs_follow_up
                    && !capturing_notes
                    && turn_context.tools_config.exec_job_completion
                    && let Some(state) = turn_state.as_ref()
                {
                    crate::unified_exec::jobs::await_after_tools(
                        &sess,
                        turn_context,
                        state,
                        &cancellation_token,
                    )
                    .await;
                    if cancellation_token.is_cancelled() {
                        break;
                    }
                }
                let total_usage_tokens = sess.get_total_token_usage().await;
                let token_limit_reached = total_usage_tokens >= auto_compact_limit;

                let estimated_token_count =
                    sess.get_estimated_token_count(turn_context.as_ref()).await;

                info!(
                    turn_id = %turn_context.sub_id,
                    total_usage_tokens,
                    estimated_token_count = ?estimated_token_count,
                    auto_compact_limit,
                    token_limit_reached,
                    needs_follow_up,
                    "post sampling token usage"
                );

                if capturing_notes {
                    let (notes, remaining) =
                        match captured_pre_compact_work_notes(&sess, turn_context).await {
                            Ok(notes) => notes,
                            Err(_) => return None,
                        };
                    if notes.is_none() {
                        if remaining > 0 {
                            info!(
                                remaining,
                                "Work-notes capture yielded no notes; retrying capture"
                            );
                            continue;
                        }
                        info!("Work-notes capture exhausted; compacting without new notes");
                    }
                    if let Err(e) = run_auto_compact(
                        &sess,
                        &compact_context,
                        notes,
                        InitialContextInjection::BeforeLastUserMessage,
                        Some(&mut client_session),
                        CompactionReason::TokenLimit,
                        &cancellation_token,
                    )
                    .await
                    {
                        info!("Auto-compaction failed after work-notes capture: {e:#}");
                        return None;
                    }
                    compaction = FollowUpCompaction::Compacted;
                    continue;
                }

                // A successful ordinary round can add fresh tool work to compact.
                compaction = FollowUpCompaction::Idle;
                if !Arc::ptr_eq(turn_context, &incoming) {
                    // The old request completed before the deferred prompt or its settings
                    // could take effect. Keep its measured model as the outgoing source.
                    source_context = Arc::clone(turn_context);
                    if turn_context.model_provider_id != incoming.model_provider_id {
                        client_session = sess
                            .services
                            .model_client
                            .new_session_with_provider(incoming.provider.clone());
                    }
                    turn_context = &incoming;
                    compact_context = CompactionContext::new(turn_context);
                    turn_metadata_header = turn_context.resolve_turn_metadata_header().await;
                    activate_before_admission = true;
                    auto_compact_limit = turn_context
                        .model_info
                        .auto_compact_token_limit()
                        .unwrap_or(i64::MAX);
                }
                if needs_follow_up && total_usage_tokens >= auto_compact_limit {
                    compaction = FollowUpCompaction::Pending;
                    continue;
                }

                if !needs_follow_up {
                    last_agent_message = sampling_request_last_agent_message;
                    sess.hooks()
                        .dispatch(crate::hooks::HookPayload {
                            session_id: sess.conversation_id,
                            cwd: turn_context.cwd.clone(),
                            triggered_at: chrono::Utc::now(),
                            hook_event: HookEvent::AfterAgent {
                                event: HookEventAfterAgent {
                                    thread_id: sess.conversation_id,
                                    turn_id: turn_context.sub_id.clone(),
                                    input_messages: sampling_request_input_messages,
                                    last_assistant_message: last_agent_message.clone(),
                                },
                            },
                        })
                        .await;
                    break;
                }
                continue;
            }
            Err(CodexErr::TurnAborted) => {
                // Aborted turn is reported via a different event.
                break;
            }
            Err(e) if capturing_notes => {
                info!("Work-notes capture failed; compacting without notes: {e:#}");
                match run_auto_compact(
                    &sess,
                    &compact_context,
                    None,
                    InitialContextInjection::BeforeLastUserMessage,
                    Some(&mut client_session),
                    CompactionReason::TokenLimit,
                    &cancellation_token,
                )
                .await
                {
                    Ok(_) => {}
                    Err(compact_err) => {
                        info!(
                            "Auto-compaction failed after work-notes capture error: {compact_err:#}"
                        );
                        return None;
                    }
                }
                compaction = FollowUpCompaction::Compacted;
                continue;
            }
            Err(CodexErr::InvalidImageRequest()) => {
                let mut state = sess.state.lock().await;
                error_or_panic(
                    "Invalid image detected; sanitizing tool output to prevent poisoning",
                );
                if state.history.replace_last_turn_images("Invalid image") {
                    continue;
                }
                let event = EventMsg::Error(ErrorEvent {
                    message: "Invalid image in your last message. Please remove it and try again."
                        .to_string(),
                    codex_error_info: Some(CodexErrorInfo::BadRequest),
                    client_user_message_id: None,
                });
                sess.send_event(turn_context, event).await;
                break;
            }
            Err(e) => {
                info!("Turn error: {e:#}");
                let event = EventMsg::Error(e.to_error_event(None));
                sess.send_event(turn_context, event).await;
                // let the user continue the conversation
                break;
            }
        }
    }

    if turn_context.config.bounded_explorer {
        Some(
            turn_context
                .exploration
                .report(last_agent_message, explorer_limited),
        )
    } else {
        last_agent_message
    }
}

async fn admit_recovered_inputs(
    sess: &Arc<Session>,
    context: &Arc<TurnContext>,
    revisions: &[u64],
    cancellation: &CancellationToken,
) -> Option<()> {
    for &revision in revisions {
        let Some((fresh, prepared)) = sess
            .state
            .lock()
            .await
            .completion_review
            .pending_inputs
            .iter()
            .find(|input| input.revision == revision)
            .map(|input| {
                (
                    input.needs_fresh_admission().then(|| input.content.clone()),
                    input.has_prepared_prompt(),
                )
            })
        else {
            continue;
        };
        if let Some(input) = fresh {
            admit_new_turn_input(sess, context, &input, Some(revision), cancellation).await?;
        } else {
            if !prepared {
                sess.seed_initial_context_if_needed(context).await;
            }
            if !sess
                .install_completion_review_input(context, revision, true)
                .await
            {
                return None;
            }
        }
    }
    Some(())
}

/// Install a fresh user revision and its skill context together, in admission order.
async fn admit_new_turn_input(
    sess: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
    input: &[UserInput],
    revision: Option<u64>,
    cancellation_token: &CancellationToken,
) -> Option<HashMap<String, usize>> {
    let skills = sess
        .services
        .skills_manager
        .skills_for_cwd(&turn_context.cwd, false, &turn_context.config)
        .await;
    let (skill_name_counts, skill_names) =
        build_skill_name_counts(&skills.skills, &skills.disabled_paths);
    let connector_slug_counts = if turn_context.config.features.enabled(Feature::Apps) {
        let mcp_tools = sess
            .current_mcp_manager()
            .await
            .list_all_tools()
            .or_cancel(cancellation_token)
            .await
            .ok()?;
        let connectors = connectors::accessible_connectors_from_mcp_tools(&mcp_tools);
        build_connector_slug_counts(&connectors)
    } else {
        HashMap::new()
    };
    let mentioned_skills = collect_explicit_skill_mentions(
        input,
        &skills.skills,
        &skills.disabled_paths,
        &skill_name_counts,
        &connector_slug_counts,
    );

    maybe_prompt_and_install_mcp_dependencies(
        sess.as_ref(),
        turn_context.as_ref(),
        cancellation_token,
        &mentioned_skills,
    )
    .await;

    let otel_manager = turn_context.otel_manager.clone();
    let thread_id = sess.conversation_id.to_string();
    let tracking = build_track_events_context(turn_context.model_info.slug.clone(), thread_id);
    let SkillInjections {
        items: skill_items,
        warnings: skill_warnings,
    } = build_skill_injections(
        &mentioned_skills,
        Some(&otel_manager),
        &sess.services.analytics_events_client,
        tracking.clone(),
    )
    .await;

    for message in skill_warnings {
        sess.send_event(turn_context, EventMsg::Warning(WarningEvent { message }))
            .await;
    }

    if let Some(revision) = revision {
        if !sess
            .install_review_input_with_skills(turn_context, revision, true, Some(skill_items))
            .await
        {
            return None;
        }
    } else {
        sess.record_context_updates_and_set_reference_context_item(turn_context)
            .await;
        sess.state.lock().await.completion_review.explicit_app_paths =
            collect_explicit_app_paths(input);
        let response_item: ResponseItem = ResponseInputItem::from(input.to_vec()).into();
        sess.record_user_prompt_and_emit_turn_item(
            turn_context.as_ref(),
            input,
            response_item,
            None,
        )
        .await;
        if !skill_items.is_empty() {
            sess.record_conversation_items(turn_context, &skill_items)
                .await;
        }
    }
    Some(skill_names)
}

async fn begin_follow_up_compaction(
    sess: &Arc<Session>,
    context: &CompactionContext<'_>,
    client_session: &mut ModelClientSession,
    cancellation_token: &CancellationToken,
) -> CodexResult<FollowUpCompaction> {
    let decision = sess
        .checkpoint_auto_compaction(
            context.request,
            InitialContextInjection::BeforeLastUserMessage,
            context.source_token_adjustment,
            0,
        )
        .await?;
    if decision.notes_remaining.unwrap_or_default() > 0 {
        inject_pre_compact_work_notes_request(sess, context.request).await;
        return Ok(FollowUpCompaction::AwaitingNotes);
    }
    run_auto_compact(
        sess,
        context,
        None,
        InitialContextInjection::BeforeLastUserMessage,
        Some(client_session),
        CompactionReason::TokenLimit,
        cancellation_token,
    )
    .await?;
    Ok(FollowUpCompaction::Compacted)
}

async fn work_notes_request_context(
    sess: &Arc<Session>,
    current: &Arc<TurnContext>,
    source_model: Option<&str>,
) -> CodexResult<Arc<TurnContext>> {
    if let Some(model) = source_model {
        return if model == current.model_info.slug {
            Ok(Arc::clone(current))
        } else {
            sess.turn_context_with_model(current, model).await
        };
    }

    // Before a marker (or for a legacy marker), keep the measured, larger work model.
    // Fall back to the previous user-turn model only if the ledger has no owner.
    let source = sess
        .state
        .lock()
        .await
        .work_source_model()
        .map(str::to_owned);
    if let Some(previous) = source
        && previous != current.model_info.slug
    {
        let context = sess.turn_context_with_model(current, &previous).await?;
        if context
            .model_context_window()
            .zip(current.model_context_window())
            .is_some_and(|(source, target)| source > target)
        {
            return Ok(context);
        }
    }
    Ok(Arc::clone(current))
}

fn should_compact_with_previous_model(
    previous_model: &str,
    current_model: &str,
    total_usage_tokens: i64,
    current_auto_compact_limit: i64,
    previous_context_window: Option<i64>,
    current_context_window: Option<i64>,
) -> bool {
    if previous_model == current_model || total_usage_tokens < current_auto_compact_limit {
        return false;
    }

    matches!(
        (previous_context_window, current_context_window),
        (Some(previous), Some(current)) if previous > current
    )
}

async fn run_auto_compact(
    sess: &Arc<Session>,
    context: &CompactionContext<'_>,
    preserved_work_notes: Option<String>,
    initial_context_injection: InitialContextInjection,
    mut client_session: Option<&mut ModelClientSession>,
    reason: CompactionReason,
    cancellation_token: &CancellationToken,
) -> CodexResult<bool> {
    let turn_context = context.request;
    if cancellation_token.is_cancelled() {
        return Err(CodexErr::TurnAborted);
    }
    let implementation = compaction_implementation(turn_context);
    sess.advance_work_notes(WorkNotesCheckpoint::BeforeCompact)
        .await?;
    turn_context.otel_manager.counter(
        "codex.task.compact",
        1,
        &[("type", implementation.metric_label())],
    );
    let result = match implementation {
        CompactionImplementation::RemoteV2 => {
            return crate::compact_remote_v2::run_remote_compact_task(
                sess,
                context,
                client_session,
                preserved_work_notes,
                initial_context_injection,
                reason,
                cancellation_token,
            )
            .await;
        }
        CompactionImplementation::Remote => {
            run_inline_remote_auto_compact_task(
                Arc::clone(sess),
                context,
                preserved_work_notes,
                initial_context_injection,
            )
            .await
        }
        CompactionImplementation::Local => {
            run_inline_auto_compact_task(
                Arc::clone(sess),
                context,
                preserved_work_notes,
                initial_context_injection,
            )
            .await
        }
    };
    if result.as_ref().is_ok_and(|compacted| *compacted) {
        if !Arc::ptr_eq(context.request, context.replacement) {
            sess.activate_mcp_context(context.replacement).await;
        }
        if let Some(client_session) = client_session.as_mut() {
            client_session.reset_after_compaction();
        }
    }
    result
}

async fn run_pre_turn_compact(
    sess: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
    destination: &Arc<TurnContext>,
    injection: InitialContextInjection,
    cancellation_token: &CancellationToken,
) -> CodexResult<bool> {
    let mut context = CompactionContext {
        request: turn_context,
        ..CompactionContext::new(destination)
    };
    let reason = if turn_context
        .model_context_window()
        .zip(destination.model_context_window())
        .is_some_and(|(source, target)| source > target)
    {
        CompactionReason::ModelDownshift
    } else {
        CompactionReason::TokenLimit
    };
    if cancellation_token.is_cancelled() {
        return Err(CodexErr::TurnAborted);
    }
    sess.activate_mcp_context(turn_context).await;
    let (pending, has_following_input) = {
        let history = sess.clone_history().await;
        let pending = pending_work_notes(history.raw_items())?;
        let has_following_input = pending
            .as_ref()
            .is_some_and(|pending| pending.capture_end < history.raw_items().len());
        (pending, has_following_input)
    };
    context.review_frames = match sess.compaction_review_frames(turn_context).await {
        Ok(frame) => frame,
        Err(error) => {
            sess.send_event(turn_context, EventMsg::Error(error.to_error_event(None)))
                .await;
            return Err(error);
        }
    };
    context.source_token_adjustment = compaction_source_token_adjustment(sess, turn_context).await;
    let decision = sess
        .checkpoint_auto_compaction(
            turn_context,
            injection,
            context.source_token_adjustment,
            pending.as_ref().map_or(0, |p| p.attempts),
        )
        .await?;
    context.source_token_adjustment = decision.source_token_adjustment;
    let mut notes = pending.as_ref().and_then(|pending| pending.notes.clone());
    let mut remaining = decision.notes_remaining.unwrap_or_default();
    let mut client_session = sess
        .services
        .model_client
        .new_session_with_provider(turn_context.provider.clone());
    if notes.is_none() && remaining > 0 && !has_following_input {
        if pending.is_none() {
            inject_pre_compact_work_notes_request(sess, turn_context).await;
        }
        let tracker = Arc::new(Mutex::new(TurnDiffTracker::new()));
        let metadata = turn_context.resolve_turn_metadata_header().await;
        let turn_state = sess.turn_state_for_sub_id(&turn_context.sub_id).await;
        let skill_names = connector_skill_names(sess, turn_context).await;
        while remaining > 0 {
            let input = sampling_input_for_pre_compact_work_notes(sess, &context).await?;
            let output = run_sampling_request(
                Arc::clone(sess),
                Arc::clone(turn_context),
                turn_state.clone(),
                Arc::clone(&tracker),
                &mut client_session,
                metadata.as_deref(),
                input,
                context.review_frame_tokens(),
                &skill_names,
                SamplingPurpose::WorkNotes,
                cancellation_token.child_token(),
            )
            .await;
            match output {
                Ok(_) => {
                    (notes, remaining) =
                        captured_pre_compact_work_notes(sess, turn_context).await?;
                    if notes.is_some() {
                        break;
                    }
                }
                Err(error @ (CodexErr::TurnAborted | CodexErr::Interrupted)) => return Err(error),
                Err(error) => {
                    info!("Work-notes capture failed; compacting with existing notes: {error:#}");
                    break;
                }
            }
        }
    }
    run_auto_compact(
        sess,
        &context,
        notes,
        decision.placement,
        Some(&mut client_session),
        reason,
        cancellation_token,
    )
    .await
}

pub(super) fn history_needs_continuation(history: &[ResponseItem]) -> bool {
    if !matches!(pending_work_notes(history), Ok(None)) {
        return true;
    }
    let Some(last_user_index) = history
        .iter()
        .rposition(is_user_turn_boundary_response_item)
    else {
        return false;
    };

    tail_needs_continuation(&history[last_user_index + 1..])
}

pub(super) fn tail_needs_continuation(tail: &[ResponseItem]) -> bool {
    if tail.is_empty() || first_dangling_tool_call_index(tail).is_some() {
        return true;
    }

    let last_non_contextual = tail
        .iter()
        .rev()
        .find(|item| !is_contextual_state_item(item));
    match last_non_contextual {
        Some(ResponseItem::Message { role, .. }) if role == "assistant" => false,
        Some(ResponseItem::Message { .. }) => true,
        Some(
            ResponseItem::Reasoning { .. }
            | ResponseItem::LocalShellCall { .. }
            | ResponseItem::FunctionCall { .. }
            | ResponseItem::FunctionCallOutput { .. }
            | ResponseItem::CustomToolCall { .. }
            | ResponseItem::CustomToolCallOutput { .. }
            | ResponseItem::WebSearchCall { .. },
        ) => true,
        Some(_) => !tail.iter().any(|item| {
            matches!(
                item,
                ResponseItem::Message { role, .. } if role == "assistant"
            )
        }),
        None => true,
    }
}

fn is_contextual_state_item(item: &ResponseItem) -> bool {
    match item {
        ResponseItem::Message { role, content, .. } if role == "user" => {
            crate::event_mapping::is_contextual_user_message_content(content)
        }
        ResponseItem::Message { role, content, .. } if role == "developer" => {
            crate::event_mapping::is_contextual_dev_message_content(content)
        }
        _ => false,
    }
}

pub(super) fn is_review_rollout_user_message(item: &ResponseItem) -> bool {
    let ResponseItem::Message {
        id, role, content, ..
    } = item
    else {
        return false;
    };
    if role != "user" {
        return false;
    }

    id.as_deref() == Some("review_rollout_user")
        || content.iter().any(|item| match item {
            ContentItem::InputText { text } | ContentItem::OutputText { text, .. } => {
                text.contains("User initiated a review task.")
            }
            ContentItem::InputImage { .. } => false,
        })
}

pub(super) fn is_user_turn_boundary_response_item(item: &ResponseItem) -> bool {
    crate::context_manager::is_user_turn_boundary(item)
}

pub(super) fn remove_trailing_turn_aborted_marker(items: &mut Vec<ResponseItem>) -> bool {
    if items.last().is_some_and(is_turn_aborted_marker) {
        items.pop();
        true
    } else {
        false
    }
}

pub(super) fn trim_incomplete_continuation_tail(items: &mut Vec<ResponseItem>) -> bool {
    let Some(last_user_index) = items.iter().rposition(|item| {
        is_user_turn_boundary_response_item(item) || matches!(item, ResponseItem::Compaction { .. })
    }) else {
        return false;
    };

    let tail = &items[last_user_index + 1..];
    // Capture calls are rejected without execution; their outputs may not yet
    // be recorded. Notes recovery owns this segment and must keep exact notes.
    if tail.iter().any(compact::is_work_notes_request_item) {
        return false;
    }
    let Some(relative_index) = first_dangling_tool_call_index(tail) else {
        return false;
    };

    let snapshots = items
        .drain(last_user_index + 1 + relative_index..)
        .filter(|item| matches!(item, ResponseItem::GhostSnapshot { .. }))
        .collect::<Vec<_>>();
    items.extend(snapshots);
    true
}

fn first_dangling_tool_call_index(items: &[ResponseItem]) -> Option<usize> {
    let mut open_calls: HashMap<String, usize> = HashMap::new();
    let mut first_incomplete_local_shell: Option<usize> = None;

    for (index, item) in items.iter().enumerate() {
        match item {
            ResponseItem::FunctionCall { call_id, .. }
            | ResponseItem::CustomToolCall { call_id, .. } => {
                open_calls.entry(call_id.clone()).or_insert(index);
            }
            ResponseItem::FunctionCallOutput { call_id, .. }
            | ResponseItem::CustomToolCallOutput { call_id, .. } => {
                open_calls.remove(call_id);
            }
            ResponseItem::LocalShellCall { status, .. }
                if !matches!(status, codex_protocol::models::LocalShellStatus::Completed) =>
            {
                first_incomplete_local_shell =
                    Some(first_incomplete_local_shell.map_or(index, |current| current.min(index)));
            }
            _ => {}
        }
    }

    open_calls
        .values()
        .copied()
        .chain(first_incomplete_local_shell)
        .min()
}

pub(super) fn filter_connectors_for_input(
    connectors: Vec<connectors::AppInfo>,
    input: &[ResponseItem],
    explicit_app_paths: &[String],
    skill_name_counts_lower: &HashMap<String, usize>,
) -> Vec<connectors::AppInfo> {
    let user_messages = collect_user_messages(input);
    if user_messages.is_empty() && explicit_app_paths.is_empty() {
        return Vec::new();
    }

    let mentions = collect_tool_mentions_from_messages(&user_messages);
    let mention_names_lower = mentions
        .plain_names
        .iter()
        .map(|name| name.to_ascii_lowercase())
        .collect::<HashSet<String>>();

    let connector_slug_counts = build_connector_slug_counts(&connectors);
    let mut allowed_connector_ids: HashSet<String> = HashSet::new();
    for path in explicit_app_paths
        .iter()
        .chain(mentions.paths.iter())
        .filter(|path| tool_kind_for_path(path) == ToolMentionKind::App)
    {
        if let Some(connector_id) = app_id_from_path(path) {
            allowed_connector_ids.insert(connector_id.to_string());
        }
    }

    connectors
        .into_iter()
        .filter(|connector| {
            connector_inserted_in_messages(
                connector,
                &mention_names_lower,
                &allowed_connector_ids,
                &connector_slug_counts,
                skill_name_counts_lower,
            )
        })
        .collect()
}

fn connector_inserted_in_messages(
    connector: &connectors::AppInfo,
    mention_names_lower: &HashSet<String>,
    allowed_connector_ids: &HashSet<String>,
    connector_slug_counts: &HashMap<String, usize>,
    skill_name_counts_lower: &HashMap<String, usize>,
) -> bool {
    if allowed_connector_ids.contains(&connector.id) {
        return true;
    }

    let mention_slug = connectors::connector_mention_slug(connector);
    let connector_count = connector_slug_counts
        .get(&mention_slug)
        .copied()
        .unwrap_or(0);
    let skill_count = skill_name_counts_lower
        .get(&mention_slug)
        .copied()
        .unwrap_or(0);
    connector_count == 1 && skill_count == 0 && mention_names_lower.contains(&mention_slug)
}

fn filter_codex_apps_mcp_tools(
    mut mcp_tools: HashMap<String, crate::mcp_connection_manager::ToolInfo>,
    connectors: &[connectors::AppInfo],
) -> HashMap<String, crate::mcp_connection_manager::ToolInfo> {
    let allowed: HashSet<&str> = connectors
        .iter()
        .map(|connector| connector.id.as_str())
        .collect();

    mcp_tools.retain(|_, tool| {
        if tool.server_name != CODEX_APPS_MCP_SERVER_NAME {
            return true;
        }
        let Some(connector_id) = codex_apps_connector_id(tool) else {
            return false;
        };
        allowed.contains(connector_id)
    });

    mcp_tools
}

fn codex_apps_connector_id(tool: &crate::mcp_connection_manager::ToolInfo) -> Option<&str> {
    tool.connector_id.as_deref()
}

#[allow(clippy::too_many_arguments)]
#[instrument(level = "trace",
    skip_all,
    fields(
        turn_id = %turn_context.sub_id,
        model = %turn_context.model_info.slug,
        cwd = %turn_context.cwd.display()
    )
)]
async fn run_sampling_request(
    sess: Arc<Session>,
    turn_context: Arc<TurnContext>,
    turn_state: Option<Arc<Mutex<TurnState>>>,
    turn_diff_tracker: SharedTurnDiffTracker,
    client_session: &mut ModelClientSession,
    turn_metadata_header: Option<&str>,
    input: Vec<ResponseItem>,
    request_context_tokens: i64,
    skill_names: &HashMap<String, usize>,
    purpose: SamplingPurpose,
    cancellation_token: CancellationToken,
) -> CodexResult<SamplingRequestResult> {
    client_session.cache_operation = purpose.cache_operation();
    let mut input_item_count = sess.state.lock().await.history.raw_items().len();
    let router = sampling_tool_router(
        &sess,
        &turn_context,
        &input,
        skill_names,
        &cancellation_token,
    )
    .await?;
    let tools = router.specs();
    client_session.sampling_tools = Some((turn_context.model_info.slug.clone(), tools.clone()));

    let model_supports_parallel = turn_context.model_info.supports_parallel_tool_calls;

    let base_instructions = sess.get_base_instructions_for_turn(&turn_context).await;

    let mut prompt = Prompt {
        input,
        tools,
        parallel_tool_calls: model_supports_parallel,
        base_instructions,
        personality: turn_context.personality,
        output_schema: turn_context.final_output_json_schema.clone(),
    };

    let mut retries = 0;
    client_session.cache_attempt = 0;
    loop {
        client_session.cache_attempt += 1;
        let err = match try_run_sampling_request(
            Arc::clone(&router),
            Arc::clone(&sess),
            Arc::clone(&turn_context),
            turn_state.clone(),
            client_session,
            turn_metadata_header,
            Arc::clone(&turn_diff_tracker),
            &prompt,
            request_context_tokens,
            input_item_count,
            purpose,
            cancellation_token.child_token(),
        )
        .await
        {
            Ok(output) => {
                return Ok(output);
            }
            Err(CodexErr::ContextWindowExceeded) => {
                sess.set_total_tokens_full(&turn_context).await;
                return Err(CodexErr::ContextWindowExceeded);
            }
            Err(CodexErr::UsageLimitReached(e)) => {
                let rate_limits = e.rate_limits.clone();
                if let Some(rate_limits) = rate_limits {
                    sess.update_rate_limits(&turn_context, rate_limits).await;
                }
                return Err(CodexErr::UsageLimitReached(e));
            }
            Err(err) => err,
        };

        if !err.is_retryable() {
            return Err(err);
        }

        // Keep the existing request prefix, including request-only review
        // instructions. A failed response may have recorded rejected call/result
        // pairs; retry with those items rather than pretending they do not exist.
        let history = sess.clone_history().await;
        let items = history.raw_items();
        if let Some(suffix) = items.get(input_item_count..) {
            prompt
                .input
                .extend(ContextManager::prepare_items_for_prompt_with_modalities(
                    suffix.to_vec(),
                    &turn_context.model_info.input_modalities,
                ));
            input_item_count = items.len();
        }
        if matches!(err, CodexErr::Stream(..)) {
            client_session.drop_websocket_connection();
        }

        // Use the configured provider-specific stream retry budget.
        let max_retries = turn_context.provider.stream_max_retries();
        if retries >= max_retries
            && client_session.try_switch_fallback_transport(&turn_context.otel_manager)
        {
            sess.send_event(
                &turn_context,
                EventMsg::Warning(WarningEvent {
                    message: format!("Falling back from WebSockets to HTTPS transport. {err:#}"),
                }),
            )
            .await;
            retries = 0;
            continue;
        }
        if retries < max_retries {
            retries += 1;
            let delay = match &err {
                CodexErr::Stream(_, requested_delay) => {
                    requested_delay.unwrap_or_else(|| backoff(retries))
                }
                _ => backoff(retries),
            };
            warn!(
                "stream disconnected - retrying sampling request ({retries}/{max_retries} in {delay:?})...",
            );

            // Surface retry information to any UI/front‑end so the
            // user understands what is happening instead of staring
            // at a seemingly frozen screen.
            sess.notify_stream_error(
                &turn_context,
                format!("Reconnecting... {retries}/{max_retries}"),
                err,
            )
            .await;

            tokio::time::sleep(delay)
                .or_cancel(&cancellation_token)
                .await?;
        } else {
            return Err(err);
        }
    }
}

async fn connector_skill_names(sess: &Session, context: &TurnContext) -> HashMap<String, usize> {
    if !context.features.enabled(Feature::Apps) {
        return HashMap::new();
    }
    let skills = sess
        .services
        .skills_manager
        .skills_for_cwd(&context.cwd, false, &context.config)
        .await;
    build_skill_name_counts(&skills.skills, &skills.disabled_paths).1
}

async fn sampling_tool_router(
    sess: &Session,
    turn_context: &TurnContext,
    input: &[ResponseItem],
    skill_names: &HashMap<String, usize>,
    cancellation_token: &CancellationToken,
) -> CodexResult<Arc<ToolRouter>> {
    let mcp_connection_manager = sess.current_mcp_manager().await;
    let has_mcp_servers = mcp_connection_manager.has_servers();
    let mut mcp_tools = mcp_connection_manager
        .list_all_tools()
        .or_cancel(cancellation_token)
        .await?;
    let hosted_binding = mcp_connection_manager.hosted_binding();
    let connectors_for_tools = if turn_context.config.features.enabled(Feature::Apps) {
        let connectors = connectors::accessible_connectors_from_mcp_tools(&mcp_tools);
        let state = sess.state.lock().await;
        Some(filter_connectors_for_input(
            connectors,
            input,
            &state.completion_review.explicit_app_paths,
            skill_names,
        ))
    } else {
        None
    };
    if let Some(connectors) = connectors_for_tools.as_ref() {
        mcp_tools = filter_codex_apps_mcp_tools(mcp_tools, connectors);
    }
    Ok(Arc::new(ToolRouter::from_config(
        &turn_context.tools_config,
        has_mcp_servers.then_some(mcp_tools),
        turn_context.dynamic_tools.as_slice(),
        hosted_binding,
    )))
}

pub(crate) async fn compaction_tool_specs(
    sess: &Session,
    turn_context: &TurnContext,
    input: &[ResponseItem],
    cancellation_token: &CancellationToken,
) -> CodexResult<Vec<crate::client_common::tools::ToolSpec>> {
    sess.activate_mcp_context(turn_context).await;
    let skill_names = connector_skill_names(sess, turn_context).await;
    let router =
        sampling_tool_router(sess, turn_context, input, &skill_names, cancellation_token).await?;
    Ok(router.specs())
}

#[derive(Debug)]
struct SamplingRequestResult {
    needs_follow_up: bool,
    last_agent_message: Option<String>,
}

/// Ephemeral per-response state for streaming a single proposed plan.
/// This is intentionally not persisted or stored in session/state since it
/// only exists while a response is actively streaming. The final plan text
/// is extracted from the completed assistant message.
/// Tracks a single proposed plan item across a streaming response.
struct ProposedPlanItemState {
    item_id: String,
    started: bool,
    completed: bool,
}

/// Per-item plan parsers so we can buffer text while detecting `<proposed_plan>`
/// tags without ever mixing buffered lines across item ids.
struct PlanParsers {
    assistant: HashMap<String, ProposedPlanParser>,
}

impl PlanParsers {
    fn new() -> Self {
        Self {
            assistant: HashMap::new(),
        }
    }

    fn assistant_parser_mut(&mut self, item_id: &str) -> &mut ProposedPlanParser {
        self.assistant
            .entry(item_id.to_string())
            .or_insert_with(ProposedPlanParser::new)
    }

    fn take_assistant_parser(&mut self, item_id: &str) -> Option<ProposedPlanParser> {
        self.assistant.remove(item_id)
    }

    fn drain_assistant_parsers(&mut self) -> Vec<(String, ProposedPlanParser)> {
        self.assistant.drain().collect()
    }
}

/// Aggregated state used only while streaming a plan-mode response.
/// Includes per-item parsers, deferred agent message bookkeeping, and the plan item lifecycle.
struct PlanModeStreamState {
    /// Per-item parsers for assistant streams in plan mode.
    plan_parsers: PlanParsers,
    /// Agent message items started by the model but deferred until we see non-plan text.
    pending_agent_message_items: HashMap<String, TurnItem>,
    /// Agent message items whose start notification has been emitted.
    started_agent_message_items: HashSet<String>,
    /// Leading whitespace buffered until we see non-whitespace text for an item.
    leading_whitespace_by_item: HashMap<String, String>,
    /// Tracks plan item lifecycle while streaming plan output.
    plan_item_state: ProposedPlanItemState,
}

impl PlanModeStreamState {
    fn new(turn_id: &str) -> Self {
        Self {
            plan_parsers: PlanParsers::new(),
            pending_agent_message_items: HashMap::new(),
            started_agent_message_items: HashSet::new(),
            leading_whitespace_by_item: HashMap::new(),
            plan_item_state: ProposedPlanItemState::new(turn_id),
        }
    }
}

#[derive(Default)]
struct ProgressTraceStreamState {
    saw_work_category: bool,
    pending_prefill: bool,
    prefill_open: bool,
    reasoning_open: bool,
    gen_open: bool,
}

impl ProgressTraceStreamState {
    async fn mark_work_seen(&mut self, sess: &Session, turn_context: &TurnContext) {
        if self.saw_work_category {
            return;
        }
        self.saw_work_category = true;
        if self.pending_prefill && !self.prefill_open {
            sess.emit_progress_trace(
                turn_context,
                ProgressTraceCategory::Prefill,
                ProgressTraceState::Started,
                None,
                Some("stream"),
            )
            .await;
            self.prefill_open = true;
        }
    }

    async fn on_text_delta(&mut self, sess: &Session, turn_context: &TurnContext) {
        if self.saw_work_category {
            if !self.gen_open {
                sess.emit_progress_trace(
                    turn_context,
                    ProgressTraceCategory::Gen,
                    ProgressTraceState::Started,
                    None,
                    Some("stream"),
                )
                .await;
                self.gen_open = true;
            }
            return;
        }
        self.pending_prefill = true;
    }

    async fn on_reasoning_delta(&mut self, sess: &Session, turn_context: &TurnContext) {
        if !self.reasoning_open {
            sess.emit_progress_trace(
                turn_context,
                ProgressTraceCategory::Reasoning,
                ProgressTraceState::Started,
                None,
                Some("stream"),
            )
            .await;
            self.reasoning_open = true;
        }
    }

    async fn on_reasoning_section_break(&mut self, sess: &Session, turn_context: &TurnContext) {
        if self.reasoning_open {
            sess.emit_progress_trace(
                turn_context,
                ProgressTraceCategory::Reasoning,
                ProgressTraceState::Completed,
                None,
                Some("stream"),
            )
            .await;
            self.reasoning_open = false;
        }
    }

    async fn finalize(&mut self, sess: &Session, turn_context: &TurnContext) {
        if self.reasoning_open {
            sess.emit_progress_trace(
                turn_context,
                ProgressTraceCategory::Reasoning,
                ProgressTraceState::Completed,
                None,
                Some("stream"),
            )
            .await;
            self.reasoning_open = false;
        }
        if self.prefill_open {
            sess.emit_progress_trace(
                turn_context,
                ProgressTraceCategory::Prefill,
                ProgressTraceState::Completed,
                None,
                Some("stream"),
            )
            .await;
            self.prefill_open = false;
        }
        if self.gen_open {
            sess.emit_progress_trace(
                turn_context,
                ProgressTraceCategory::Gen,
                ProgressTraceState::Completed,
                None,
                Some("stream"),
            )
            .await;
            self.gen_open = false;
        } else if self.pending_prefill && !self.saw_work_category {
            // Conversational turns with no work are categorized as generation.
            sess.emit_progress_trace(
                turn_context,
                ProgressTraceCategory::Gen,
                ProgressTraceState::Started,
                None,
                Some("stream"),
            )
            .await;
            sess.emit_progress_trace(
                turn_context,
                ProgressTraceCategory::Gen,
                ProgressTraceState::Completed,
                None,
                Some("stream"),
            )
            .await;
        }
        self.pending_prefill = false;
    }
}

impl ProposedPlanItemState {
    fn new(turn_id: &str) -> Self {
        Self {
            item_id: format!("{turn_id}-plan"),
            started: false,
            completed: false,
        }
    }

    async fn start(&mut self, sess: &Session, turn_context: &TurnContext) {
        if self.started || self.completed {
            return;
        }
        self.started = true;
        let item = TurnItem::Plan(PlanItem {
            id: self.item_id.clone(),
            text: String::new(),
        });
        sess.emit_turn_item_started(turn_context, &item).await;
    }

    async fn push_delta(&mut self, sess: &Session, turn_context: &TurnContext, delta: &str) {
        if self.completed {
            return;
        }
        if delta.is_empty() {
            return;
        }
        let event = PlanDeltaEvent {
            thread_id: sess.conversation_id.to_string(),
            turn_id: turn_context.sub_id.clone(),
            item_id: self.item_id.clone(),
            delta: delta.to_string(),
        };
        sess.send_event(turn_context, EventMsg::PlanDelta(event))
            .await;
    }

    async fn complete_with_text(
        &mut self,
        sess: &Session,
        turn_context: &TurnContext,
        text: String,
    ) {
        if self.completed || !self.started {
            return;
        }
        self.completed = true;
        let item = TurnItem::Plan(PlanItem {
            id: self.item_id.clone(),
            text,
        });
        sess.emit_turn_item_completed(turn_context, item).await;
    }
}

/// In plan mode we defer agent message starts until the parser emits non-plan
/// text. The parser buffers each line until it can rule out a tag prefix, so
/// plan-only outputs never show up as empty assistant messages.
async fn maybe_emit_pending_agent_message_start(
    sess: &Session,
    turn_context: &TurnContext,
    state: &mut PlanModeStreamState,
    item_id: &str,
) {
    if state.started_agent_message_items.contains(item_id) {
        return;
    }
    if let Some(item) = state.pending_agent_message_items.remove(item_id) {
        sess.emit_turn_item_started(turn_context, &item).await;
        state
            .started_agent_message_items
            .insert(item_id.to_string());
    }
}

/// Agent messages are text-only today; concatenate all text entries.
fn agent_message_text(item: &codex_protocol::items::AgentMessageItem) -> String {
    item.content
        .iter()
        .map(|entry| match entry {
            codex_protocol::items::AgentMessageContent::Text { text } => text.as_str(),
        })
        .collect()
}

/// Split the stream into normal assistant text vs. proposed plan content.
/// Normal text becomes AgentMessage deltas; plan content becomes PlanDelta +
/// TurnItem::Plan.
async fn handle_plan_segments(
    sess: &Session,
    turn_context: &TurnContext,
    state: &mut PlanModeStreamState,
    item_id: &str,
    segments: Vec<ProposedPlanSegment>,
) {
    for segment in segments {
        match segment {
            ProposedPlanSegment::Normal(delta) => {
                if delta.is_empty() {
                    continue;
                }
                let has_non_whitespace = delta.chars().any(|ch| !ch.is_whitespace());
                if !has_non_whitespace && !state.started_agent_message_items.contains(item_id) {
                    let entry = state
                        .leading_whitespace_by_item
                        .entry(item_id.to_string())
                        .or_default();
                    entry.push_str(&delta);
                    continue;
                }
                let delta = if !state.started_agent_message_items.contains(item_id) {
                    if let Some(prefix) = state.leading_whitespace_by_item.remove(item_id) {
                        format!("{prefix}{delta}")
                    } else {
                        delta
                    }
                } else {
                    delta
                };
                maybe_emit_pending_agent_message_start(sess, turn_context, state, item_id).await;

                let event = AgentMessageContentDeltaEvent {
                    thread_id: sess.conversation_id.to_string(),
                    turn_id: turn_context.sub_id.clone(),
                    item_id: item_id.to_string(),
                    delta,
                };
                sess.send_event(turn_context, EventMsg::AgentMessageContentDelta(event))
                    .await;
            }
            ProposedPlanSegment::ProposedPlanStart => {
                if !state.plan_item_state.completed {
                    state.plan_item_state.start(sess, turn_context).await;
                }
            }
            ProposedPlanSegment::ProposedPlanDelta(delta) => {
                if !state.plan_item_state.completed {
                    if !state.plan_item_state.started {
                        state.plan_item_state.start(sess, turn_context).await;
                    }
                    state
                        .plan_item_state
                        .push_delta(sess, turn_context, &delta)
                        .await;
                }
            }
            ProposedPlanSegment::ProposedPlanEnd => {}
        }
    }
}

/// Flush any buffered proposed-plan segments when a specific assistant message ends.
async fn flush_proposed_plan_segments_for_item(
    sess: &Session,
    turn_context: &TurnContext,
    state: &mut PlanModeStreamState,
    item_id: &str,
) {
    let Some(mut parser) = state.plan_parsers.take_assistant_parser(item_id) else {
        return;
    };
    let segments = parser.finish();
    if segments.is_empty() {
        return;
    }
    handle_plan_segments(sess, turn_context, state, item_id, segments).await;
}

/// Flush any remaining assistant plan parsers when the response completes.
async fn flush_proposed_plan_segments_all(
    sess: &Session,
    turn_context: &TurnContext,
    state: &mut PlanModeStreamState,
) {
    for (item_id, mut parser) in state.plan_parsers.drain_assistant_parsers() {
        let segments = parser.finish();
        if segments.is_empty() {
            continue;
        }
        handle_plan_segments(sess, turn_context, state, &item_id, segments).await;
    }
}

/// Emit completion for plan items by parsing the finalized assistant message.
async fn maybe_complete_plan_item_from_message(
    sess: &Session,
    turn_context: &TurnContext,
    state: &mut PlanModeStreamState,
    item: &ResponseItem,
) {
    if let ResponseItem::Message { role, content, .. } = item
        && role == "assistant"
    {
        let mut text = String::new();
        for entry in content {
            if let ContentItem::OutputText {
                annotations: None,
                logprobs: None,
                text: chunk,
                ..
            } = entry
            {
                text.push_str(chunk);
            }
        }
        if let Some(plan_text) = extract_proposed_plan_text(&text) {
            if !state.plan_item_state.started {
                state.plan_item_state.start(sess, turn_context).await;
            }
            state
                .plan_item_state
                .complete_with_text(sess, turn_context, plan_text)
                .await;
        }
    }
}

/// Emit a completed agent message in plan mode, respecting deferred starts.
async fn emit_agent_message_in_plan_mode(
    sess: &Session,
    turn_context: &TurnContext,
    agent_message: codex_protocol::items::AgentMessageItem,
    state: &mut PlanModeStreamState,
) {
    let agent_message_id = agent_message.id.clone();
    let text = agent_message_text(&agent_message);
    if text.trim().is_empty() {
        state.pending_agent_message_items.remove(&agent_message_id);
        state.started_agent_message_items.remove(&agent_message_id);
        return;
    }

    maybe_emit_pending_agent_message_start(sess, turn_context, state, &agent_message_id).await;

    if !state
        .started_agent_message_items
        .contains(&agent_message_id)
    {
        let start_item = state
            .pending_agent_message_items
            .remove(&agent_message_id)
            .unwrap_or_else(|| {
                TurnItem::AgentMessage(codex_protocol::items::AgentMessageItem {
                    id: agent_message_id.clone(),
                    content: Vec::new(),
                })
            });
        sess.emit_turn_item_started(turn_context, &start_item).await;
        state
            .started_agent_message_items
            .insert(agent_message_id.clone());
    }

    sess.emit_turn_item_completed(turn_context, TurnItem::AgentMessage(agent_message))
        .await;
    state.started_agent_message_items.remove(&agent_message_id);
}

/// Emit completion for a plan-mode turn item, handling agent messages specially.
async fn emit_turn_item_in_plan_mode(
    sess: &Session,
    turn_context: &TurnContext,
    turn_item: TurnItem,
    previously_active_item: Option<&TurnItem>,
    state: &mut PlanModeStreamState,
) {
    match turn_item {
        TurnItem::AgentMessage(agent_message) => {
            emit_agent_message_in_plan_mode(sess, turn_context, agent_message, state).await;
        }
        _ => {
            if previously_active_item.is_none() {
                sess.emit_turn_item_started(turn_context, &turn_item).await;
            }
            sess.emit_turn_item_completed(turn_context, turn_item).await;
        }
    }
}

/// Handle a completed assistant response item in plan mode, returning true if handled.
async fn handle_assistant_item_done_in_plan_mode(
    sess: &Session,
    turn_context: &TurnContext,
    item: &ResponseItem,
    state: &mut PlanModeStreamState,
    previously_active_item: Option<&TurnItem>,
    last_agent_message: &mut Option<String>,
) -> bool {
    if let ResponseItem::Message { role, .. } = item
        && role == "assistant"
    {
        maybe_complete_plan_item_from_message(sess, turn_context, state, item).await;

        if let Some(turn_item) = handle_non_tool_response_item(item, true).await {
            emit_turn_item_in_plan_mode(
                sess,
                turn_context,
                turn_item,
                previously_active_item,
                state,
            )
            .await;
        }

        sess.record_conversation_items(turn_context, std::slice::from_ref(item))
            .await;
        if let Some(agent_message) = last_assistant_message_from_item(item, true) {
            *last_agent_message = Some(agent_message);
        }
        return true;
    }
    false
}

async fn drain_in_flight(
    in_flight: &mut FuturesOrdered<BoxFuture<'static, CodexResult<ResponseInputItem>>>,
    sess: Arc<Session>,
    turn_context: Arc<TurnContext>,
) -> CodexResult<()> {
    while let Some(res) = in_flight.next().await {
        match res {
            Ok(response_input) => {
                sess.record_conversation_items(&turn_context, &[response_input.into()])
                    .await;
            }
            Err(err) => {
                error_or_panic(format!("in-flight tool future failed during drain: {err}"));
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
#[instrument(level = "trace",
    skip_all,
    fields(
        turn_id = %turn_context.sub_id,
        model = %turn_context.model_info.slug
    )
)]
async fn try_run_sampling_request(
    router: Arc<ToolRouter>,
    sess: Arc<Session>,
    turn_context: Arc<TurnContext>,
    turn_state: Option<Arc<Mutex<TurnState>>>,
    client_session: &mut ModelClientSession,
    turn_metadata_header: Option<&str>,
    turn_diff_tracker: SharedTurnDiffTracker,
    prompt: &Prompt,
    request_context_tokens: i64,
    input_item_count: usize,
    purpose: SamplingPurpose,
    cancellation_token: CancellationToken,
) -> CodexResult<SamplingRequestResult> {
    feedback_tags!(
        model = turn_context.model_info.slug.clone(),
        approval_policy = turn_context.approval_policy,
        sandbox_policy = turn_context.sandbox_policy,
        effort = turn_context.reasoning_effort,
        auth_mode = sess.services.auth_manager.auth_mode(),
        features = sess.features.enabled_features(),
    );

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
        .instrument(trace_span!("stream_request"))
        .or_cancel(&cancellation_token)
        .await??;

    let tool_execution_mode = purpose.tool_execution();
    let tool_runtime = ToolCallRuntime::new(
        Arc::clone(&router),
        Arc::clone(&sess),
        Arc::clone(&turn_context),
        Arc::clone(&turn_diff_tracker),
    )
    .with_execution_mode(tool_execution_mode);
    let mut queued_calls = Vec::new();
    let mut needs_follow_up = false;
    let mut last_agent_message: Option<String> = None;
    let mut active_item: Option<TurnItem> = None;
    let mut should_emit_turn_diff = false;
    let mut server_reasoning_included = false;
    let plan_mode = turn_context.collaboration_mode.mode == ModeKind::Plan
        && matches!(tool_execution_mode, ToolCallExecutionMode::Normal);
    let mut plan_mode_state = plan_mode.then(|| PlanModeStreamState::new(&turn_context.sub_id));
    let mut progress_trace_state = ProgressTraceStreamState::default();
    let receiving_span = trace_span!("receiving_stream");
    let outcome: CodexResult<SamplingRequestResult> = loop {
        let handle_responses = trace_span!(
            parent: &receiving_span,
            "handle_responses",
            otel.name = field::Empty,
            tool_name = field::Empty,
            from = field::Empty,
        );

        let event = match stream
            .next()
            .instrument(trace_span!(parent: &handle_responses, "receiving"))
            .or_cancel(&cancellation_token)
            .await
        {
            Ok(event) => event,
            Err(codex_async_utils::CancelErr::Cancelled) => break Err(CodexErr::TurnAborted),
        };

        let event = match event {
            Some(Ok(event)) => event,
            Some(Err(err)) => break Err(err),
            None => {
                break Err(CodexErr::Stream(
                    "stream closed before response.completed".into(),
                    None,
                ));
            }
        };

        turn_context
            .otel_manager
            .record_responses(&handle_responses, &event);

        match event {
            ResponseEvent::Created => {}
            ResponseEvent::OutputItemDone(item) => {
                let previously_active_item = active_item.take();
                if matches!(previously_active_item, Some(TurnItem::WebSearch(_))) {
                    progress_trace_state
                        .mark_work_seen(&sess, &turn_context)
                        .await;
                    sess.emit_progress_trace(
                        &turn_context,
                        ProgressTraceCategory::Network,
                        ProgressTraceState::Completed,
                        None,
                        Some("web_search"),
                    )
                    .await;
                }
                if let Some(state) = plan_mode_state.as_mut() {
                    if let Some(previous) = previously_active_item.as_ref() {
                        let item_id = previous.id();
                        if matches!(previous, TurnItem::AgentMessage(_)) {
                            flush_proposed_plan_segments_for_item(
                                &sess,
                                &turn_context,
                                state,
                                &item_id,
                            )
                            .await;
                        }
                    }
                    if handle_assistant_item_done_in_plan_mode(
                        &sess,
                        &turn_context,
                        &item,
                        state,
                        previously_active_item.as_ref(),
                        &mut last_agent_message,
                    )
                    .await
                    {
                        continue;
                    }
                }

                let mut ctx = HandleOutputCtx {
                    sess: sess.clone(),
                    turn_context: turn_context.clone(),
                    router: &router,
                    plan_mode,
                };

                let output_result =
                    match handle_output_item_done(&mut ctx, item, previously_active_item)
                        .instrument(handle_responses)
                        .await
                    {
                        Ok(result) => result,
                        Err(err) => break Err(err),
                    };
                if let Some(tool_call) = output_result.tool_call {
                    progress_trace_state
                        .mark_work_seen(&sess, &turn_context)
                        .await;
                    queued_calls.push(tool_call);
                }
                if let Some(agent_message) = output_result.last_agent_message {
                    last_agent_message = Some(agent_message);
                }
                if output_result.needs_follow_up {
                    needs_follow_up = true;
                }
            }
            ResponseEvent::OutputItemAdded(item) => {
                if let Some(turn_item) = handle_non_tool_response_item(&item, plan_mode).await {
                    if matches!(turn_item, TurnItem::WebSearch(_)) {
                        progress_trace_state
                            .mark_work_seen(&sess, &turn_context)
                            .await;
                        sess.emit_progress_trace(
                            &turn_context,
                            ProgressTraceCategory::Network,
                            ProgressTraceState::Started,
                            None,
                            Some("web_search"),
                        )
                        .await;
                    }
                    if let Some(state) = plan_mode_state.as_mut()
                        && matches!(turn_item, TurnItem::AgentMessage(_))
                    {
                        let item_id = turn_item.id();
                        state
                            .pending_agent_message_items
                            .insert(item_id, turn_item.clone());
                    } else {
                        sess.emit_turn_item_started(&turn_context, &turn_item).await;
                    }
                    active_item = Some(turn_item);
                }
            }
            ResponseEvent::ServerReasoningIncluded(included) => {
                server_reasoning_included = included;
            }
            ResponseEvent::ServerModel(server_model) => {
                if !turn_context
                    .server_model_warning_emitted
                    .load(Ordering::Relaxed)
                    && sess
                        .maybe_pause_on_server_model_mismatch(&turn_context, server_model)
                        .await
                {
                    turn_context
                        .server_model_warning_emitted
                        .store(true, Ordering::Relaxed);
                    break Err(CodexErr::TurnAborted);
                }
            }
            ResponseEvent::ModelVerifications(verifications) => {
                if !turn_context
                    .model_verification_emitted
                    .swap(true, Ordering::Relaxed)
                {
                    sess.emit_model_verification(&turn_context, verifications)
                        .await;
                }
            }
            ResponseEvent::ToolCallInputDelta { .. } => {}
            ResponseEvent::RateLimits(snapshot) => {
                // Update internal state with latest rate limits, but defer sending until
                // token usage is available to avoid duplicate TokenCount events.
                sess.update_rate_limits(&turn_context, snapshot).await;
            }
            ResponseEvent::ModelsEtag(etag) => {
                // Update internal state with latest models etag
                sess.services
                    .models_manager
                    .refresh_if_new_etag(etag, &turn_context.config)
                    .await;
            }
            ResponseEvent::Completed {
                response_id: _,
                token_usage,
                end_turn,
                ..
            } => {
                if purpose == SamplingPurpose::WorkNotes {
                    sess.advance_work_notes(WorkNotesCheckpoint::CompletedResponse)
                        .await?;
                } else if !sess.finish_ordinary_input().await {
                    return Err(CodexErr::Fatal(
                        "Could not persist ordinary input completion.".into(),
                    ));
                }
                progress_trace_state.finalize(&sess, &turn_context).await;
                if let Some(state) = plan_mode_state.as_mut() {
                    flush_proposed_plan_segments_all(&sess, &turn_context, state).await;
                }
                sess.update_token_usage_info_with_request_context(
                    &turn_context,
                    token_usage.as_ref(),
                    request_context_tokens,
                    server_reasoning_included,
                    input_item_count,
                )
                .await;
                should_emit_turn_diff = true;

                if let Some(false) = end_turn {
                    needs_follow_up = true;
                }
                if let Some(turn_state) = turn_state.as_ref() {
                    needs_follow_up |= sess.has_pending_input_for_turn_state(turn_state).await;
                }

                break Ok(SamplingRequestResult {
                    needs_follow_up,
                    last_agent_message,
                });
            }
            ResponseEvent::OutputTextDelta(delta) => {
                progress_trace_state
                    .on_text_delta(&sess, &turn_context)
                    .await;
                // In review child threads, suppress assistant text deltas; the
                // UI will show a selection popup from the final ReviewOutput.
                if let Some(active) = active_item.as_ref() {
                    let item_id = active.id();
                    if let Some(state) = plan_mode_state.as_mut()
                        && matches!(active, TurnItem::AgentMessage(_))
                    {
                        let segments = state
                            .plan_parsers
                            .assistant_parser_mut(&item_id)
                            .parse(&delta);
                        handle_plan_segments(&sess, &turn_context, state, &item_id, segments).await;
                    } else {
                        let event = AgentMessageContentDeltaEvent {
                            thread_id: sess.conversation_id.to_string(),
                            turn_id: turn_context.sub_id.clone(),
                            item_id,
                            delta,
                        };
                        sess.send_event(&turn_context, EventMsg::AgentMessageContentDelta(event))
                            .await;
                    }
                } else {
                    error_or_panic("OutputTextDelta without active item".to_string());
                }
            }
            ResponseEvent::ReasoningSummaryDelta {
                delta,
                summary_index,
            } => {
                progress_trace_state
                    .on_reasoning_delta(&sess, &turn_context)
                    .await;
                if let Some(active) = active_item.as_ref() {
                    let event = ReasoningContentDeltaEvent {
                        thread_id: sess.conversation_id.to_string(),
                        turn_id: turn_context.sub_id.clone(),
                        item_id: active.id(),
                        delta,
                        summary_index,
                    };
                    sess.send_event(&turn_context, EventMsg::ReasoningContentDelta(event))
                        .await;
                } else {
                    error_or_panic("ReasoningSummaryDelta without active item".to_string());
                }
            }
            ResponseEvent::ReasoningSummaryPartAdded { summary_index } => {
                progress_trace_state
                    .on_reasoning_section_break(&sess, &turn_context)
                    .await;
                if let Some(active) = active_item.as_ref() {
                    let event =
                        EventMsg::AgentReasoningSectionBreak(AgentReasoningSectionBreakEvent {
                            item_id: active.id(),
                            summary_index,
                        });
                    sess.send_event(&turn_context, event).await;
                } else {
                    error_or_panic("ReasoningSummaryPartAdded without active item".to_string());
                }
            }
            ResponseEvent::ReasoningContentDelta {
                delta,
                content_index,
            } => {
                progress_trace_state
                    .on_reasoning_delta(&sess, &turn_context)
                    .await;
                if let Some(active) = active_item.as_ref() {
                    let event = ReasoningRawContentDeltaEvent {
                        thread_id: sess.conversation_id.to_string(),
                        turn_id: turn_context.sub_id.clone(),
                        item_id: active.id(),
                        delta,
                        content_index,
                    };
                    sess.send_event(&turn_context, EventMsg::ReasoningRawContentDelta(event))
                        .await;
                } else {
                    error_or_panic("ReasoningRawContentDelta without active item".to_string());
                }
            }
        }
    };

    progress_trace_state.finalize(&sess, &turn_context).await;
    if outcome.is_ok() {
        let mut in_flight = queued_calls
            .into_iter()
            .map(|call| {
                let future: BoxFuture<'static, CodexResult<ResponseInputItem>> = Box::pin(
                    tool_runtime
                        .clone()
                        .handle_tool_call(call, cancellation_token.child_token()),
                );
                future
            })
            .collect::<FuturesOrdered<_>>();
        drain_in_flight(&mut in_flight, sess.clone(), turn_context.clone()).await?;
    } else {
        for call in queued_calls {
            let rejected = ToolCallRuntime::rejected_response(
                &call,
                "The response ended before the tool batch was accepted. This call was not executed.",
            );
            sess.record_conversation_items(&turn_context, &[rejected.into()])
                .await;
        }
    }

    if should_emit_turn_diff {
        let unified_diff = {
            let mut tracker = turn_diff_tracker.lock().await;
            tracker.get_unified_diff()
        };
        if let Ok(Some(unified_diff)) = unified_diff {
            let msg = EventMsg::TurnDiff(TurnDiffEvent { unified_diff });
            sess.clone().send_event(&turn_context, msg).await;
        }
    }

    outcome
}

pub(crate) fn get_last_assistant_message_from_turn(responses: &[ResponseItem]) -> Option<String> {
    responses.iter().rev().find_map(|item| {
        if let ResponseItem::Message { role, content, .. } = item {
            if role == "assistant" {
                content.iter().rev().find_map(|ci| {
                    if let ContentItem::OutputText { text, .. } = ci {
                        Some(text.clone())
                    } else {
                        None
                    }
                })
            } else {
                None
            }
        } else {
            None
        }
    })
}

#[cfg(test)]
mod tests {
    use super::AUTO_COMPACT_WORK_NOTES_REQUEST_TAG;
    use super::compaction_source_token_adjustment;
    use super::estimate_work_notes_prompt_tokens;
    use super::inject_pre_compact_work_notes_request;
    use super::should_compact_with_previous_model;
    use super::trim_pre_compact_work_notes_input_to_headroom;
    use super::unsampled_review_token_limit;
    use super::work_notes_input_token_target;
    use super::work_notes_request_item;
    use crate::protocol::TokenUsage;
    use crate::session::tests::make_session_and_context;
    use codex_protocol::models::BaseInstructions;
    use codex_protocol::models::ContentItem;
    use codex_protocol::models::FunctionCallOutputBody;
    use codex_protocol::models::FunctionCallOutputPayload;
    use codex_protocol::models::ResponseItem;
    use pretty_assertions::assert_eq;

    #[test]
    fn model_downshift_decision_requires_larger_previous_context_window() {
        assert!(should_compact_with_previous_model(
            "larger-model",
            "smaller-model",
            9_000,
            8_000,
            Some(128_000),
            Some(32_000),
        ));

        assert!(!should_compact_with_previous_model(
            "same-window-a",
            "same-window-b",
            9_000,
            8_000,
            Some(32_000),
            Some(32_000),
        ));
    }

    #[tokio::test]
    async fn unsampled_frame_respects_the_limiting_helper_including_its_own_suffix() {
        let (_, mut context) = make_session_and_context().await;
        let trigger =
            crate::context_manager::estimate_item_token_count(&ResponseItem::CompactionTrigger {});
        let marker =
            crate::context_manager::estimate_item_token_count(&work_notes_request_item(&context));
        context.model_info.context_window = Some(372_000);
        context.model_info.effective_context_window_percent = 98;
        assert_eq!(unsampled_review_token_limit(&context), 364_560 - trigger);

        context.model_info.context_window = Some(24_000);
        context.model_info.effective_context_window_percent = 95;
        assert_eq!(unsampled_review_token_limit(&context), 22_800 - marker);
        context.final_output_json_schema = Some(serde_json::json!({"type": "object"}));
        assert_eq!(unsampled_review_token_limit(&context), 22_800 - trigger);
    }

    #[test]
    fn model_downshift_decision_requires_different_model_and_over_limit_history() {
        assert!(!should_compact_with_previous_model(
            "same-model",
            "same-model",
            9_000,
            8_000,
            Some(128_000),
            Some(32_000),
        ));

        assert!(!should_compact_with_previous_model(
            "larger-model",
            "smaller-model",
            7_000,
            8_000,
            Some(128_000),
            Some(32_000),
        ));
    }

    #[tokio::test]
    async fn work_notes_recovery_only_calibrates_from_pre_capture_usage() {
        let (sess, context) = make_session_and_context().await;
        let work: ResponseItem = codex_protocol::models::DeveloperInstructions::new("work").into();
        sess.record_conversation_items(&context, &[work]).await;
        let history = sess.clone_history().await;
        let start = history.raw_items().len();
        let base = sess.get_base_instructions_for_turn(&context).await;
        let local = history
            .estimate_token_count_with_base_instructions(&base)
            .unwrap();
        let usage = TokenUsage {
            input_tokens: 9_000,
            total_tokens: 9_000,
            ..Default::default()
        };
        sess.update_token_usage_info_with_request_context(
            &context,
            Some(&usage),
            2_000,
            true,
            start,
        )
        .await;
        inject_pre_compact_work_notes_request(&sess, &context).await;
        assert_eq!(
            compaction_source_token_adjustment(&sess, &context).await,
            7_000 - local
        );

        // Usage now covers a possibly trimmed capture. Keep the preceding work measurement.
        sess.update_token_usage_info_with_request_context(
            &context,
            Some(&usage),
            0,
            true,
            start + 1,
        )
        .await;
        assert_eq!(
            compaction_source_token_adjustment(&sess, &context).await,
            7_000 - local
        );
    }

    #[test]
    fn work_notes_prompt_trimming_reserves_output_headroom() {
        let context_window = 4_000;
        let target = work_notes_input_token_target(context_window).expect("target");
        let large_output = "tool output line\n".repeat(8_000);
        let mut input = vec![
            ResponseItem::Message {
                encrypted_content: None,
                status: None,
                id: None,
                role: "user".to_string(),
                content: vec![ContentItem::InputText {
                    text: "initial user request".to_string(),
                }],
                end_turn: None,
                phase: None,
            },
            ResponseItem::FunctionCall {
                encrypted_content: None,
                status: None,
                id: None,
                name: "shell".to_string(),
                arguments: "{}".to_string(),
                call_id: "call-1".to_string(),
            },
            ResponseItem::FunctionCallOutput {
                call_id: "call-1".to_string(),
                output: FunctionCallOutputPayload {
                    body: FunctionCallOutputBody::Text(large_output),
                    ..Default::default()
                },
            },
            ResponseItem::Message {
                encrypted_content: None,
                status: None,
                id: None,
                role: "developer".to_string(),
                content: vec![ContentItem::InputText {
                    text: format!("{AUTO_COMPACT_WORK_NOTES_REQUEST_TAG}\nrequest notes"),
                }],
                end_turn: None,
                phase: None,
            },
        ];
        let base_instructions = BaseInstructions {
            text: "base instructions".to_string(),
        };
        let before = estimate_work_notes_prompt_tokens(&input, &base_instructions);
        assert!(
            before > target,
            "fixture must exceed the target input budget before trimming"
        );

        let adjustment = target + 500 - before;
        let stats = trim_pre_compact_work_notes_input_to_headroom(
            &mut input,
            &base_instructions,
            Some(context_window),
            adjustment,
        )
        .expect("expected trimming");

        assert_eq!(stats.estimated_tokens_before, target + 500);
        assert_eq!(
            stats.estimated_tokens_after,
            estimate_work_notes_prompt_tokens(&input, &base_instructions) + adjustment
        );
        assert!(
            stats.estimated_tokens_after <= target,
            "after={} target={} before={}",
            stats.estimated_tokens_after,
            target,
            before
        );
        assert_eq!(
            input.last(),
            Some(&ResponseItem::Message {
                encrypted_content: None,
                status: None,
                id: None,
                role: "developer".to_string(),
                content: vec![ContentItem::InputText {
                    text: format!("{AUTO_COMPACT_WORK_NOTES_REQUEST_TAG}\nrequest notes"),
                }],
                end_turn: None,
                phase: None,
            })
        );
        let ResponseItem::FunctionCallOutput { output, .. } = &input[2] else {
            panic!("expected function call output");
        };
        let FunctionCallOutputBody::Text(text) = &output.body else {
            panic!("expected text output");
        };
        assert!(text.contains("tokens truncated"));
    }

    #[test]
    fn work_notes_prompt_trimming_counts_base_instructions() {
        let context_window = 1_000;
        let target = work_notes_input_token_target(context_window).expect("target");
        let empty_base = BaseInstructions {
            text: String::new(),
        };
        let base_instructions = BaseInstructions {
            text: "base instructions line\n".repeat(40),
        };
        let mut input = vec![
            ResponseItem::FunctionCallOutput {
                call_id: "call-1".to_string(),
                output: FunctionCallOutputPayload {
                    body: FunctionCallOutputBody::Text("tool output line\n".repeat(130)),
                    ..Default::default()
                },
            },
            ResponseItem::Message {
                encrypted_content: None,
                status: None,
                id: None,
                role: "developer".to_string(),
                content: vec![ContentItem::InputText {
                    text: format!("{AUTO_COMPACT_WORK_NOTES_REQUEST_TAG}\nrequest notes"),
                }],
                end_turn: None,
                phase: None,
            },
        ];
        let input_only = estimate_work_notes_prompt_tokens(&input, &empty_base);
        assert!(
            input_only <= target,
            "fixture input_only={input_only} target={target}"
        );
        let before = estimate_work_notes_prompt_tokens(&input, &base_instructions);
        assert!(
            before > target,
            "fixture with base instructions must exceed the target input budget"
        );

        let stats = trim_pre_compact_work_notes_input_to_headroom(
            &mut input,
            &base_instructions,
            Some(context_window),
            0,
        )
        .expect("expected trimming");

        assert_eq!(stats.estimated_tokens_before, before);
        assert_eq!(stats.target_input_tokens, target);
        assert!(
            stats.estimated_tokens_after <= target,
            "after={} target={} before={}",
            stats.estimated_tokens_after,
            target,
            before
        );
    }
}
