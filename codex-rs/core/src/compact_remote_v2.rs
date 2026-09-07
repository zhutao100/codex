//! Remote compaction over the ordinary Responses transport.

use std::sync::Arc;
use std::time::Instant;

use crate::Prompt;
use crate::client::ModelClientSession;
use crate::client_common::ResponseEvent;
use crate::client_common::ResponseStream;
use crate::compact::InitialContextInjection;
use crate::compact::insert_initial_context_before_last_real_user_message;
use crate::compact::prepare_history_for_compaction;
use crate::compact::preserved_work_notes_message;
use crate::compact_remote::trim_function_call_history_to_fit_context_window;
use crate::error::CodexErr;
use crate::error::Result as CodexResult;
use crate::protocol::CompactedItem;
use crate::protocol::EventMsg;
use crate::protocol::TurnStartedEvent;
use crate::session::session::Session;
use crate::session::turn::compaction_tool_specs;
use crate::session::turn_context::TurnContext;
use crate::truncate::approx_token_count;
use crate::util::backoff;
use codex_protocol::items::ContextCompactionItem;
use codex_protocol::items::TurnItem;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use futures::StreamExt;
use serde_json::json;
use tokio_util::sync::CancellationToken;
use tracing::info;

#[path = "compact_remote_v2_history.rs"]
mod history;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CompactionReason {
    UserRequested,
    TokenLimit,
    ModelDownshift {
        target_window: Option<i64>,
        target_auto_limit: Option<i64>,
    },
}

pub(crate) async fn run_remote_compact_task(
    sess: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
    client_session: Option<&mut ModelClientSession>,
    notes: Option<String>,
    injection: InitialContextInjection,
    reason: CompactionReason,
    cancellation_token: &CancellationToken,
) -> CodexResult<bool> {
    if reason == CompactionReason::UserRequested {
        sess.send_event(
            turn_context,
            EventMsg::TurnStarted(TurnStartedEvent {
                model_context_window: turn_context.model_context_window(),
                collaboration_mode_kind: turn_context.collaboration_mode.mode,
            }),
        )
        .await;
    }
    let mut owned_session;
    let client_session = match client_session {
        Some(session) => session,
        None => {
            owned_session = sess
                .services
                .model_client
                .new_session_with_provider(turn_context.provider.clone());
            &mut owned_session
        }
    };
    let result = run_compact(
        sess,
        turn_context,
        client_session,
        notes,
        injection,
        reason,
        cancellation_token,
    )
    .await;
    if let Err(err) = &result
        && !matches!(err, CodexErr::TurnAborted | CodexErr::Interrupted)
    {
        sess.send_event(
            turn_context,
            EventMsg::Error(err.to_error_event(Some("Error running remote compact task".into()))),
        )
        .await;
    }
    result.map(|()| true)
}

async fn run_compact(
    sess: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
    client_session: &mut ModelClientSession,
    notes: Option<String>,
    injection: InitialContextInjection,
    reason: CompactionReason,
    cancellation_token: &CancellationToken,
) -> CodexResult<()> {
    check_cancelled(cancellation_token)?;
    let started = Instant::now();
    let lifecycle_item = TurnItem::ContextCompaction(ContextCompactionItem::new());
    sess.emit_turn_item_started(turn_context, &lifecycle_item)
        .await;
    let prepared = prepare_history_for_compaction(sess.clone_history().await, notes)?;
    let mut source = prepared.source_history;
    let base_instructions = sess.get_base_instructions().await;
    let (rewritten_outputs, estimated_deleted_tokens) =
        trim_function_call_history_to_fit_context_window(
            &mut source,
            turn_context,
            &base_instructions,
        );
    let mut input = source.for_prompt_with_modalities(&turn_context.model_info.input_modalities);
    let tools = match &client_session.sampling_tools {
        Some((model, tools)) if model == &turn_context.model_info.slug => tools.clone(),
        _ => compaction_tool_specs(sess, turn_context, &input, cancellation_token).await?,
    };
    input.push(ResponseItem::CompactionTrigger {});
    let mut prompt = Prompt {
        input,
        tools,
        parallel_tool_calls: turn_context.model_info.supports_parallel_tool_calls,
        base_instructions,
        personality: turn_context.personality,
        output_schema: None,
    };
    let metadata = compaction_metadata(
        turn_context.resolve_turn_metadata_header().await.as_deref(),
        reason,
        injection,
    );
    let output = request_compaction(
        sess,
        turn_context,
        client_session,
        &prompt,
        &metadata,
        cancellation_token,
    )
    .await?;
    prompt.input.pop();

    // Reserve canonical context even for standalone/pre-turn compaction: the
    // next normal request seeds it. Count notes and tools outside retention.
    let initial_context = sess.build_initial_context(turn_context).await;
    let notes_item = prepared
        .preserved_work_notes
        .as_deref()
        .map(preserved_work_notes_message);
    let fixed_tokens = initial_context
        .iter()
        .chain(std::iter::once(&output.item))
        .chain(notes_item.iter())
        .map(history::retained_tokens)
        .sum::<usize>()
        .saturating_add(approx_token_count(&prompt.base_instructions.text))
        .saturating_add(approx_token_count(&serde_json::to_string(&prompt.tools)?));
    let (window, auto_limit) = match reason {
        CompactionReason::ModelDownshift {
            target_window,
            target_auto_limit,
        } => (target_window, target_auto_limit),
        CompactionReason::UserRequested | CompactionReason::TokenLimit => (
            turn_context.model_context_window(),
            turn_context.model_info.auto_compact_token_limit(),
        ),
    };
    let budget = retention_budget(fixed_tokens, window, auto_limit)?;
    let mut replacement = history::retain_messages(&prompt.input, budget);
    let retained_tokens = replacement
        .iter()
        .map(history::retained_tokens)
        .sum::<usize>();
    let retained_images = replacement
        .iter()
        .map(|item| match item {
            ResponseItem::Message { content, .. } => content
                .iter()
                .filter(|item| matches!(item, ContentItem::InputImage { .. }))
                .count(),
            _ => 0,
        })
        .sum::<usize>();
    replacement.push(output.item);
    let reference = match injection {
        InitialContextInjection::DoNotInject => None,
        InitialContextInjection::BeforeLastUserMessage => {
            insert_initial_context_before_last_real_user_message(&mut replacement, initial_context);
            Some(turn_context.to_turn_context_item())
        }
    };
    replacement.extend(notes_item);
    check_cancelled(cancellation_token)?;

    install_compacted_history(
        sess,
        turn_context,
        replacement,
        reference,
        lifecycle_item,
        cancellation_token,
    )
    .await?;
    client_session.reset_after_compaction();
    info!(
        turn_id = %turn_context.sub_id,
        response_id = %output.response_id,
        retained_tokens,
        retained_images,
        full_context_tokens = fixed_tokens.saturating_add(retained_tokens),
        rewritten_outputs,
        estimated_deleted_tokens,
        elapsed_ms = started.elapsed().as_millis(),
        "installed remote compaction v2"
    );
    Ok(())
}

async fn install_compacted_history(
    sess: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
    mut replacement: Vec<ResponseItem>,
    reference: Option<crate::protocol::TurnContextItem>,
    lifecycle_item: TurnItem,
    cancellation_token: &CancellationToken,
) -> CodexResult<()> {
    // Acquire before spawning so task-stop joins this boundary even if its
    // grace timeout aborts the parent task while rollout persistence is blocked.
    let guard = Arc::clone(&sess.compaction_installation).lock_owned().await;
    check_cancelled(cancellation_token)?;
    let install_session = Arc::clone(sess);
    let install_context = Arc::clone(turn_context);
    let install = tokio::spawn(async move {
        let _guard = guard;
        // Snapshot capture shares this guard. A snapshot that completed while
        // the model was compacting must be included in the exact checkpoint;
        // one completing later appends after that checkpoint.
        replacement.extend(
            install_session
                .clone_history()
                .await
                .raw_items()
                .iter()
                .filter(|item| matches!(item, ResponseItem::GhostSnapshot { .. }))
                .cloned(),
        );
        let compacted = CompactedItem {
            message: String::new(),
            replacement_history: Some(replacement.clone()),
        };
        install_session
            .replace_compacted_history(&install_context, replacement, reference, compacted)
            .await;
        install_session
            .emit_turn_item_completed(&install_context, lifecycle_item)
            .await;
    });
    install.await?;
    Ok(())
}

fn retention_budget(
    fixed_tokens: usize,
    window: Option<i64>,
    auto_limit: Option<i64>,
) -> CodexResult<usize> {
    let target = window
        .filter(|limit| *limit > 0)
        .into_iter()
        .chain(auto_limit.filter(|limit| *limit > 0).map(|limit| limit - 1))
        .min()
        .map(|limit| usize::try_from(limit).unwrap_or(usize::MAX));
    if let Some(target) = target {
        if fixed_tokens > target {
            return Err(CodexErr::Fatal(format!(
                "Compaction summary, notes, instructions and tools need about {fixed_tokens} tokens, exceeding the {target}-token context target; use a larger context window"
            )));
        }
        return Ok(history::RETAINED_MESSAGE_TOKEN_BUDGET.min(target - fixed_tokens));
    }
    Ok(history::RETAINED_MESSAGE_TOKEN_BUDGET)
}

fn check_cancelled(token: &CancellationToken) -> CodexResult<()> {
    if token.is_cancelled() {
        Err(CodexErr::TurnAborted)
    } else {
        Ok(())
    }
}

fn compaction_metadata(
    base: Option<&str>,
    reason: CompactionReason,
    injection: InitialContextInjection,
) -> String {
    let mut metadata = base
        .and_then(|base| {
            serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(base).ok()
        })
        .unwrap_or_default();
    let manual = reason == CompactionReason::UserRequested;
    metadata.insert("request_kind".into(), json!("compaction"));
    metadata.insert(
        "compaction".into(),
        json!({
            "trigger": if manual { "manual" } else { "auto" },
            "reason": match reason {
                CompactionReason::UserRequested => "user_requested",
                CompactionReason::TokenLimit => "context_limit",
                CompactionReason::ModelDownshift { .. } => "model_downshift",
            },
            "implementation": "responses_compaction_v2",
            "phase": if manual { "standalone_turn" } else {
                match injection {
                    InitialContextInjection::DoNotInject => "pre_turn",
                    InitialContextInjection::BeforeLastUserMessage => "mid_turn",
                }
            },
            "strategy": "memento"
        }),
    );
    serde_json::Value::Object(metadata).to_string()
}

fn retryable(error: &CodexErr) -> bool {
    !error
        .http_status_code_value()
        .is_some_and(|status| (400..500).contains(&status) && !matches!(status, 408 | 429))
        && error.is_retryable()
}

async fn request_compaction(
    sess: &Session,
    context: &TurnContext,
    client: &mut ModelClientSession,
    prompt: &Prompt,
    metadata: &str,
    token: &CancellationToken,
) -> CodexResult<CompactionOutput> {
    let max_retries = context.provider.stream_max_retries().min(2);
    let mut retries = 0;
    loop {
        let attempt = async {
            let stream = client
                .stream(
                    prompt,
                    &context.model_info,
                    &context.otel_manager,
                    context.reasoning_effort,
                    context.reasoning_summary,
                    context.service_tier.clone(),
                    Some(metadata),
                )
                .await?;
            collect_compaction_output(sess, context, stream).await
        };
        let error = tokio::select! {
            biased;
            _ = token.cancelled() => return Err(CodexErr::TurnAborted),
            result = attempt => match result {
                Ok(output) => return Ok(output),
                Err(error) => error,
            }
        };
        if !retryable(&error) {
            return Err(error);
        }
        if retries >= max_retries && client.try_switch_fallback_transport(&context.otel_manager) {
            retries = 0;
            continue;
        }
        if retries >= max_retries {
            return Err(error);
        }
        retries += 1;
        let delay = match &error {
            CodexErr::Stream(_, delay) => delay.unwrap_or_else(|| backoff(retries)),
            _ => backoff(retries),
        };
        context
            .otel_manager
            .counter("codex.compaction.retry", 1, &[("type", "remote_v2")]);
        sess.notify_stream_error(
            context,
            format!("Retrying compaction... {retries}/{max_retries}"),
            error,
        )
        .await;
        tokio::select! {
            biased;
            _ = token.cancelled() => return Err(CodexErr::TurnAborted),
            _ = tokio::time::sleep(delay) => {}
        }
    }
}

#[derive(Debug, PartialEq)]
struct CompactionOutput {
    item: ResponseItem,
    response_id: String,
}

async fn collect_compaction_output(
    sess: &Session,
    context: &TurnContext,
    mut stream: ResponseStream,
) -> CodexResult<CompactionOutput> {
    let mut candidate = None;
    let mut count = 0;
    while let Some(event) = stream.next().await {
        match event? {
            ResponseEvent::OutputItemDone(item @ ResponseItem::Compaction { .. }) => {
                count += 1;
                if candidate.is_none() {
                    candidate = Some(item);
                }
            }
            ResponseEvent::Completed {
                response_id,
                token_usage,
                ..
            } => {
                sess.update_token_usage_info(context, token_usage.as_ref())
                    .await;
                if count != 1 {
                    return Err(CodexErr::Fatal(format!(
                        "Remote compaction v2 expected exactly one compaction output, got {count}"
                    )));
                }
                let Some(item) = candidate else {
                    unreachable!("exactly one completed compaction item");
                };
                return Ok(CompactionOutput { item, response_id });
            }
            ResponseEvent::RateLimits(snapshot) => sess.update_rate_limits(context, snapshot).await,
            ResponseEvent::ServerReasoningIncluded(included) => {
                sess.set_server_reasoning_included(included).await
            }
            _ => {}
        }
    }
    Err(CodexErr::Stream(
        "Remote compaction v2 stream closed before response.completed".into(),
        None,
    ))
}

#[cfg(test)]
#[path = "compact_remote_v2_tests.rs"]
mod tests;
