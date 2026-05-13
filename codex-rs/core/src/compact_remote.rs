use std::sync::Arc;

use crate::Prompt;
use crate::compact::InitialContextInjection;
use crate::compact::PreparedCompactionInput;
use crate::compact::insert_initial_context_before_last_real_user_message;
use crate::compact::prepare_history_for_compaction;
use crate::compact::preserved_work_notes_message;
use crate::context_manager::ContextManager;
use crate::context_manager::is_codex_generated_item;
use crate::error::Result as CodexResult;
use crate::protocol::CompactedItem;
use crate::protocol::EventMsg;
use crate::protocol::TurnStartedEvent;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use codex_protocol::items::ContextCompactionItem;
use codex_protocol::items::TurnItem;
use codex_protocol::models::BaseInstructions;
use codex_protocol::models::ResponseItem;
use tracing::info;

pub(crate) async fn run_inline_remote_auto_compact_task(
    sess: Arc<Session>,
    turn_context: Arc<TurnContext>,
    preserved_work_notes: Option<String>,
    initial_context_injection: InitialContextInjection,
) -> CodexResult<bool> {
    run_remote_compact_task_inner(
        &sess,
        &turn_context,
        preserved_work_notes,
        initial_context_injection,
    )
    .await?;
    Ok(true)
}

pub(crate) async fn run_remote_compact_task(
    sess: Arc<Session>,
    turn_context: Arc<TurnContext>,
) -> CodexResult<()> {
    let start_event = EventMsg::TurnStarted(TurnStartedEvent {
        model_context_window: turn_context.model_context_window(),
        collaboration_mode_kind: turn_context.collaboration_mode.mode,
    });
    sess.send_event(&turn_context, start_event).await;

    run_remote_compact_task_inner(
        &sess,
        &turn_context,
        None,
        InitialContextInjection::DoNotInject,
    )
    .await
}

async fn run_remote_compact_task_inner(
    sess: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
    preserved_work_notes: Option<String>,
    initial_context_injection: InitialContextInjection,
) -> CodexResult<()> {
    if let Err(err) = run_remote_compact_task_inner_impl(
        sess,
        turn_context,
        preserved_work_notes,
        initial_context_injection,
    )
    .await
    {
        let event = EventMsg::Error(
            err.to_error_event(Some("Error running remote compact task".to_string())),
        );
        sess.send_event(turn_context, event).await;
        return Err(err);
    }
    Ok(())
}

async fn run_remote_compact_task_inner_impl(
    sess: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
    preserved_work_notes: Option<String>,
    initial_context_injection: InitialContextInjection,
) -> CodexResult<()> {
    let compaction_item = TurnItem::ContextCompaction(ContextCompactionItem::new());
    sess.emit_turn_item_started(turn_context, &compaction_item)
        .await;
    let base_instructions = sess.get_base_instructions().await;
    let PreparedCompactionInput {
        mut source_history,
        preserved_work_notes,
    } = prepare_history_for_compaction(sess.clone_history().await, preserved_work_notes);
    let deleted_items = trim_function_call_history_to_fit_context_window(
        &mut source_history,
        turn_context.as_ref(),
        &base_instructions,
    );
    if deleted_items > 0 {
        info!(
            turn_id = %turn_context.sub_id,
            deleted_items,
            "trimmed history items before remote compaction"
        );
    }

    // Required to keep `/undo` available after compaction
    let ghost_snapshots: Vec<ResponseItem> = source_history
        .raw_items()
        .iter()
        .filter(|item| matches!(item, ResponseItem::GhostSnapshot { .. }))
        .cloned()
        .collect();

    let prompt = Prompt {
        input: source_history.for_prompt_with_modalities(&turn_context.model_info.input_modalities),
        tools: vec![],
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

    let reference_context_item = match initial_context_injection {
        InitialContextInjection::DoNotInject => None,
        InitialContextInjection::BeforeLastUserMessage => {
            let initial_context = sess.build_initial_context(turn_context.as_ref()).await;
            insert_initial_context_before_last_real_user_message(&mut new_history, initial_context);
            Some(turn_context.to_turn_context_item())
        }
    };
    if let Some(notes) = preserved_work_notes.as_ref() {
        new_history.push(preserved_work_notes_message(notes));
    }
    if !ghost_snapshots.is_empty() {
        new_history.extend(ghost_snapshots);
    }
    let compacted_item = CompactedItem {
        message: String::new(),
        replacement_history: Some(new_history.clone()),
    };
    sess.replace_compacted_history(
        turn_context.as_ref(),
        new_history,
        reference_context_item,
        compacted_item,
    )
    .await;

    sess.emit_turn_item_completed(turn_context, compaction_item)
        .await;
    Ok(())
}

fn trim_function_call_history_to_fit_context_window(
    history: &mut ContextManager,
    turn_context: &TurnContext,
    base_instructions: &BaseInstructions,
) -> usize {
    let mut deleted_items = 0usize;
    let Some(context_window) = turn_context.model_context_window() else {
        return deleted_items;
    };

    while history
        .estimate_token_count_with_base_instructions(base_instructions)
        .is_some_and(|estimated_tokens| estimated_tokens > context_window)
    {
        let Some(last_item) = history.raw_items().last() else {
            break;
        };
        if !is_codex_generated_item(last_item) {
            break;
        }
        if !history.remove_last_item() {
            break;
        }
        deleted_items += 1;
    }

    deleted_items
}
