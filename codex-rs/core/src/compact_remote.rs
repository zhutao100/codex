use std::sync::Arc;

use crate::Prompt;
use crate::compact::CompactionContext;
use crate::compact::InitialContextInjection;
use crate::compact::PreparedCompactionInput;
use crate::compact::insert_initial_context_before_last_real_user_message;
use crate::compact::is_preserved_work_notes_item;
use crate::compact::preserved_work_notes_message;
use crate::context_manager::estimate_item_token_count;
use crate::error::Result as CodexResult;
use crate::protocol::EventMsg;
use crate::protocol::TurnStartedEvent;
use crate::session::session::Session;
use crate::truncate::approx_token_count;
use codex_protocol::items::ContextCompactionItem;
use codex_protocol::items::TurnItem;
use codex_protocol::models::BaseInstructions;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::ResponseItem;
use tracing::info;

pub(crate) async fn run_inline_remote_auto_compact_task(
    sess: Arc<Session>,
    context: &CompactionContext<'_>,
    preserved_work_notes: Option<String>,
    initial_context_injection: InitialContextInjection,
) -> CodexResult<bool> {
    run_remote_compact_task_inner(
        &sess,
        context,
        preserved_work_notes,
        initial_context_injection,
    )
    .await?;
    Ok(true)
}

pub(crate) async fn run_remote_compact_task(
    sess: Arc<Session>,
    context: &CompactionContext<'_>,
) -> CodexResult<()> {
    let turn_context = context.request;
    let start_event = EventMsg::TurnStarted(TurnStartedEvent {
        model_context_window: turn_context.model_context_window(),
        collaboration_mode_kind: turn_context.collaboration_mode.mode,
    });
    sess.send_event(turn_context, start_event).await;

    run_remote_compact_task_inner(&sess, context, None, InitialContextInjection::DoNotInject).await
}

async fn run_remote_compact_task_inner(
    sess: &Arc<Session>,
    context: &CompactionContext<'_>,
    preserved_work_notes: Option<String>,
    initial_context_injection: InitialContextInjection,
) -> CodexResult<()> {
    if let Err(err) = run_remote_compact_task_inner_impl(
        sess,
        context,
        preserved_work_notes,
        initial_context_injection,
    )
    .await
    {
        let event = EventMsg::Error(
            err.to_error_event(Some("Error running remote compact task".to_string())),
        );
        sess.send_event(context.request, event).await;
        return Err(err);
    }
    Ok(())
}

async fn run_remote_compact_task_inner_impl(
    sess: &Arc<Session>,
    context: &CompactionContext<'_>,
    preserved_work_notes: Option<String>,
    initial_context_injection: InitialContextInjection,
) -> CodexResult<()> {
    let turn_context = context.request;
    let compaction_item = TurnItem::ContextCompaction(ContextCompactionItem::new());
    sess.emit_turn_item_started(turn_context, &compaction_item)
        .await;
    let base_instructions = sess.get_base_instructions_for_turn(turn_context).await;
    let PreparedCompactionInput {
        source_history,
        preserved_work_notes,
    } = sess
        .prepare_history_for_compaction(preserved_work_notes, &context.review_frames)
        .await?;
    // Ghost snapshots are local history, not model input.
    let ghost_snapshots: Vec<ResponseItem> = source_history
        .raw_items()
        .iter()
        .filter(|item| matches!(item, ResponseItem::GhostSnapshot { .. }))
        .cloned()
        .collect();
    let mut input =
        source_history.for_prompt_with_modalities(&turn_context.model_info.input_modalities);
    let (rewritten_outputs, estimated_deleted_tokens) =
        trim_function_call_history_to_fit_context_window(
            &mut input,
            turn_context.model_context_window(),
            &base_instructions,
            // The unary endpoint does not use the ordinary sampling request surface.
            0,
        );
    if rewritten_outputs > 0 {
        info!(
            turn_id = %turn_context.sub_id,
            rewritten_outputs,
            estimated_deleted_tokens,
            "rewrote history outputs before remote compaction"
        );
    }

    let prompt = Prompt {
        input,
        verbosity: Some(turn_context.verbosity),
        sampled_controls: turn_context.sampled_controls.clone(),
        helper_fit: None,
        review_admission: None,
        tools: Default::default(),
        parallel_tool_calls: false,
        base_instructions,
        personality: turn_context.personality,
        output_schema: None,
    };

    let mut new_history = sess
        .services
        .model_client
        .compact_conversation_history_with_provider(
            &turn_context.provider,
            &prompt,
            &turn_context.model_info,
            &turn_context.otel_manager,
        )
        .await?;

    // The unary endpoint can echo context with new IDs. The host owns its
    // installation, either here or on the first ordinary admission.
    let canonical = sess.build_initial_context(context.replacement).await;
    let source_canonical = if Arc::ptr_eq(context.request, context.replacement) {
        Vec::new()
    } else {
        sess.build_initial_context(context.request).await
    };
    new_history.retain(|item| {
        !is_preserved_work_notes_item(item) && !context.review_frames.matches_message(item)
            && !canonical.iter().chain(&source_canonical).any(|known| matches!(
                (item, known),
                (ResponseItem::Message { role, content, .. }, ResponseItem::Message { role: old_role, content: old_content, .. })
                if role == old_role && content == old_content
            ))
    });
    let reference_context_item = match initial_context_injection {
        InitialContextInjection::DoNotInject => None,
        InitialContextInjection::BeforeLastUserMessage => {
            insert_initial_context_before_last_real_user_message(&mut new_history, canonical);
            Some(context.replacement.to_turn_context_item())
        }
    };
    if let Some(notes) = preserved_work_notes.as_ref() {
        new_history.push(preserved_work_notes_message(notes));
    }
    if !ghost_snapshots.is_empty() {
        new_history.extend(ghost_snapshots);
    }
    sess.replace_compacted_history(
        context.replacement,
        new_history,
        reference_context_item,
        String::new(),
    )
    .await;

    sess.emit_turn_item_completed(turn_context, compaction_item)
        .await;
    Ok(())
}

pub(crate) const TRUNCATED_OUTPUT: &str =
    "Output exceeded the available model context and was truncated";

pub(crate) fn trim_function_call_history_to_fit_context_window(
    items: &mut [ResponseItem],
    source_token_limit: Option<i64>,
    base_instructions: &BaseInstructions,
    source_token_adjustment: i64,
) -> (usize, i64) {
    let Some(context_window) = source_token_limit else {
        return (0, 0);
    };
    // Keep the accumulator unclamped while subtracting rewritten outputs.
    let mut tokens = items
        .iter()
        .map(estimate_item_token_count)
        .map(i128::from)
        .sum::<i128>()
        + approx_token_count(&base_instructions.text) as i128
        + i128::from(source_token_adjustment);
    let original_tokens = tokens;
    let mut rewritten_outputs = 0;
    for item in items.iter_mut().rev() {
        if tokens <= i128::from(context_window) {
            break;
        }
        if matches!(item, ResponseItem::GhostSnapshot { .. }) {
            continue;
        }
        let before = estimate_item_token_count(item);
        let mut rewritten = item.clone();
        match &mut rewritten {
            ResponseItem::FunctionCallOutput { output, .. } => {
                output.body = FunctionCallOutputBody::Text(TRUNCATED_OUTPUT.into());
            }
            ResponseItem::CustomToolCallOutput { output, .. } => {
                *output = TRUNCATED_OUTPUT.into();
            }
            _ => break,
        }
        let after = estimate_item_token_count(&rewritten);
        if after < before {
            *item = rewritten;
            tokens -= i128::from(before - after);
            rewritten_outputs += 1;
        }
    }
    (
        rewritten_outputs,
        i64::try_from(original_tokens - tokens).unwrap_or(i64::MAX),
    )
}
